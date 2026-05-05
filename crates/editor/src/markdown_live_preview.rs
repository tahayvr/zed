use std::any::TypeId;
use std::ops::Range;
use std::sync::Arc;

use collections::{HashMap, HashSet};
use futures::FutureExt;
use gpui::prelude::InteractiveElement as _;
use gpui::{
    App, Context, ElementId, FontStyle, FontWeight, HighlightStyle, ImageSource,
    IntoElement, MouseButton, ParentElement, Refineable, StrikethroughStyle, Styled, StyledImage,
    StyledText, Task, TextStyle, WeakEntity, Window, div, px,
};
use language::{BufferSnapshot, Language, LanguageName, Node, TreeCursor};
use markdown::{
    MarkdownEvent, MarkdownFont, MarkdownStyle, MarkdownTag,
    MarkdownTagEnd, apply_markdown_heading_style_for_level,
    markdown_blockquote_body, markdown_blockquote_div,
    markdown_code_block_content_div, markdown_code_block_parent_div,
    markdown_heading_div_for_level, markdown_rule_div, markdown_table_cell_div,
    markdown_table_div, parse_markdown_blockquote_callout, parse_markdown_events,
    parse_markdown_table_rows,
    render_markdown_paragraph_lines,
};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset};
use settings::Settings;
use theme::ActiveTheme;
use ui::{Checkbox, CopyButton, FluentBuilder, ToggleState, VisibleOnHover};

use crate::{Editor};
use crate::display_map::{
    BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId, FoldPlaceholder,
    HighlightKey, RenderBlock,
};
use crate::editor_settings::EditorSettings;

const IMAGE_BLOCK_FALLBACK_HEIGHT_LINES: u32 = 18;
const IMAGE_BLOCK_MIN_HEIGHT_LINES: u32 = 6;
const IMAGE_BLOCK_MAX_HEIGHT_LINES: u32 = 32;
const IMAGE_BLOCK_MAX_VISIBLE_LINE_FRACTION: f64 = 0.6;
const IMAGE_BLOCK_MAX_WIDTH_PX: f32 = 900.0;
const IMAGE_ESTIMATED_LINE_HEIGHT_PX: f32 = 20.0;
const HORIZONTAL_RULE_BLOCK_HEIGHT_LINES: u32 = 2;
const ACTIVE_SOURCE_BACKGROUND_ALPHA: f32 = 0.35;
const LIVE_PREVIEW_RIGHT_INSET_PX: f32 = 24.0;

/// Type tag used to identify folds created by the markdown live preview feature.
/// This allows selective removal without disturbing other fold types.
struct MarkdownLivePreviewFold;

/// Returns the `TypeId` used to tag markdown live preview folds.
/// Used by `folds_did_change` in `editor.rs` to exclude ephemeral folds from persistence.
pub fn fold_type_id() -> std::any::TypeId {
    TypeId::of::<MarkdownLivePreviewFold>()
}

pub fn block_ids(editor: &Editor) -> HashSet<CustomBlockId> {
    editor
        .markdown_live_preview
        .as_ref()
        .map(|preview| preview.preview_blocks.values().copied().collect())
        .unwrap_or_default()
}

/// Holds the state for the markdown live preview feature within an `Editor`.
pub struct MarkdownLivePreview {
    /// Cached parsed decorator nodes for the current buffer.
    nodes: Vec<MarkdownDecoratorNode>,
    /// Inserted preview blocks keyed by their index into `nodes`.
    /// A node index is present here iff its Replace block is currently inserted.
    preview_blocks: HashMap<usize, CustomBlockId>,
    /// Tracks which inline (non-block-replacement) node indices had the cursor
    /// inside them at the last sync. Used to avoid redundant fold mutations.
    inline_cursor_inside: HashSet<usize>,
    /// Set to true after a refresh so the next sync applies all folds from
    /// scratch rather than only applying delta changes.
    needs_full_fold_sync: bool,
    /// Debounce task for re-parsing after buffer changes.
    _refresh_task: Option<Task<()>>,
}

impl MarkdownLivePreview {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            preview_blocks: HashMap::default(),
            inline_cursor_inside: HashSet::default(),
            needs_full_fold_sync: true,
            _refresh_task: None,
        }
    }
}

/// A single markdown syntax node whose decorators can be hidden.
#[derive(Debug, Clone)]
pub struct MarkdownDecoratorNode {
    /// The full range of the construct (e.g., the entire `**bold**` span).
    pub full_range: Range<Anchor>,
    /// The specific decorator ranges to fold away (e.g., the `**` markers).
    pub decorator_ranges: Vec<Range<Anchor>>,
    /// Kind-specific data for this node.
    pub data: MarkdownNodeData,
}

/// Kind-specific data for a markdown node. Each variant carries exactly the
/// fields it needs — no optional fields that are only meaningful for some kinds.
#[derive(Debug, Clone)]
pub enum MarkdownNodeData {
    // ── Inline nodes (fold + highlight decoration) ──────────────────────────
    Bold,
    Italic,
    BoldItalic,
    Strikethrough,
    InlineCode,
    Link,
    /// A task list checkbox (`[ ]` or `[x]`). Folded and replaced with an
    /// interactive Checkbox widget rendered inline.
    Checkbox,
    /// A list item marker (`-`, `*`, `+`, or `1.`). Folded and replaced with
    /// a rendered bullet or number inline.
    ListMarker { bullet: String },
    Paragraph,
    FrontMatter,

    // ── Block nodes (Replace block decoration) ───────────────────────────────
    Heading { level: u8, preview_text: String },
    Image { url: String },
    FencedCode { language: Option<String>, preview_text: String },
    /// `body` has already had the outermost `> ` marker stripped from every
    /// line, so the renderer does not need to re-do that work each frame.
    Blockquote { body: String },
    HorizontalRule,
    Table { preview_text: String },
    HtmlBlock { preview_text: String },
}

/// Returns true if `data` represents a block-level node that gets a Replace block.
/// These nodes are never folded; their decorator_ranges are only used for
/// cursor-inside detection (to remove the block when the user clicks inside).
fn is_block_replacement(data: &MarkdownNodeData) -> bool {
    matches!(
        data,
        MarkdownNodeData::Heading { .. }
            | MarkdownNodeData::Image { .. }
            | MarkdownNodeData::FencedCode { .. }
            | MarkdownNodeData::Blockquote { .. }
            | MarkdownNodeData::HorizontalRule
            | MarkdownNodeData::Table { .. }
            | MarkdownNodeData::HtmlBlock { .. }
    )
}

/// Returns true if the editor should activate live preview for its current buffer.
pub fn should_activate(editor: &Editor, cx: &App) -> bool {
    if !EditorSettings::get_global(cx).markdown.live_preview {
        return false;
    }
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return false;
    };
    let language_name = buffer
        .read(cx)
        .language()
        .map(|l| l.name())
        .unwrap_or(LanguageName::new(""));
    language_name.0.as_ref() == "Markdown"
}

/// Called after the buffer has been reparsed. Rebuilds the cached node list and
/// re-syncs all decorations from scratch.
pub fn refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer_entity) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let single_snapshot = buffer_entity.read(cx).snapshot();
    let multi_snapshot = editor.buffer().read(cx).snapshot(cx);
    let nodes = collect_nodes(&single_snapshot, &multi_snapshot);

    // Node indices may have shifted after a reparse, so all existing blocks must
    // be removed before the node list is replaced.
    if let Some(preview) = editor.markdown_live_preview.as_mut() {
        let old_ids: HashSet<CustomBlockId> = preview.preview_blocks.values().copied().collect();
        if !old_ids.is_empty() {
            editor.remove_blocks(old_ids, None, cx);
        }
    }
    if let Some(preview) = editor.markdown_live_preview.as_mut() {
        preview.nodes = nodes;
        preview.preview_blocks.clear();
        // Node indices are rebuilt after a reparse, so the fold-tracking cache is stale.
        // Clear it and request a full fold rebuild on the next sync.
        preview.inline_cursor_inside.clear();
        preview.needs_full_fold_sync = true;
        preview._refresh_task = None;
    }

    // Remove any inline folds from the previous parse since node indices reset.
    let buffer_len = multi_snapshot.len().0;
    let full_range = multi_snapshot.anchor_before(MultiBufferOffset(0))
        ..multi_snapshot.anchor_after(MultiBufferOffset(buffer_len));
    editor.remove_folds_with_type(
        &[full_range],
        TypeId::of::<MarkdownLivePreviewFold>(),
        false,
        cx,
    );

    apply_highlights(editor, cx);
    sync(editor, window, cx);
}

/// Called on every cursor movement. Re-syncs folds and block decorations.
pub fn update(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    sync(editor, window, cx);
}

/// Reconciles all live-preview decorations (folds, blocks) with the current
/// cursor position in a single pass over the node list.
fn sync(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(preview) = editor.markdown_live_preview.as_ref() else {
        return;
    };

    let multi_snapshot = editor.buffer().read(cx).snapshot(cx);

    let cursor_offsets: Vec<usize> = editor
        .selections
        .disjoint_anchors()
        .iter()
        .map(|s| s.head().to_offset(&multi_snapshot).0)
        .collect();

    let nodes = preview.nodes.clone();
    let prev_cursor_inside = preview.inline_cursor_inside.clone();
    let full_sync = preview.needs_full_fold_sync;

    // Metrics for block height estimation, derived from live theme/font settings.
    let editor_line_height = editor
        .style(cx)
        .text
        .line_height_in_pixels(window.rem_size())
        .as_f32()
        .max(1.0);
    let markdown_style = preview_markdown_style(window, cx);
    let preview_font_size_px = markdown_style
        .base_text_style
        .font_size
        .to_pixels(window.rem_size())
        .as_f32();
    // Matches the line_height = buffer_font_size * 1.75 formula in MarkdownStyle::themed.
    let preview_line_height_px = preview_font_size_px * 1.75;

    let base_dir: Option<Arc<std::path::Path>> = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buf| {
            buf.read(cx)
                .file()
                .and_then(|f| f.as_local())
                .map(|f| f.abs_path(cx))
        })
        .and_then(|p| p.parent().map(Arc::from));

    let weak_editor = cx.weak_entity();
    let visible_line_count = editor.visible_line_count().map(|count| count as u32);
    let estimated_width = editor
        .visible_column_count()
        .map(|columns| (columns as f32 * 8.0).min(IMAGE_BLOCK_MAX_WIDTH_PX))
        .unwrap_or(IMAGE_BLOCK_MAX_WIDTH_PX);

    let existing: HashMap<usize, CustomBlockId> = editor
        .markdown_live_preview
        .as_ref()
        .map(|p| p.preview_blocks.clone())
        .unwrap_or_default();

    let mut to_fold: Vec<Crease<Anchor>> = Vec::new();
    let mut to_unfold: Vec<Range<Anchor>> = Vec::new();
    let mut active_source_ranges: Vec<Range<Anchor>> = Vec::new();
    let mut new_cursor_inside: HashSet<usize> = HashSet::default();
    let mut to_remove: HashSet<CustomBlockId> = HashSet::default();
    let mut to_remove_indices: Vec<usize> = Vec::new();
    let mut to_insert: Vec<(usize, BlockProperties<Anchor>)> = Vec::new();

    for (node_index, node) in nodes.iter().enumerate() {
        let node_start = node.full_range.start.to_offset(&multi_snapshot).0;
        let node_end = node.full_range.end.to_offset(&multi_snapshot).0;
        let cursor_inside = cursor_offsets
            .iter()
            .any(|&offset| offset >= node_start && offset <= node_end);

        if is_block_replacement(&node.data) {
            if cursor_inside {
                active_source_ranges.push(node.full_range.clone());
            }

            let has_block = existing.contains_key(&node_index);
            match (cursor_inside, has_block) {
                (true, true) => {
                    // Cursor entered: remove the preview block to reveal source.
                    to_remove.insert(existing[&node_index]);
                    to_remove_indices.push(node_index);
                }
                (false, false) => {
                    // Cursor outside, no block yet: build and insert preview block.
                    let props = build_block_props(
                        node,
                        node_index,
                        editor_line_height,
                        preview_line_height_px,
                        base_dir.as_deref(),
                        estimated_width,
                        visible_line_count,
                        editor,
                        cx,
                    );
                    if let Some(props) = props {
                        to_insert.push((node_index, props));
                    }
                }
                _ => {} // No change needed.
            }
        } else {
            // Inline node: manage folds.
            if cursor_inside {
                new_cursor_inside.insert(node_index);
            }

            if node.decorator_ranges.is_empty() {
                continue;
            }

            let was_inside = prev_cursor_inside.contains(&node_index);
            if !full_sync && cursor_inside == was_inside {
                // State hasn't changed for this node: don't touch its folds.
                continue;
            }

            if cursor_inside {
                // Transitioning outside→inside (or full sync with cursor inside): remove the fold.
                for range in &node.decorator_ranges {
                    to_unfold.push(range.clone());
                }
            } else {
                // Transitioning inside→outside (or full sync with cursor outside): add the fold.
                for range in &node.decorator_ranges {
                    let placeholder = match &node.data {
                        MarkdownNodeData::Checkbox => {
                            checkbox_fold_placeholder(weak_editor.clone())
                        }
                        MarkdownNodeData::ListMarker { bullet } => {
                            list_marker_fold_placeholder(bullet.clone())
                        }
                        _ => invisible_fold_placeholder(),
                    };
                    to_fold.push(Crease::simple(range.clone(), placeholder));
                }
            }
        }
    }

    if !to_unfold.is_empty() {
        editor.unfold_ranges(&to_unfold, true, false, cx);
    }
    if !to_fold.is_empty() {
        editor.fold_creases(to_fold, false, window, cx);
    }

    if !to_remove.is_empty() {
        editor.remove_blocks(to_remove, None, cx);
        if let Some(preview) = editor.markdown_live_preview.as_mut() {
            for index in to_remove_indices {
                preview.preview_blocks.remove(&index);
            }
        }
    }
    if !to_insert.is_empty() {
        let (node_indices, props_list): (Vec<_>, Vec<_>) = to_insert.into_iter().unzip();
        let ids = editor.insert_blocks(props_list, None, cx);
        if let Some(preview) = editor.markdown_live_preview.as_mut() {
            for (node_index, block_id) in node_indices.into_iter().zip(ids) {
                preview.preview_blocks.insert(node_index, block_id);
            }
        }
    }

    if let Some(preview) = editor.markdown_live_preview.as_mut() {
        preview.inline_cursor_inside = new_cursor_inside;
        preview.needs_full_fold_sync = false;
    }

    apply_active_source_highlights(editor, active_source_ranges, cx);
}

/// Builds `BlockProperties` for a block-level node. Returns `None` if the node
/// data is missing required fields (e.g., no preview text for a code block).
#[allow(clippy::too_many_arguments)]
fn build_block_props(
    node: &MarkdownDecoratorNode,
    node_index: usize,
    editor_line_height: f32,
    preview_line_height_px: f32,
    base_dir: Option<&std::path::Path>,
    estimated_width: f32,
    visible_line_count: Option<u32>,
    editor: &Editor,
    cx: &mut Context<Editor>,
) -> Option<BlockProperties<Anchor>> {
    let placement = BlockPlacement::Replace(node.full_range.start..=node.full_range.end);

    let props = match &node.data {
        MarkdownNodeData::Heading { level, preview_text } => BlockProperties {
            placement,
            height: Some(heading_block_height(*level)),
            style: BlockStyle::Flex,
            render: render_heading_block(
                preview_text.clone(),
                *level,
            ),
            priority: 0,
        },

        MarkdownNodeData::Image { url } => {
            let resolved_url = resolve_image_url(url, base_dir);
            let image_metadata =
                image_block_metadata(&resolved_url, estimated_width, visible_line_count);
            BlockProperties {
                placement,
                height: Some(image_metadata.height_lines),
                style: BlockStyle::Flex,
                render: render_image_block(
                    resolved_url,
                    image_metadata,
                ),
                priority: 0,
            }
        }

        MarkdownNodeData::FencedCode { language: language_name, preview_text } => {
            let text = trim_trailing_empty_lines(preview_text).to_string();
            let height = preview_block_height(
                text.lines().count(),
                preview_line_height_px,
                36.0,
                editor_line_height,
            );
            let language = language_name.as_deref().and_then(|name| {
                let registry = editor_language_registry(editor, cx)?;
                registry
                    .language_for_name(name)
                    .now_or_never()
                    .and_then(Result::ok)
            });
            let language = language.or_else(|| {
                editor
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .and_then(|buffer| buffer.read(cx).language().cloned())
            });
            BlockProperties {
                placement,
                height: Some(height),
                style: BlockStyle::Flex,
                render: render_code_block(text, language, node_index),
                priority: 0,
            }
        }

        MarkdownNodeData::Blockquote { body } => {
            let height = preview_block_height(
                body.lines().count(),
                preview_line_height_px,
                12.0,
                editor_line_height,
            );
            BlockProperties {
                placement,
                height: Some(height),
                style: BlockStyle::Flex,
                render: render_blockquote_block(body.clone()),
                priority: 0,
            }
        }

        MarkdownNodeData::HorizontalRule => BlockProperties {
            placement,
            height: Some(HORIZONTAL_RULE_BLOCK_HEIGHT_LINES),
            style: BlockStyle::Flex,
            render: render_horizontal_rule_block(),
            priority: 0,
        },

        MarkdownNodeData::Table { preview_text } => {
            let height = preview_block_height(
                preview_text.lines().count(),
                preview_line_height_px,
                12.0,
                editor_line_height,
            );
            BlockProperties {
                placement,
                height: Some(height),
                style: BlockStyle::Flex,
                render: render_table_block(preview_text.clone()),
                priority: 0,
            }
        }

        MarkdownNodeData::HtmlBlock { preview_text } => {
            let height = preview_block_height(
                preview_text.lines().count(),
                preview_line_height_px,
                24.0,
                editor_line_height,
            );
            BlockProperties {
                placement,
                height: Some(height),
                style: BlockStyle::Flex,
                render: render_html_block(preview_text.clone()),
                priority: 0,
            }
        }

        // Inline-only kinds — should never reach here since is_block_replacement filters them.
        _ => return None,
    };

    Some(props)
}

fn editor_language_registry(editor: &Editor, cx: &App) -> Option<Arc<language::LanguageRegistry>> {
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language_registry())
}

fn apply_active_source_highlights(
    editor: &mut Editor,
    ranges: Vec<Range<Anchor>>,
    cx: &mut Context<Editor>,
) {
    editor.clear_background_highlights(HighlightKey::MarkdownLivePreviewActiveSource, cx);

    if !ranges.is_empty() {
        editor.highlight_background(
            HighlightKey::MarkdownLivePreviewActiveSource,
            &ranges,
            |_index, theme| {
                let mut color = theme.colors().editor_document_highlight_read_background;
                color.a *= ACTIVE_SOURCE_BACKGROUND_ALPHA;
                color
            },
            cx,
        );
    }
}

/// Removes all live preview folds, highlights, and block decorations from the editor.
pub fn remove_all(editor: &mut Editor, cx: &mut Context<Editor>) {
    let multi_snapshot = editor.buffer().read(cx).snapshot(cx);
    let buffer_len = multi_snapshot.len().0;
    let full_range = multi_snapshot.anchor_before(MultiBufferOffset(0))
        ..multi_snapshot.anchor_after(MultiBufferOffset(buffer_len));

    editor.remove_folds_with_type(
        &[full_range],
        TypeId::of::<MarkdownLivePreviewFold>(),
        false,
        cx,
    );
    editor.clear_highlights_with(
        &mut |key| matches!(key, HighlightKey::MarkdownLivePreview(_)),
        cx,
    );
    editor.clear_background_highlights(HighlightKey::MarkdownLivePreviewBackground, cx);
    editor.clear_background_highlights(HighlightKey::MarkdownLivePreviewActiveSource, cx);

    if let Some(preview) = editor.markdown_live_preview.as_mut() {
        let ids: HashSet<CustomBlockId> = preview.preview_blocks.values().copied().collect();
        preview.preview_blocks.clear();
        if !ids.is_empty() {
            editor.remove_blocks(ids, None, cx);
        }
    }
}

/// Applies rich-text highlight styles to markdown content ranges.
fn apply_highlights(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(preview) = editor.markdown_live_preview.as_ref() else {
        return;
    };

    let nodes = preview.nodes.clone();
    let mut highlight_ranges: Vec<(HighlightStyle, Vec<Range<Anchor>>)> = Vec::new();

    for node in &nodes {
        if is_block_replacement(&node.data) {
            continue;
        }

        let style = highlight_style_for_data(&node.data, cx);
        if let Some(style) = style {
            let content_range = node.full_range.clone();
            if let Some(entry) = highlight_ranges.iter_mut().find(|(s, _)| *s == style) {
                entry.1.push(content_range);
            } else {
                highlight_ranges.push((style, vec![content_range]));
            }
        }
    }

    editor.clear_highlights_with(
        &mut |key| matches!(key, HighlightKey::MarkdownLivePreview(_)),
        cx,
    );
    for (i, (style, ranges)) in highlight_ranges.into_iter().enumerate() {
        editor.highlight_text(HighlightKey::MarkdownLivePreview(i), ranges, style, cx);
    }
}

fn preview_block_height(
    line_count: usize,
    preview_line_height: f32,
    vertical_padding: f32,
    editor_line_height: f32,
) -> u32 {
    ((line_count.max(1) as f32 * preview_line_height + vertical_padding) / editor_line_height)
        .ceil()
        .max(1.0) as u32
}

fn preview_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    MarkdownStyle::themed(preview_markdown_font(), window, cx)
}

fn preview_markdown_font() -> MarkdownFont {
    MarkdownFont::Preview
}

fn heading_block_height(level: u8) -> u32 {
    match level {
        1 => 3,
        _ => 2,
    }
}

#[derive(Clone, Copy)]
struct ImageBlockMetadata {
    height_lines: u32,
    loaded_local_dimensions: bool,
}

fn image_block_metadata(
    resolved_url: &str,
    available_width: f32,
    visible_line_count: Option<u32>,
) -> ImageBlockMetadata {
    let height_lines = image_block_height_lines(resolved_url, available_width, visible_line_count);
    ImageBlockMetadata {
        height_lines,
        loaded_local_dimensions: !resolved_url.contains("://")
            && image::image_dimensions(resolved_url).is_ok(),
    }
}

fn image_block_height_lines(
    resolved_url: &str,
    available_width: f32,
    visible_line_count: Option<u32>,
) -> u32 {
    let max_height_lines = visible_line_count
        .map(|line_count| {
            ((line_count as f64 * IMAGE_BLOCK_MAX_VISIBLE_LINE_FRACTION).ceil() as u32)
                .clamp(IMAGE_BLOCK_MIN_HEIGHT_LINES, IMAGE_BLOCK_MAX_HEIGHT_LINES)
        })
        .unwrap_or(IMAGE_BLOCK_MAX_HEIGHT_LINES);

    if resolved_url.contains("://") {
        return IMAGE_BLOCK_FALLBACK_HEIGHT_LINES.min(max_height_lines);
    }

    let Ok((width, height)) = image::image_dimensions(resolved_url) else {
        return IMAGE_BLOCK_FALLBACK_HEIGHT_LINES.min(max_height_lines);
    };
    if width == 0 || height == 0 {
        return IMAGE_BLOCK_FALLBACK_HEIGHT_LINES.min(max_height_lines);
    }

    let rendered_width = (width as f32).min(available_width.max(1.0));
    let rendered_height = rendered_width * height as f32 / width as f32;
    ((rendered_height / IMAGE_ESTIMATED_LINE_HEIGHT_PX).ceil() as u32 + 1)
        .clamp(IMAGE_BLOCK_MIN_HEIGHT_LINES, max_height_lines)
}

/// Resolves an image URL to an absolute string suitable for display.
/// Returns an absolute URI (http/https) or an absolute file path string.
fn resolve_image_url(url: &str, base_dir: Option<&std::path::Path>) -> String {
    if url.contains("://") {
        return url.to_string();
    }
    if let Some(base) = base_dir {
        return base.join(url).to_string_lossy().into_owned();
    }
    url.to_string()
}

/// Attaches a click handler that reveals the source for the block when the user
/// clicks anywhere on the element, placing the cursor at the block's start anchor.


fn render_heading_block(
    text: String,
    level: u8,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let element = markdown_heading_div_for_level(&markdown_style, level, None)
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .child(text.as_ref().to_string());
        let mut element = element;
        element.style().refine(&markdown_style.heading);
        apply_markdown_heading_style_for_level(
            element,
            level,
            markdown_style.heading_level_styles.as_ref(),
        )
        .into_any_element()
    })
}

/// Constructs the render closure for an image block decoration.
/// Captures only a `String` (Send + Sync) to avoid ImageSource::Custom's non-Send dyn Fn.
fn render_image_block(
    resolved_url: String,
    metadata: ImageBlockMetadata,
) -> RenderBlock {
    let resolved_url: Arc<str> = resolved_url.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        if resolved_url.is_empty() {
            return gpui::Empty.into_any_element();
        }
        let source: ImageSource = if resolved_url.contains("://") {
            ImageSource::from(resolved_url.to_string())
        } else {
            ImageSource::from(std::path::PathBuf::from(resolved_url.as_ref()))
        };
        let container = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .py_0p5();
        if !metadata.loaded_local_dimensions && !resolved_url.contains("://") {
            let colors = cx.app.theme().colors();
            return container
                .rounded_lg()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.element_background)
                .text_color(colors.text_muted)
                .font(cx.editor_style.text.font())
                .child(
                    div()
                        .h_full()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child("Image unavailable"),
                )
                .into_any_element();
        }
        container
            .child(
                gpui::img(source)
                    .object_fit(gpui::ObjectFit::Contain)
                    .w_full()
                    .h_full(),
            )
            .into_any_element()
    })
}

fn render_code_block(
    text: String,
    language: Option<Arc<Language>>,
    node_index: usize,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let colors = cx.app.theme().colors();
        let text_style = preview_code_text_style(&markdown_style, cx.editor_style.text.clone());
        let outer = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .bg(colors.editor_background)
            .overflow_hidden();

        outer
            .child(
                markdown_code_block_parent_div(&markdown_style, colors.border_variant, true)
                    .w_full()
                    .font(text_style.font())
                    .text_color(colors.text)
                    .line_height(text_style.line_height)
                    .child(markdown_code_block_content_div().overflow_x_hidden().children(code_lines(
                        text.as_ref(),
                        language.as_ref(),
                        &text_style,
                        cx.app,
                    )))
                    .child(
                        div().flex().flex_row()
                            .w_4()
                            .absolute()
                            .top_0()
                            .right_0()
                            .justify_end()
                            .visible_on_hover("code_block")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(render_copy_code_block_button(node_index, text.to_string())),
                    ),
            )
            .into_any_element()
    })
}

fn render_copy_code_block_button(node_index: usize, code: String) -> impl IntoElement {
    CopyButton::new(
        ElementId::Name(format!("markdown-live-preview-copy-code-{}", node_index).into()),
        code,
    )
}

fn trim_trailing_empty_lines(text: &str) -> &str {
    text.trim_end_matches(['\n', '\r'])
}

fn code_lines(
    text: &str,
    language: Option<&Arc<Language>>,
    text_style: &gpui::TextStyle,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    text.lines()
        .map(|line| {
            let highlights = language
                .map(|language| syntax_highlights_for_line(line, language, cx))
                .unwrap_or_default();
            div()
                .whitespace_nowrap()
                .child(
                    StyledText::new(line.to_string())
                        .with_default_highlights(text_style, highlights),
                )
                .into_any_element()
        })
        .collect()
}

fn syntax_highlights_for_line(
    line: &str,
    language: &Arc<Language>,
    cx: &App,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let rope = rope::Rope::from(line);
    language
        .highlight_text(&rope, 0..line.len())
        .into_iter()
        .filter_map(|(range, highlight_id)| {
            cx.theme()
                .syntax()
                .get(highlight_id)
                .cloned()
                .map(|style| (range, style))
        })
        .collect()
}

/// Strips one level of `> ` / `>` blockquote markers from each line.
fn strip_blockquote_level(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.strip_prefix("> ")
                .or_else(|| line.strip_prefix(">"))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders a blockquote body (one level already stripped) recursively.
/// Lines that still start with `> ` are grouped into nested blockquote divs;
/// other lines are rendered as inline-markdown paragraphs.
fn render_blockquote_body_elements(
    body: &str,
    markdown_style: &MarkdownStyle,
) -> Vec<gpui::AnyElement> {
    let mut elements: Vec<gpui::AnyElement> = Vec::new();
    let mut paragraph_lines: Vec<&str> = Vec::new();
    let mut inner_quote_lines: Vec<&str> = Vec::new();

    let flush_paragraph =
        |lines: &mut Vec<&str>, elements: &mut Vec<gpui::AnyElement>| {
            if !lines.is_empty() {
                let text = lines.join("\n");
                elements.extend(render_markdown_paragraph_lines(&text, markdown_style));
                lines.clear();
            }
        };

    let flush_inner_quote =
        |lines: &mut Vec<&str>, elements: &mut Vec<gpui::AnyElement>| {
            if !lines.is_empty() {
                let source = lines.join("\n");
                let stripped = strip_blockquote_level(&source);
                let callout = parse_markdown_blockquote_callout(&stripped);
                let body = markdown_blockquote_body(&stripped, callout);
                let inner_elements = render_blockquote_body_elements(body, markdown_style);
                elements.push(
                    markdown_blockquote_div(markdown_style, callout)
                        .children(inner_elements)
                        .into_any_element(),
                );
                lines.clear();
            }
        };

    for line in body.lines() {
        if line.starts_with("> ") || line == ">" {
            flush_paragraph(&mut paragraph_lines, &mut elements);
            inner_quote_lines.push(line);
        } else {
            flush_inner_quote(&mut inner_quote_lines, &mut elements);
            paragraph_lines.push(line);
        }
    }
    flush_paragraph(&mut paragraph_lines, &mut elements);
    flush_inner_quote(&mut inner_quote_lines, &mut elements);

    elements
}

fn render_blockquote_block(
    body: String,
) -> RenderBlock {
    let body: Arc<str> = body.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let callout = parse_markdown_blockquote_callout(body.as_ref());
        let inner_body = markdown_blockquote_body(body.as_ref(), callout);
        let text_color = markdown_style
            .block_quote
            .color
            .unwrap_or_else(|| cx.app.theme().colors().text);
        let outer = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full();
        outer
            .child(
                markdown_blockquote_div(&markdown_style, callout)
                    .text_size(markdown_style.base_text_style.font_size)
                    .text_color(text_color)
                    .line_height(markdown_style.base_text_style.line_height)
                    .children(render_blockquote_body_elements(inner_body, &markdown_style)),
            )
            .into_any_element()
    })
}

fn render_horizontal_rule_block() -> RenderBlock {
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let container = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full();
        container
            .child(markdown_rule_div(&markdown_style).w_full())
            .into_any_element()
    })
}

fn render_table_block(
    text: String,
) -> RenderBlock {
    let rows: Arc<[Vec<String>]> = parse_markdown_table_rows(&text).into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let col_count = rows.first().map_or(0, |row| row.len()) as u16;
        let outer = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full();
        outer
            .child(
                markdown_table_div(&markdown_style, col_count, colors)
                    .text_size(markdown_style.base_text_style.font_size)
                    .text_color(colors.text)
                    .line_height(markdown_style.base_text_style.line_height)
                    .children(
                        rows.iter()
                            .enumerate()
                            .flat_map(|(row_index, row)| {
                                let is_header = row_index == 0;
                                row.iter()
                                    .enumerate()
                                    .map(|(column_index, cell)| {
                                        markdown_table_cell_div(
                                            is_header,
                                            row_index,
                                            column_index,
                                            colors,
                                        )
                                        .when(is_header, |this| {
                                            this.font_weight(FontWeight::SEMIBOLD)
                                        })
                                        .children(render_markdown_paragraph_lines(
                                            cell,
                                            &markdown_style,
                                        ))
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .into_any_element()
    })
}

fn render_html_block(
    text: String,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let text_style = preview_code_text_style(&markdown_style, cx.editor_style.text.clone());
        let container =
            markdown_code_block_parent_div(&markdown_style, colors.border_variant, true)
                .pl(cx.anchor_x)
                .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
                .w_full()
                .h_full()
                .bg(colors.element_background)
                .font(text_style.font())
                .text_color(colors.text_muted)
                .line_height(text_style.line_height)
                .overflow_hidden();
        let mut container = container;
        container.style().margin = Default::default();
        container
            .child(
                markdown_code_block_content_div()
                    .px_2()
                    .py_1()
                    .children(code_lines(text.as_ref(), None, &text_style, cx.app)),
            )
            .into_any_element()
    })
}

/// Creates a fold placeholder that renders an inline bullet or ordered marker.
fn list_marker_fold_placeholder(bullet: String) -> FoldPlaceholder {
    FoldPlaceholder {
        render: Arc::new(move |_, _, _| {
            gpui::StyledText::new(bullet.clone()).into_any_element()
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<MarkdownLivePreviewFold>()),
        collapsed_text: None,
        hide_fold_indicator: true,
    }
}

/// Creates a fold placeholder that renders an interactive `Checkbox` widget inline.
/// The checked state is derived from the buffer text at the fold range at render time.
fn checkbox_fold_placeholder(weak_editor: WeakEntity<Editor>) -> FoldPlaceholder {
    FoldPlaceholder {
        render: Arc::new(move |fold_id, range, cx| {
            let checked = weak_editor
                .read_with(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let start = range.start.to_offset(&snapshot).0;
                    let end = range.end.to_offset(&snapshot).0;
                    snapshot.text_for_range(MultiBufferOffset(start)..MultiBufferOffset(end)).collect::<String>()
                })
                .map(|text| text == "[x]" || text == "[X]")
                .unwrap_or(false);

            let toggle_state = if checked {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            };
            let weak_editor_click = weak_editor.clone();
            Checkbox::new(ElementId::from(fold_id), toggle_state)
                .fill()
                .on_click(move |_state, _window, cx| {
                    weak_editor_click
                        .update(cx, |editor, cx| {
                            let snapshot = editor.buffer().read(cx).snapshot(cx);
                            let start = range.start.to_offset(&snapshot).0;
                            let end = range.end.to_offset(&snapshot).0;
                            let marker_start =
                                snapshot.anchor_before(MultiBufferOffset(start));
                            let marker_end =
                                snapshot.anchor_after(MultiBufferOffset(end));
                            let new_text: Arc<str> =
                                if checked { "[ ]".into() } else { "[x]".into() };
                            editor.buffer().update(cx, |buffer, cx| {
                                buffer.edit(
                                    [(marker_start..marker_end, new_text)],
                                    None,
                                    cx,
                                );
                            });
                        })
                        .ok();
                })
                .into_any_element()
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<MarkdownLivePreviewFold>()),
        collapsed_text: None,
        hide_fold_indicator: true,
    }
}

fn preview_code_text_style(
    markdown_style: &MarkdownStyle,
    fallback_text_style: TextStyle,
) -> TextStyle {
    let mut text_style = fallback_text_style;
    text_style.refine(&markdown_style.code_block.text);
    text_style
}

/// Returns the highlight style to apply to the content of a markdown node.
fn highlight_style_for_data(data: &MarkdownNodeData, cx: &App) -> Option<HighlightStyle> {
    let colors = cx.theme().colors();
    match data {
        MarkdownNodeData::Bold => Some(HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        }),
        MarkdownNodeData::Italic => Some(HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        }),
        MarkdownNodeData::BoldItalic => Some(HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        }),
        MarkdownNodeData::Strikethrough => Some(HighlightStyle {
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
            ..Default::default()
        }),
        MarkdownNodeData::InlineCode => Some(HighlightStyle {
            background_color: Some(colors.editor_foreground.opacity(0.08)),
            ..Default::default()
        }),
        MarkdownNodeData::Link => Some(HighlightStyle {
            background_color: Some(colors.editor_foreground.opacity(0.025)),
            color: Some(colors.text_accent),
            underline: Some(gpui::UnderlineStyle {
                thickness: px(1.0),
                color: Some(colors.text_accent.opacity(0.5)),
                wavy: false,
            }),
            ..Default::default()
        }),
        MarkdownNodeData::Heading { level, .. } => {
            let color = colors.text;
            let weight = FontWeight::BOLD;
            let fade_out = (*level >= 5).then_some(0.1);
            Some(HighlightStyle {
                color: Some(color),
                font_weight: Some(weight),
                fade_out,
                ..Default::default()
            })
        }
        // Block-replacement kinds don't get inline highlights.
        MarkdownNodeData::Image { .. }
        | MarkdownNodeData::FencedCode { .. }
        | MarkdownNodeData::Blockquote { .. }
        | MarkdownNodeData::HorizontalRule
        | MarkdownNodeData::Table { .. }
        | MarkdownNodeData::HtmlBlock { .. }
        | MarkdownNodeData::ListMarker { .. }
        | MarkdownNodeData::Checkbox
        | MarkdownNodeData::Paragraph
        | MarkdownNodeData::FrontMatter => None,
    }
}

/// Creates a zero-width fold placeholder used to hide syntax markers.
fn invisible_fold_placeholder() -> FoldPlaceholder {
    FoldPlaceholder {
        render: Arc::new(|_, _, _| gpui::Empty.into_any_element()),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<MarkdownLivePreviewFold>()),
        collapsed_text: None,
        hide_fold_indicator: true,
    }
}

/// Walks the syntax tree of the buffer and collects all markdown decorator nodes.
pub fn collect_nodes(
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Vec<MarkdownDecoratorNode> {
    let mut nodes = Vec::new();

    for layer in snapshot.syntax_layers() {
        let language_name = layer.language.name();
        let name_str = language_name.0.as_ref();

        match name_str {
            "Markdown" => collect_block_nodes(layer.node(), snapshot, multi_snapshot, &mut nodes),
            "Markdown-Inline" => {
                collect_inline_nodes(layer.node(), snapshot, multi_snapshot, &mut nodes)
            }
            _ => {}
        }
    }

    collect_list_marker_nodes(snapshot, multi_snapshot, &mut nodes);

    let nodes = remove_ineligible_paragraph_nodes(nodes, multi_snapshot);
    remove_inline_nodes_inside_fenced_code(nodes, multi_snapshot)
}

fn remove_ineligible_paragraph_nodes(
    nodes: Vec<MarkdownDecoratorNode>,
    multi_snapshot: &MultiBufferSnapshot,
) -> Vec<MarkdownDecoratorNode> {
    let paragraph_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| matches!(node.data, MarkdownNodeData::Paragraph))
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();
    let replacement_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| {
            !matches!(node.data, MarkdownNodeData::Paragraph) && is_block_replacement(&node.data)
        })
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();
    nodes
        .into_iter()
        .filter(|node| {
            if !matches!(node.data, MarkdownNodeData::Paragraph) {
                return true;
            }

            let node_start = node.full_range.start.to_offset(multi_snapshot).0;
            let node_end = node.full_range.end.to_offset(multi_snapshot).0;

            !replacement_ranges
                .iter()
                .any(|range| node_start >= range.start && node_end <= range.end)
                && !paragraph_ranges.iter().any(|range| {
                    range.start != node_start && node_start >= range.start && node_end <= range.end
                })
        })
        .collect()
}

/// Collects `ListMarker` nodes for every list item line in the buffer.
///
/// Each node covers only the raw marker characters (e.g. `-`, `1.`) at the
/// start of the item. The leading indentation and the space after the marker
/// are left in the buffer so the editor handles spacing naturally. The fold
/// placeholder renders the display form (`•`, `1.`, etc.) inline.
///
/// Lines inside fenced code blocks are skipped. Checkbox markers (`[ ]`,
/// `[x]`) are handled separately by tree-sitter via `Checkbox` nodes; this
/// function still emits a `ListMarker` for the bullet part of those lines.
fn collect_list_marker_nodes(
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    nodes: &mut Vec<MarkdownDecoratorNode>,
) {
    let fenced_code_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| matches!(node.data, MarkdownNodeData::FencedCode { .. }))
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();

    let text = snapshot
        .chars_for_range(0..snapshot.len())
        .collect::<String>();
    let mut byte_offset = 0usize;

    for line in text.split_inclusive('\n') {
        let line_start = byte_offset;
        byte_offset += line.len();

        let line_trimmed = line.trim_end_matches(['\r', '\n']);

        // Skip lines inside fenced code blocks.
        let inside_fenced_code = fenced_code_ranges
            .iter()
            .any(|range| line_start >= range.start && line_start < range.end);
        if inside_fenced_code {
            continue;
        }

        if let Some((fold_range, bullet)) =
            list_marker_fold_range(line_trimmed, line_start)
        {
            let full_range = byte_range_to_anchor_range(fold_range.clone(), multi_snapshot);
            nodes.push(MarkdownDecoratorNode {
                full_range: full_range.clone(),
                decorator_ranges: vec![full_range],
                data: MarkdownNodeData::ListMarker { bullet },
            });
        }
    }
}

/// Returns the byte range (within the full buffer) and display text for the
/// raw list marker on a given line, or `None` if the line is not a list item.
///
/// The fold range covers the raw marker characters only (e.g. `-` or `1.`),
/// NOT the mandatory space that follows. That space remains visible in the
/// buffer so the editor provides natural glyph spacing between the rendered
/// placeholder and the item content.
fn list_marker_fold_range(
    line: &str,
    line_start: usize,
) -> Option<(Range<usize>, String)> {
    let indent_bytes = line.len() - line.trim_start().len();
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let first_char = trimmed.chars().next()?;

    let (raw_marker_len, display) = if matches!(first_char, '-' | '+' | '*') {
        // Unordered: marker is the single character, followed by whitespace.
        let rest = &trimmed[first_char.len_utf8()..];
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        (first_char.len_utf8(), "•".to_string())
    } else if first_char.is_ascii_digit() {
        // Ordered: one or more digits followed by `.` or `)` then whitespace.
        let dot_pos = trimmed
            .char_indices()
            .find_map(|(i, c)| matches!(c, '.' | ')').then_some((i, c)))?;
        let number = &trimmed[..dot_pos.0];
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let marker_end = dot_pos.0 + dot_pos.1.len_utf8();
        let rest = &trimmed[marker_end..];
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let display = format!("{number}.");
        (marker_end, display)
    } else {
        return None;
    };

    let marker_start = line_start + indent_bytes;
    let marker_end = marker_start + raw_marker_len;
    Some((marker_start..marker_end, display))
}

fn remove_inline_nodes_inside_fenced_code(
    nodes: Vec<MarkdownDecoratorNode>,
    multi_snapshot: &MultiBufferSnapshot,
) -> Vec<MarkdownDecoratorNode> {
    let fenced_code_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| matches!(node.data, MarkdownNodeData::FencedCode { .. }))
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();

    if fenced_code_ranges.is_empty() {
        return nodes;
    }

    nodes
        .into_iter()
        .filter(|node| {
            if !matches!(
                node.data,
                MarkdownNodeData::Bold
                    | MarkdownNodeData::Italic
                    | MarkdownNodeData::BoldItalic
                    | MarkdownNodeData::Strikethrough
                    | MarkdownNodeData::InlineCode
                    | MarkdownNodeData::Link
                    | MarkdownNodeData::Image { .. }
                    | MarkdownNodeData::Checkbox
                    | MarkdownNodeData::ListMarker { .. }
            ) {
                return true;
            }

            let node_start = node.full_range.start.to_offset(multi_snapshot).0;
            let node_end = node.full_range.end.to_offset(multi_snapshot).0;
            !fenced_code_ranges
                .iter()
                .any(|range| node_start >= range.start && node_end <= range.end)
        })
        .collect()
}

/// Collects block-level markdown nodes (headings, task list markers, fenced code, blockquotes).
fn collect_block_nodes(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    nodes: &mut Vec<MarkdownDecoratorNode>,
) {
    let mut cursor = node.walk();
    visit_block_nodes(&mut cursor, snapshot, multi_snapshot, nodes);
}

fn visit_block_nodes(
    cursor: &mut TreeCursor<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    nodes: &mut Vec<MarkdownDecoratorNode>,
) {
    loop {
        let node = cursor.node();

        // Skip error nodes — they represent incomplete/malformed input and may
        // produce garbled ranges if descended into.
        if node.is_error() {
            if !cursor.goto_next_sibling() {
                break;
            }
            continue;
        }

        match node.kind() {
            "atx_heading" => {
                if let Some(decorator) = parse_atx_heading(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "setext_heading" => {
                if let Some(decorator) = parse_setext_heading(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            "task_list_marker_unchecked" | "task_list_marker_checked" => {
                if let Some(decorator) = parse_task_list_marker(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "fenced_code_block" => {
                if let Some(decorator) = parse_fenced_code_block(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "block_quote" => {
                if let Some(decorator) = parse_blockquote(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
                // Don't descend — the blockquote parser handles the whole subtree.
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            "thematic_break" => {
                let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
                nodes.push(MarkdownDecoratorNode {
                    decorator_ranges: vec![full_range.clone()],
                    full_range,
                    data: MarkdownNodeData::HorizontalRule,
                });
            }
            "pipe_table" => {
                let preview_text = snapshot.chars_for_range(node.byte_range()).collect();
                let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
                nodes.push(MarkdownDecoratorNode {
                    decorator_ranges: vec![full_range.clone()],
                    full_range,
                    data: MarkdownNodeData::Table { preview_text },
                });
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            "html_block" => {
                let preview_text = snapshot.chars_for_range(node.byte_range()).collect();
                let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
                nodes.push(MarkdownDecoratorNode {
                    decorator_ranges: vec![full_range.clone()],
                    full_range,
                    data: MarkdownNodeData::HtmlBlock { preview_text },
                });
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            "paragraph" => {
                if let Some(decorator) = parse_image_paragraph(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                } else if let Some(decorator) = parse_paragraph(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "minus_metadata" | "plus_metadata" => {
                let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
                nodes.push(MarkdownDecoratorNode {
                    decorator_ranges: vec![full_range.clone()],
                    full_range,
                    data: MarkdownNodeData::FrontMatter,
                });
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            _ => {}
        }

        if cursor.goto_first_child() {
            visit_block_nodes(cursor, snapshot, multi_snapshot, nodes);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn parse_paragraph(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let preview_text = snapshot
        .chars_for_range(node.byte_range())
        .collect::<String>()
        .trim()
        .to_string();
    if preview_text.is_empty() {
        return None;
    }

    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![],
        data: MarkdownNodeData::Paragraph,
    })
}

fn parse_image_paragraph(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let source = snapshot
        .chars_for_range(node.byte_range())
        .collect::<String>();
    let parsed = parse_markdown_events(&source);

    let mut image_destination = None;
    let mut in_image = false;
    let mut image_count: usize = 0;
    let mut paragraph_depth: usize = 0;

    for (range, event) in parsed.iter() {
        match event {
            MarkdownEvent::Start(MarkdownTag::Paragraph) => {
                paragraph_depth += 1;
            }
            MarkdownEvent::End(MarkdownTagEnd::Paragraph) => {
                paragraph_depth = paragraph_depth.saturating_sub(1);
            }
            MarkdownEvent::Start(MarkdownTag::Image { dest_url, .. }) => {
                if paragraph_depth == 0 || image_destination.is_some() {
                    return None;
                }
                image_destination = Some(dest_url.to_string());
                image_count += 1;
                in_image = true;
            }
            MarkdownEvent::End(MarkdownTagEnd::Image) => {
                in_image = false;
            }
            MarkdownEvent::Text if in_image => {}
            MarkdownEvent::SubstitutedText(_) if in_image => {}
            MarkdownEvent::Text => {
                if !source[range.clone()].trim().is_empty() {
                    return None;
                }
            }
            MarkdownEvent::SubstitutedText(text) => {
                if !text.trim().is_empty() {
                    return None;
                }
            }
            MarkdownEvent::SoftBreak
            | MarkdownEvent::HardBreak
            | MarkdownEvent::RootStart
            | MarkdownEvent::RootEnd(_) => {}
            MarkdownEvent::Start(_)
            | MarkdownEvent::End(_)
            | MarkdownEvent::Code
            | MarkdownEvent::Html
            | MarkdownEvent::InlineHtml
            | MarkdownEvent::FootnoteReference(_)
            | MarkdownEvent::Rule
            | MarkdownEvent::TaskListMarker(_) => {
                if !in_image {
                    return None;
                }
            }
        }
    }

    if image_count != 1 || in_image || paragraph_depth != 0 {
        return None;
    }

    let url = image_destination?;
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        data: MarkdownNodeData::Image { url },
    })
}

/// Collects inline markdown nodes (bold, italic, code spans, links, images).
fn collect_inline_nodes(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    nodes: &mut Vec<MarkdownDecoratorNode>,
) {
    let mut cursor = node.walk();
    visit_inline_nodes(&mut cursor, snapshot, multi_snapshot, nodes);
}

fn visit_inline_nodes(
    cursor: &mut TreeCursor<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    nodes: &mut Vec<MarkdownDecoratorNode>,
) {
    loop {
        let node = cursor.node();
        match node.kind() {
            "strong_emphasis" => {
                if let Some(decorator) = parse_emphasis_node(node, snapshot, multi_snapshot, true) {
                    nodes.push(decorator);
                }
                // Don't descend into strong_emphasis — we handle its children directly.
            }
            "emphasis" => {
                if let Some(decorator) = parse_emphasis_node(node, snapshot, multi_snapshot, false)
                {
                    nodes.push(decorator);
                }
                // Don't descend — we handle delimiters (including nested bold-italic) directly.
            }
            "code_span" => {
                if let Some(decorator) = parse_code_span(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "strikethrough" => {
                if let Some(decorator) = parse_strikethrough_node(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "inline_link" => {
                if let Some(decorator) = parse_inline_link(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "uri_autolink" | "email_autolink" => {
                if let Some(decorator) = parse_autolink(node, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            "image" => {
                if let Some(decorator) = parse_image(node, snapshot, multi_snapshot) {
                    nodes.push(decorator);
                }
            }
            _ => {
                if cursor.goto_first_child() {
                    visit_inline_nodes(cursor, snapshot, multi_snapshot, nodes);
                    cursor.goto_parent();
                }
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn byte_range_to_anchor_range(
    byte_range: std::ops::Range<usize>,
    multi_snapshot: &MultiBufferSnapshot,
) -> Range<Anchor> {
    multi_snapshot.anchor_before(MultiBufferOffset(byte_range.start))
        ..multi_snapshot.anchor_after(MultiBufferOffset(byte_range.end))
}

fn parse_atx_heading(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut marker_range: Option<Range<Anchor>> = None;
    let mut level: u8 = 1;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            let heading_level = match child.kind() {
                "atx_h1_marker" => Some(1u8),
                "atx_h2_marker" => Some(2),
                "atx_h3_marker" => Some(3),
                "atx_h4_marker" => Some(4),
                "atx_h5_marker" => Some(5),
                "atx_h6_marker" => Some(6),
                _ => None,
            };
            if let Some(l) = heading_level {
                level = l;
                // Include the trailing space that follows the marker.
                let buffer_len = snapshot.len();
                let end_byte = if child.end_byte() < buffer_len {
                    child.end_byte() + 1
                } else {
                    child.end_byte()
                };
                marker_range = Some(byte_range_to_anchor_range(
                    child.start_byte()..end_byte,
                    multi_snapshot,
                ));
                break;
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let marker_range = marker_range?;
    let marker_start = marker_range.start.to_offset(multi_snapshot).0;
    let marker_end = marker_range.end.to_offset(multi_snapshot).0;
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    let mut preview_text = snapshot
        .chars_for_range(node.byte_range())
        .collect::<String>();
    let start = marker_start.saturating_sub(node.start_byte());
    let end = marker_end.saturating_sub(node.start_byte());
    if start <= end && end <= preview_text.len() {
        preview_text.replace_range(start..end, "");
    }

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![marker_range],
        data: MarkdownNodeData::Heading {
            level,
            preview_text: preview_text.trim().to_string(),
        },
    })
}

fn parse_setext_heading(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut underline_range: Option<Range<usize>> = None;
    let mut level = 1;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            match child.kind() {
                "setext_h1_underline" => {
                    underline_range = Some(child.byte_range());
                    level = 1;
                    break;
                }
                "setext_h2_underline" => {
                    underline_range = Some(child.byte_range());
                    level = 2;
                    break;
                }
                _ => {}
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let underline_range = underline_range?;
    let node_range = node.byte_range();
    let full_range = byte_range_to_anchor_range(node_range.clone(), multi_snapshot);
    let preview_text = snapshot
        .chars_for_range(node_range.start..underline_range.start)
        .collect::<String>()
        .trim()
        .to_string();

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![byte_range_to_anchor_range(underline_range, multi_snapshot)],
        data: MarkdownNodeData::Heading { level, preview_text },
    })
}

fn parse_task_list_marker(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    Some(MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        data: MarkdownNodeData::Checkbox,
    })
}

fn parse_fenced_code_block(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut content_start: Option<usize> = None;
    let mut content_end: Option<usize> = None;
    let mut language: Option<String> = None;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            match child.kind() {
                "info_string" => {
                    language = snapshot
                        .chars_for_range(child.byte_range())
                        .collect::<String>()
                        .trim()
                        .split_whitespace()
                        .next()
                        .map(str::to_string);
                }
                "code_fence_content" => {
                    content_start = Some(child.start_byte());
                    content_end = Some(child.end_byte());
                    break;
                }
                _ => {}
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let node_range = node.byte_range();

    let (decorator_ranges, preview_text) =
        if let (Some(start), Some(end)) = (content_start, content_end) {
            // Fold the opening fence line (up to where content begins) and the closing fence line.
            let mut ranges = Vec::new();
            if node_range.start < start {
                ranges.push(byte_range_to_anchor_range(
                    node_range.start..start,
                    multi_snapshot,
                ));
            }
            if end < node_range.end {
                ranges.push(byte_range_to_anchor_range(
                    end..node_range.end,
                    multi_snapshot,
                ));
            }
            let text = snapshot.chars_for_range(start..end).collect::<String>();
            (ranges, Some(text))
        } else {
            // No content node — fold the entire block.
            (
                vec![byte_range_to_anchor_range(
                    node_range.clone(),
                    multi_snapshot,
                )],
                None,
            )
        };

    if decorator_ranges.is_empty() {
        return None;
    }

    let preview_text = preview_text?;
    let full_range = byte_range_to_anchor_range(node_range, multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        data: MarkdownNodeData::FencedCode { language, preview_text },
    })
}

/// Collects all blockquote marker byte ranges within a `block_quote` subtree recursively.
fn collect_block_quote_markers(node: Node<'_>, out: &mut Vec<Range<usize>>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let child = cursor.node();
        if matches!(child.kind(), "block_quote_marker" | "block_continuation") {
            out.push(child.byte_range());
        } else {
            collect_block_quote_markers(child, out);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn parse_blockquote(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut marker_byte_ranges: Vec<Range<usize>> = Vec::new();
    collect_block_quote_markers(node, &mut marker_byte_ranges);

    if marker_byte_ranges.is_empty() {
        return None;
    }

    let decorator_ranges: Vec<Range<Anchor>> = marker_byte_ranges
        .iter()
        .map(|range| byte_range_to_anchor_range(range.clone(), multi_snapshot))
        .collect();

    let node_range = node.byte_range();
    // Strip the outermost level of `> ` markers at parse time so render closures
    // receive already-processed body text rather than raw source.
    let raw_source: String = snapshot
        .chars_for_range(node_range.clone())
        .collect();
    let body = strip_blockquote_level(&raw_source);

    let full_range = byte_range_to_anchor_range(node_range, multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        data: MarkdownNodeData::Blockquote { body },
    })
}

fn parse_emphasis_node(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    is_strong: bool,
) -> Option<MarkdownDecoratorNode> {
    let mut delimiter_ranges: Vec<Range<Anchor>> = Vec::new();
    let mut has_nested_emphasis = false;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            match child.kind() {
                "emphasis_delimiter" => {
                    delimiter_ranges.push(byte_range_to_anchor_range(
                        child.byte_range(),
                        multi_snapshot,
                    ));
                }
                // Collect delimiters from nested emphasis nodes for bold-italic (***text***).
                "emphasis" | "strong_emphasis" => {
                    has_nested_emphasis = true;
                    let mut inner = child.walk();
                    if inner.goto_first_child() {
                        loop {
                            if inner.node().kind() == "emphasis_delimiter" {
                                delimiter_ranges.push(byte_range_to_anchor_range(
                                    inner.node().byte_range(),
                                    multi_snapshot,
                                ));
                            }
                            if !inner.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if delimiter_ranges.is_empty() {
        return None;
    }

    let data = if has_nested_emphasis {
        MarkdownNodeData::BoldItalic
    } else if is_strong {
        MarkdownNodeData::Bold
    } else {
        MarkdownNodeData::Italic
    };

    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        data,
    })
}

fn parse_strikethrough_node(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut delimiter_ranges = Vec::new();

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.kind() == "emphasis_delimiter" {
                delimiter_ranges.push(byte_range_to_anchor_range(
                    child.byte_range(),
                    multi_snapshot,
                ));
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if delimiter_ranges.is_empty() {
        return None;
    }

    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        data: MarkdownNodeData::Strikethrough,
    })
}

fn parse_code_span(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut delimiter_ranges: Vec<Range<Anchor>> = Vec::new();

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.kind() == "code_span_delimiter" {
                delimiter_ranges.push(byte_range_to_anchor_range(
                    child.byte_range(),
                    multi_snapshot,
                ));
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if delimiter_ranges.is_empty() {
        return None;
    }

    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        data: MarkdownNodeData::InlineCode,
    })
}

fn parse_autolink(
    node: Node<'_>,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let node_range = node.byte_range();
    if node_range.end.saturating_sub(node_range.start) < 2 {
        return None;
    }

    let full_range = byte_range_to_anchor_range(node_range.clone(), multi_snapshot);
    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![
            byte_range_to_anchor_range(node_range.start..node_range.start + 1, multi_snapshot),
            byte_range_to_anchor_range(node_range.end - 1..node_range.end, multi_snapshot),
        ],
        data: MarkdownNodeData::Link,
    })
}

fn parse_inline_link(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    // For [text](url), hide the opening `[` and the tail `](url)` as two folds,
    // leaving only the link text visible.
    let mut link_text_range: Option<Range<usize>> = None;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.kind() == "link_text" {
                link_text_range = Some(child.byte_range());
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let link_text_range = link_text_range?;
    let node_range = node.byte_range();

    // Fold the opening `[` before the link text.
    let open_bracket =
        byte_range_to_anchor_range(node_range.start..node_range.start + 1, multi_snapshot);
    // Fold everything from `]` to the end of the node: `](url)`.
    let tail = byte_range_to_anchor_range(link_text_range.end..node_range.end, multi_snapshot);

    let full_range = byte_range_to_anchor_range(node_range, multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![open_bracket, tail],
        data: MarkdownNodeData::Link,
    })
}

fn parse_image(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    // For ![alt](url), fold everything except the alt text.
    let mut image_desc_range: Option<Range<usize>> = None;
    let mut link_dest_range: Option<Range<usize>> = None;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            match child.kind() {
                "image_description" => image_desc_range = Some(child.byte_range()),
                "link_destination" => link_dest_range = Some(child.byte_range()),
                _ => {}
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let node_range = node.byte_range();

    // Extract URL text from the link_destination node.
    let url: Option<String> = link_dest_range.as_ref().map(|dest_range| {
        let url = snapshot
            .chars_for_range(dest_range.clone())
            .collect::<String>();
        url.strip_prefix('<')
            .and_then(|url| url.strip_suffix('>'))
            .unwrap_or(&url)
            .to_string()
    });

    let decorator_ranges =
        if let (Some(desc), Some(dest)) = (image_desc_range.clone(), link_dest_range) {
            // Hide `![` and `](url)`.
            vec![
                byte_range_to_anchor_range(node_range.start..node_range.start + 2, multi_snapshot),
                byte_range_to_anchor_range(desc.end..dest.start, multi_snapshot),
                byte_range_to_anchor_range(dest.end..node_range.end, multi_snapshot),
            ]
        } else if let Some(desc) = image_desc_range {
            vec![
                byte_range_to_anchor_range(node_range.start..node_range.start + 2, multi_snapshot),
                byte_range_to_anchor_range(desc.end..node_range.end, multi_snapshot),
            ]
        } else {
            // Fall back to folding the whole thing.
            vec![byte_range_to_anchor_range(
                node_range.clone(),
                multi_snapshot,
            )]
        };

    let full_range = byte_range_to_anchor_range(node_range, multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        data: MarkdownNodeData::Image { url: url.unwrap_or_default() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invisible_fold_placeholder_has_correct_type_tag() {
        let placeholder = invisible_fold_placeholder();
        assert_eq!(
            placeholder.type_tag,
            Some(TypeId::of::<MarkdownLivePreviewFold>())
        );
        assert!(!placeholder.constrain_width);
        assert!(!placeholder.merge_adjacent);
        assert!(placeholder.collapsed_text.is_none());
        assert!(placeholder.hide_fold_indicator);
    }

    #[test]
    fn test_is_block_replacement() {
        assert!(is_block_replacement(&MarkdownNodeData::HorizontalRule));
        assert!(is_block_replacement(&MarkdownNodeData::Heading {
            level: 1,
            preview_text: String::new()
        }));
        assert!(!is_block_replacement(&MarkdownNodeData::Bold));
        assert!(!is_block_replacement(&MarkdownNodeData::Italic));
        assert!(!is_block_replacement(&MarkdownNodeData::InlineCode));
    }

    #[test]
    fn test_node_data_variants() {
        assert!(matches!(MarkdownNodeData::Bold, MarkdownNodeData::Bold));
        assert!(!matches!(MarkdownNodeData::Bold, MarkdownNodeData::Italic));
        assert!(matches!(
            MarkdownNodeData::Heading { level: 1, preview_text: String::new() },
            MarkdownNodeData::Heading { level: 1, .. }
        ));
        assert!(matches!(MarkdownNodeData::Checkbox, MarkdownNodeData::Checkbox));
    }

    #[test]
    fn test_resolve_image_url_absolute() {
        let url = resolve_image_url("https://example.com/img.png", None);
        assert_eq!(url, "https://example.com/img.png");
    }

    #[test]
    fn test_resolve_image_url_relative_path() {
        let base = std::path::Path::new("/tmp/notes");
        let url = resolve_image_url("images/photo.png", Some(base));
        assert!(url.contains("images/photo.png"));
    }

    #[test]
    fn test_image_block_height_uses_fallback_for_remote_images() {
        assert_eq!(
            image_block_height_lines(
                "https://example.com/image.png",
                IMAGE_BLOCK_MAX_WIDTH_PX,
                None
            ),
            IMAGE_BLOCK_FALLBACK_HEIGHT_LINES
        );
    }

    #[test]
    fn test_image_block_height_uses_fallback_for_missing_local_images() {
        assert_eq!(
            image_block_height_lines(
                "/tmp/definitely-missing-image.png",
                IMAGE_BLOCK_MAX_WIDTH_PX,
                None,
            ),
            IMAGE_BLOCK_FALLBACK_HEIGHT_LINES
        );
    }

    #[test]
    fn test_image_block_height_caps_to_visible_lines() {
        assert_eq!(
            image_block_height_lines(
                "https://example.com/image.png",
                IMAGE_BLOCK_MAX_WIDTH_PX,
                Some(10)
            ),
            6
        );
    }

    #[test]
    fn test_trim_trailing_empty_lines() {
        assert_eq!(trim_trailing_empty_lines("a\n\n"), "a");
        assert_eq!(trim_trailing_empty_lines("a\n b\n"), "a\n b");
    }

    #[test]
    fn test_parse_callout_header() {
        assert!(parse_markdown_blockquote_callout("[!warning]\nBe careful").is_some());
        assert!(parse_markdown_blockquote_callout("regular quote").is_none());
    }

    #[test]
    fn test_heading_block_height_leaves_preview_spacing() {
        assert_eq!(heading_block_height(1), 3);
        assert_eq!(heading_block_height(3), 2);
    }

    #[test]
    fn test_preview_markdown_style_uses_preview_font() {
        assert!(matches!(preview_markdown_font(), MarkdownFont::Preview));
    }

    #[test]
    fn test_list_marker_fold_range() {
        // Unordered bullet at start of line: fold just the `-`, space stays.
        assert_eq!(
            list_marker_fold_range("- item", 0),
            Some((0..1, "•".to_string()))
        );
        // Indented bullet: fold starts after indent.
        assert_eq!(
            list_marker_fold_range("  - item", 0),
            Some((2..3, "•".to_string()))
        );
        // Ordered marker: fold covers digits + period.
        assert_eq!(
            list_marker_fold_range("1. item", 0),
            Some((0..2, "1.".to_string()))
        );
        assert_eq!(
            list_marker_fold_range("10. item", 5),
            Some((5..8, "10.".to_string()))
        );
        // Checkbox item: bullet fold still emits `•` for the `-` part.
        assert_eq!(
            list_marker_fold_range("- [ ] task", 0),
            Some((0..1, "•".to_string()))
        );
        // Plain text line: no fold.
        assert_eq!(list_marker_fold_range("plain text", 0), None);
        // Marker without following space: no fold.
        assert_eq!(list_marker_fold_range("-item", 0), None);
    }

    #[test]
    fn test_parse_table_rows_skips_delimiter_row() {
        assert_eq!(
            parse_markdown_table_rows("| Name | Value |\n| --- | :---: |\n| A | 1 |"),
            vec![
                vec!["Name".to_string(), "Value".to_string()],
                vec!["A".to_string(), "1".to_string()]
            ]
        );
    }

    #[test]
    fn test_table_delimiter_detection() {
        assert!(markdown::is_markdown_table_delimiter_row(
            "| --- | :---: | ---: |"
        ));
        assert!(!markdown::is_markdown_table_delimiter_row(
            "| Name | Value |"
        ));
    }

    #[test]
    fn test_strip_blockquote_level() {
        assert_eq!(strip_blockquote_level("> hello\n> world"), "hello\nworld");
        assert_eq!(strip_blockquote_level(">hello"), "hello");
        assert_eq!(strip_blockquote_level("> > nested"), "> nested");
    }

    #[test]
    fn test_preview_block_height_min_one() {
        // Even with zero lines the height should be at least 1.
        assert_eq!(preview_block_height(0, 20.0, 0.0, 20.0), 1);
    }
}
