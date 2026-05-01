use std::any::TypeId;
use std::ops::Range;
use std::sync::Arc;

use collections::HashSet;
use futures::FutureExt;
use gpui::prelude::InteractiveElement as _;
use gpui::{
    App, Context, ElementId, FontStyle, FontWeight, HighlightStyle, ImageSource, IntoElement,
    MouseButton, ParentElement, Refineable, StrikethroughStyle, Styled, StyledImage, StyledText,
    Task, TextStyle, WeakEntity, Window, div, px,
};
use language::{BufferSnapshot, Language, LanguageName, Node, TreeCursor};
use markdown::{
    MARKDOWN_PARAGRAPH_LINE_HEIGHT_REM, MarkdownEvent, MarkdownFont, MarkdownStyle, MarkdownTag,
    MarkdownTagEnd, RenderedMarkdownListItem, apply_markdown_heading_style_for_level,
    markdown_blockquote_body, markdown_blockquote_border_color, markdown_blockquote_style,
    markdown_code_block_content_div, markdown_code_block_parent_div,
    markdown_heading_div_for_level, markdown_list_div, markdown_list_item_content_div,
    markdown_list_item_div, markdown_paragraph_div, markdown_rule_div, markdown_table_cell_div,
    markdown_table_div, parse_markdown_blockquote_callout, parse_markdown_events,
    parse_markdown_list_item_line, parse_markdown_list_items, parse_markdown_table_rows,
    render_markdown_paragraph_lines,
};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset};
use settings::Settings;
use theme::ActiveTheme;
use text::Point;
use ui::{CopyButton, FluentBuilder, VisibleOnHover};

use crate::Editor;
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
const PREVIEW_LIST_LINE_HEIGHT_REMS: f32 = 1.3;
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
        .map(|preview| preview.block_ids.clone())
        .unwrap_or_default()
}

/// Holds the state for the markdown live preview feature within an `Editor`.
pub struct MarkdownLivePreview {
    /// Cached parsed decorator nodes for the current buffer.
    nodes: Vec<MarkdownDecoratorNode>,
    /// Block decoration IDs currently inserted for image previews.
    block_ids: HashSet<CustomBlockId>,
    /// Debounce task for re-parsing after buffer changes.
    _refresh_task: Option<Task<()>>,
}

/// A single markdown syntax node whose decorators can be hidden.
#[derive(Debug, Clone)]
pub struct MarkdownDecoratorNode {
    /// The full range of the construct (e.g., the entire `**bold**` span).
    pub full_range: Range<Anchor>,
    /// The specific decorator ranges to fold away (e.g., the `**` markers).
    pub decorator_ranges: Vec<Range<Anchor>>,
    /// What kind of node this is, used to determine highlight styling.
    pub kind: MarkdownNodeKind,
    /// For nodes that warrant a background (fenced code, blockquote): the range to highlight.
    pub background_range: Option<Range<Anchor>>,
    /// For image nodes: the resolved URL text extracted from the link destination.
    pub image_url: Option<String>,
    /// For fenced code blocks: the language name from the info string, if present.
    pub code_language: Option<String>,
    /// Rendered preview text for block-level markdown nodes.
    pub preview_text: Option<String>,
    /// Preferred cursor target when clicking a replacement preview block.
    pub cursor_anchor: Option<Anchor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownNodeKind {
    Heading(u8),
    Paragraph,
    Bold,
    Italic,
    BoldItalic,
    Strikethrough,
    InlineCode,
    Link,
    Image,
    Checkbox { checked: bool },
    FencedCode,
    Blockquote,
    HorizontalRule,
    Table,
    HtmlBlock,
    List,
}

impl MarkdownLivePreview {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            block_ids: HashSet::default(),
            _refresh_task: None,
        }
    }
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
/// applies folds and highlights.
pub fn refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer_entity) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let single_snapshot = buffer_entity.read(cx).snapshot();
    let multi_snapshot = editor.buffer().read(cx).snapshot(cx);
    let nodes = collect_nodes(&single_snapshot, &multi_snapshot);

    if let Some(preview) = editor.markdown_live_preview.as_mut() {
        preview.nodes = nodes;
        preview._refresh_task = None;
    }

    apply_highlights(editor, &multi_snapshot, cx);
    apply_background_highlights(editor, &multi_snapshot, cx);
    update_folds(editor, window, cx);
}

/// Called on every cursor movement. Re-evaluates which nodes should be folded
/// based on current selection positions, and manages image block decorations.
pub fn update_folds(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(preview) = editor.markdown_live_preview.as_ref() else {
        return;
    };

    let multi_snapshot = editor.buffer().read(cx).snapshot(cx);

    // Collect cursor head offsets from anchor-based selections.
    let cursor_offsets: Vec<usize> = editor
        .selections
        .disjoint_anchors()
        .iter()
        .map(|s| s.head().to_offset(&multi_snapshot).0)
        .collect();

    let nodes = preview.nodes.clone();

    let mut to_fold: Vec<Crease<Anchor>> = Vec::new();
    let mut active_source_ranges: Vec<Range<Anchor>> = Vec::new();
    // All decorator ranges across every node — used to sweep out any lingering
    // non-typed folds (e.g. folds restored from a previous session's DB state before
    // the fix that prevents markdown live preview folds from being persisted).
    let mut all_decorator_ranges: Vec<Range<Anchor>> = Vec::new();

    for node in &nodes {
        let node_start = node.full_range.start.to_offset(&multi_snapshot).0;
        let node_end = node.full_range.end.to_offset(&multi_snapshot).0;

        let cursor_inside = cursor_offsets
            .iter()
            .any(|&offset| offset >= node_start && offset <= node_end);

        if cursor_inside && uses_replacement_preview(node.kind) {
            active_source_ranges.push(node.full_range.clone());
        }

        for range in &node.decorator_ranges {
            all_decorator_ranges.push(range.clone());
        }

        if uses_replacement_preview(node.kind) {
            continue;
        }

        if node.decorator_ranges.is_empty() {
            continue;
        }

        for range in &node.decorator_ranges {
            if !cursor_inside {
                let placeholder = invisible_fold_placeholder();
                to_fold.push(Crease::simple(range.clone(), placeholder));
            }
        }
    }

    let buffer_len = multi_snapshot.len().0;
    let full_range = multi_snapshot.anchor_before(MultiBufferOffset(0))
        ..multi_snapshot.anchor_after(MultiBufferOffset(buffer_len));

    if !all_decorator_ranges.is_empty() {
        editor.unfold_ranges(&all_decorator_ranges, true, false, cx);
    }

    editor.remove_folds_with_type(
        &[full_range],
        TypeId::of::<MarkdownLivePreviewFold>(),
        false,
        cx,
    );

    if !to_fold.is_empty() {
        editor.fold_creases(to_fold, false, window, cx);
    }

    apply_active_source_highlights(editor, active_source_ranges, cx);
    apply_blocks(editor, &nodes, &cursor_offsets, &multi_snapshot, cx);
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

fn uses_replacement_preview(kind: MarkdownNodeKind) -> bool {
    matches!(
        kind,
        MarkdownNodeKind::Heading(_)
            | MarkdownNodeKind::Paragraph
            | MarkdownNodeKind::Image
            | MarkdownNodeKind::FencedCode
            | MarkdownNodeKind::Blockquote
            | MarkdownNodeKind::HorizontalRule
            | MarkdownNodeKind::Table
            | MarkdownNodeKind::HtmlBlock
            | MarkdownNodeKind::List
    )
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
        let block_ids = std::mem::take(&mut preview.block_ids);
        if !block_ids.is_empty() {
            editor.remove_blocks(block_ids, None, cx);
        }
    }
}

/// Applies rich-text highlight styles to markdown content ranges.
fn apply_highlights(
    editor: &mut Editor,
    multi_snapshot: &MultiBufferSnapshot,
    cx: &mut Context<Editor>,
) {
    let Some(preview) = editor.markdown_live_preview.as_ref() else {
        return;
    };

    let nodes = preview.nodes.clone();
    let mut highlight_ranges: Vec<(HighlightStyle, Vec<Range<Anchor>>)> = Vec::new();

    for node in &nodes {
        if uses_replacement_preview(node.kind) {
            continue;
        }

        let style = highlight_style_for_kind(node.kind, cx);
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

    let _ = multi_snapshot; // used by caller to ensure snapshot is current
}

/// Applies background color highlights for fenced code blocks and blockquotes.
fn apply_background_highlights(
    editor: &mut Editor,
    _multi_snapshot: &MultiBufferSnapshot,
    cx: &mut Context<Editor>,
) {
    let Some(preview) = editor.markdown_live_preview.as_ref() else {
        return;
    };

    let ranges: Vec<Range<Anchor>> = preview
        .nodes
        .iter()
        .filter(|node| !uses_replacement_preview(node.kind))
        .filter_map(|node| node.background_range.clone())
        .collect();

    editor.clear_background_highlights(HighlightKey::MarkdownLivePreviewBackground, cx);

    if !ranges.is_empty() {
        editor.highlight_background(
            HighlightKey::MarkdownLivePreviewBackground,
            &ranges,
            |_index, theme| {
                let mut color = theme.colors().editor_document_highlight_read_background;
                color.a *= 0.6;
                color
            },
            cx,
        );
    }
}

/// Inserts or removes image block decorations based on cursor position.
/// Blocks are shown when the cursor is outside the image node's range.
fn apply_blocks(
    editor: &mut Editor,
    nodes: &[MarkdownDecoratorNode],
    cursor_offsets: &[usize],
    multi_snapshot: &MultiBufferSnapshot,
    cx: &mut Context<Editor>,
) {
    // Remove all existing image blocks before rebuilding.
    if let Some(preview) = editor.markdown_live_preview.as_mut() {
        let old_ids = std::mem::take(&mut preview.block_ids);
        if !old_ids.is_empty() {
            editor.remove_blocks(old_ids, None, cx);
        }
    }

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
        .and_then(|p| p.parent().map(|dir| Arc::from(dir)));

    let mut new_blocks: Vec<BlockProperties<Anchor>> = Vec::new();
    let weak_editor = cx.weak_entity();
    let visible_line_count = editor.visible_line_count().map(|count| count as u32);

    for node in nodes {
        let node_start = node.full_range.start.to_offset(multi_snapshot).0;
        let node_end = node.full_range.end.to_offset(multi_snapshot).0;
        let cursor_inside = cursor_offsets
            .iter()
            .any(|&offset| offset >= node_start && offset <= node_end);
        if cursor_inside {
            continue;
        }

        let replacement_end = replacement_end_anchor(node, multi_snapshot);

        match node.kind {
            MarkdownNodeKind::Heading(level) => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=replacement_end),
                    height: Some(heading_block_height(level)),
                    style: BlockStyle::Flex,
                    render: render_heading_block(
                        text,
                        level,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::Paragraph => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = text.lines().count().max(1) as u32;
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=replacement_end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_paragraph_block(
                        text,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::Image => {
                let Some(url_text) = node.image_url.as_deref() else {
                    continue;
                };
                let resolved_url = resolve_image_url(url_text, base_dir.as_deref());
                let estimated_width = editor
                    .visible_column_count()
                    .map(|columns| (columns as f32 * 8.0).min(IMAGE_BLOCK_MAX_WIDTH_PX))
                    .unwrap_or(IMAGE_BLOCK_MAX_WIDTH_PX);
                let image_metadata =
                    image_block_metadata(&resolved_url, estimated_width, visible_line_count);
                let render = render_image_block(
                    resolved_url,
                    image_metadata,
                    node.cursor_anchor.unwrap_or(node.full_range.start),
                    weak_editor.clone(),
                );
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(image_metadata.height_lines),
                    style: BlockStyle::Flex,
                    render,
                    priority: 0,
                });
            }
            MarkdownNodeKind::FencedCode => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let text = trim_trailing_empty_lines(&text).to_string();
                let line_height = 14.0 * 1.75;
                let height = preview_block_height(text.lines().count(), line_height, 36.0);
                let language = node.code_language.as_deref().and_then(|language_name| {
                    let registry = editor
                        .buffer()
                        .read(cx)
                        .as_singleton()
                        .and_then(|buffer| buffer.read(cx).language_registry())?;
                    registry
                        .language_for_name(language_name)
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
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_code_block(
                        text,
                        language,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::Blockquote => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = preview_block_height(text.lines().count(), 14.0 * 1.75, 12.0);
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_blockquote_block(
                        text,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::HorizontalRule => {
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(HORIZONTAL_RULE_BLOCK_HEIGHT_LINES),
                    style: BlockStyle::Flex,
                    render: render_horizontal_rule_block(
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::Table => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = preview_block_height(text.lines().count(), 14.0 * 1.75, 12.0);
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_table_block(
                        text,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::HtmlBlock => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = preview_block_height(text.lines().count(), 14.0 * 1.75, 24.0);
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_html_block(
                        text,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            MarkdownNodeKind::List => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = preview_block_height(
                    text.lines().count(),
                    14.0 * PREVIEW_LIST_LINE_HEIGHT_REMS,
                    6.0,
                );
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_list_block(
                        text,
                        node.cursor_anchor.unwrap_or(node.full_range.start),
                        weak_editor.clone(),
                    ),
                    priority: 0,
                });
            }
            _ => {}
        }
    }

    if !new_blocks.is_empty() {
        let ids = editor.insert_blocks(new_blocks, None, cx);
        if let Some(preview) = editor.markdown_live_preview.as_mut() {
            preview.block_ids.extend(ids);
        }
    }
}

fn preview_block_height(line_count: usize, line_height: f32, vertical_padding: f32) -> u32 {
    ((line_count.max(1) as f32 * line_height + vertical_padding) / IMAGE_ESTIMATED_LINE_HEIGHT_PX)
        .ceil() as u32
}

fn replacement_end_anchor(
    node: &MarkdownDecoratorNode,
    multi_snapshot: &MultiBufferSnapshot,
) -> Anchor {
    match node.kind {
        MarkdownNodeKind::Heading(_) | MarkdownNodeKind::Paragraph => {
            let end_offset = node.full_range.end.to_offset(multi_snapshot).0;
            let end_point = multi_snapshot.offset_to_point(MultiBufferOffset(end_offset));
            let next_row = end_point.row + 1;

            if next_row <= multi_snapshot.max_point().row
                && multi_snapshot.is_line_blank(multi_buffer::MultiBufferRow(next_row))
            {
                return multi_snapshot.anchor_after(Point::new(next_row, 0));
            }

            node.full_range.end
        }
        _ => node.full_range.end,
    }
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
        2 => 2,
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

/// Constructs the render closure for an image block decoration.
/// Captures only a `String` (Send + Sync) to avoid ImageSource::Custom's non-Send dyn Fn.
fn render_image_block(
    resolved_url: String,
    metadata: ImageBlockMetadata,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
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
        let mut container = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .py_0p5();
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
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
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let colors = cx.app.theme().colors();
        let text_style = preview_code_text_style(&markdown_style, cx.editor_style.text.clone());
        let mut outer = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .bg(colors.editor_background)
            .overflow_hidden();
        attach_source_click_handler(&mut outer, cursor_anchor, weak_editor.clone());

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
                            .child(render_copy_code_block_button(text.to_string())),
                    ),
            )
            .into_any_element()
    })
}

fn render_copy_code_block_button(code: String) -> impl IntoElement {
    CopyButton::new(
        ElementId::Name("markdown-live-preview-copy-code".into()),
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

fn render_heading_block(
    text: String,
    level: u8,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let mut element = markdown_heading_div_for_level(&markdown_style, level, None)
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .text_size(markdown_style.base_text_style.font_size)
            .child(text.as_ref().to_string());
        element.style().refine(&markdown_style.heading);
        attach_source_click_handler(&mut element, cursor_anchor, weak_editor.clone());
        apply_markdown_heading_style_for_level(
            element,
            level,
            markdown_style.heading_level_styles.as_ref(),
        )
        .into_any_element()
    })
}

fn render_paragraph_block(
    text: String,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let mut container = markdown_paragraph_div(&markdown_style, None)
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .font(markdown_style.base_text_style.font())
            .text_size(markdown_style.base_text_style.font_size)
            .text_color(markdown_style.base_text_style.color)
            .line_height(markdown_style.base_text_style.line_height);
        container.style().margin = Default::default();
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
        container
            .children(render_markdown_paragraph_lines(
                text.as_ref(),
                &markdown_style,
            ))
            .into_any_element()
    })
}

fn render_blockquote_block(
    text: String,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let callout = parse_markdown_blockquote_callout(text.as_ref());
        let border_color = callout
            .map(|callout| markdown_blockquote_border_color(&markdown_style, Some(callout)))
            .unwrap_or(markdown_style.block_quote_border_color);
        let text_color = markdown_style
            .block_quote
            .color
            .unwrap_or_else(|| cx.app.theme().colors().text_muted);
        let mut container = markdown_blockquote_style(border_color)
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .font(markdown_style.base_text_style.font())
            .text_size(markdown_style.base_text_style.font_size)
            .text_color(text_color)
            .line_height(markdown_style.base_text_style.line_height);
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
        container
            .children(render_markdown_paragraph_lines(
                markdown_blockquote_body(text.as_ref(), callout),
                &markdown_style,
            ))
            .into_any_element()
    })
}

fn render_horizontal_rule_block(
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let mut container = div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full();
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
        container
            .child(markdown_rule_div(&markdown_style).w_full())
            .into_any_element()
    })
}

fn render_table_block(
    text: String,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let rows: Arc<[Vec<String>]> = parse_markdown_table_rows(&text).into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let mut container = markdown_table_div(
            &markdown_style,
            rows.first().map_or(0, |row| row.len()) as u16,
            colors,
        )
        .pl(cx.anchor_x)
        .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
        .w_full()
        .h_full()
        .font(markdown_style.base_text_style.font())
        .text_size(markdown_style.base_text_style.font_size)
        .text_color(colors.text)
        .line_height(markdown_style.base_text_style.line_height);
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
        container
            .children(rows.iter().enumerate().map(|(row_index, row)| {
                let is_header = row_index == 0;
                div()
                    .flex()
                    .w_full()
                    .min_h(cx.line_height)
                    .children(row.iter().enumerate().map(move |(column_index, cell)| {
                        markdown_table_cell_div(is_header, row_index, column_index, colors)
                            .flex_1()
                            .min_w_0()
                            .when(is_header, |this| this.font_weight(FontWeight::SEMIBOLD))
                            .child(cell.clone())
                    }))
            }))
            .into_any_element()
    })
}

fn render_html_block(
    text: String,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let text_style = preview_code_text_style(&markdown_style, cx.editor_style.text.clone());
        let mut container =
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
        container.style().margin = Default::default();
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
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

fn render_list_block(
    text: String,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let items: Arc<[RenderedMarkdownListItem]> = parse_markdown_list_items(&text).into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let markdown_style = preview_markdown_style(cx.window, cx.app);
        let mut container = markdown_list_div()
            .pl(cx.anchor_x)
            .pr(px(LIVE_PREVIEW_RIGHT_INSET_PX))
            .w_full()
            .h_full()
            .pl_2p5()
            .font(markdown_style.base_text_style.font())
            .text_size(markdown_style.base_text_style.font_size)
            .text_color(markdown_style.base_text_style.color)
            .line_height(gpui::rems(MARKDOWN_PARAGRAPH_LINE_HEIGHT_REM));
        attach_source_click_handler(&mut container, cursor_anchor, weak_editor.clone());
        container
            .children(
                items
                    .iter()
                    .map(|item| render_list_item(item, colors.text_muted, &markdown_style)),
            )
            .into_any_element()
    })
}

fn render_list_item(
    item: &RenderedMarkdownListItem,
    marker_color: gpui::Hsla,
    markdown_style: &MarkdownStyle,
) -> gpui::AnyElement {
    markdown_list_item_div(
        markdown_style,
        div()
            .w(px(16.0))
            .flex_none()
            .text_color(marker_color)
            .child(item.marker.clone())
            .into_any_element(),
    )
    .mb_1()
    .pl(px(item.indent_columns as f32 * 12.0))
    .child(
        markdown_list_item_content_div()
            .children(render_markdown_paragraph_lines(&item.text, markdown_style)),
    )
    .into_any_element()
}

fn preview_code_text_style(
    markdown_style: &MarkdownStyle,
    fallback_text_style: TextStyle,
) -> TextStyle {
    let mut text_style = fallback_text_style;
    text_style.refine(&markdown_style.code_block.text);
    text_style
}

fn attach_source_click_handler(
    element: &mut gpui::Div,
    cursor_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) {
    element
        .interactivity()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            let _ = weak_editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([cursor_anchor..cursor_anchor]);
                });
            });
        });
}

/// Returns the highlight style to apply to the content of a markdown node.
fn highlight_style_for_kind(kind: MarkdownNodeKind, cx: &App) -> Option<HighlightStyle> {
    let colors = cx.theme().colors();
    match kind {
        MarkdownNodeKind::Heading(level) => {
            let color = colors.text;
            let weight = FontWeight::BOLD;
            let fade_out = (level >= 5).then_some(0.1);
            Some(HighlightStyle {
                color: Some(color),
                font_weight: Some(weight),
                fade_out,
                ..Default::default()
            })
        }
        MarkdownNodeKind::Bold => Some(HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        }),
        MarkdownNodeKind::Italic => Some(HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        }),
        MarkdownNodeKind::BoldItalic => Some(HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        }),
        MarkdownNodeKind::Strikethrough => Some(HighlightStyle {
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
            ..Default::default()
        }),
        MarkdownNodeKind::InlineCode => Some(HighlightStyle {
            background_color: Some(colors.editor_foreground.opacity(0.08)),
            ..Default::default()
        }),
        MarkdownNodeKind::Link => Some(HighlightStyle {
            background_color: Some(colors.editor_foreground.opacity(0.025)),
            color: Some(colors.text_accent),
            underline: Some(gpui::UnderlineStyle {
                thickness: px(1.0),
                color: Some(colors.text_accent.opacity(0.5)),
                wavy: false,
            }),
            ..Default::default()
        }),
        MarkdownNodeKind::Image => Some(HighlightStyle {
            color: Some(colors.text_muted),
            ..Default::default()
        }),
        MarkdownNodeKind::Checkbox { .. } => None,
        MarkdownNodeKind::FencedCode => None,
        MarkdownNodeKind::Blockquote => Some(HighlightStyle {
            color: Some(colors.text_muted),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        }),
        MarkdownNodeKind::HorizontalRule
        | MarkdownNodeKind::Table
        | MarkdownNodeKind::HtmlBlock
        | MarkdownNodeKind::List
        | MarkdownNodeKind::Paragraph => None,
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

    collect_text_list_nodes(snapshot, multi_snapshot, &mut nodes);

    let nodes = remove_ineligible_paragraph_nodes(nodes, multi_snapshot);
    remove_inline_nodes_inside_fenced_code(nodes, multi_snapshot)
}

fn remove_ineligible_paragraph_nodes(
    nodes: Vec<MarkdownDecoratorNode>,
    multi_snapshot: &MultiBufferSnapshot,
) -> Vec<MarkdownDecoratorNode> {
    let paragraph_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| node.kind == MarkdownNodeKind::Paragraph)
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();
    let replacement_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| {
            node.kind != MarkdownNodeKind::Paragraph && uses_replacement_preview(node.kind)
        })
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();
    nodes
        .into_iter()
        .filter(|node| {
            if node.kind != MarkdownNodeKind::Paragraph {
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

fn collect_text_list_nodes(
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
    nodes: &mut Vec<MarkdownDecoratorNode>,
) {
    let text = snapshot
        .chars_for_range(0..snapshot.len())
        .collect::<String>();
    let fenced_code_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| node.kind == MarkdownNodeKind::FencedCode)
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();

    let existing_list_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| node.kind == MarkdownNodeKind::List)
        .map(|node| {
            node.full_range.start.to_offset(multi_snapshot).0
                ..node.full_range.end.to_offset(multi_snapshot).0
        })
        .collect();

    for (range, text) in text_list_ranges(&text, &fenced_code_ranges, &existing_list_ranges) {
        let full_range = byte_range_to_anchor_range(range, multi_snapshot);
        let cursor_anchor = full_range.start;
        nodes.push(MarkdownDecoratorNode {
            full_range: full_range.clone(),
            decorator_ranges: vec![full_range],
            kind: MarkdownNodeKind::List,
            background_range: None,
            image_url: None,
            code_language: None,
            preview_text: Some(text),
            cursor_anchor: Some(cursor_anchor),
        });
    }
}

fn text_list_ranges(
    text: &str,
    fenced_code_ranges: &[Range<usize>],
    existing_list_ranges: &[Range<usize>],
) -> Vec<(Range<usize>, String)> {
    let mut ranges = Vec::new();
    let mut block_start: Option<usize> = None;
    let mut block_end = 0;
    let mut block_text = String::new();
    let mut offset = 0;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        let line_start = offset;
        let line_end = offset + line.len();
        offset = line_end;

        let inside_fenced_code = fenced_code_ranges
            .iter()
            .any(|range| line_start >= range.start && line_start < range.end);
        let is_list_line =
            !inside_fenced_code && parse_markdown_list_item_line(line_without_newline).is_some();

        if is_list_line {
            if block_start.is_none() {
                block_start = Some(line_start);
            }
            block_end = line_end;
            block_text.push_str(line);
            continue;
        }

        push_text_list_range(
            block_start.take(),
            block_end,
            &mut block_text,
            existing_list_ranges,
            &mut ranges,
        );
    }

    push_text_list_range(
        block_start,
        block_end,
        &mut block_text,
        existing_list_ranges,
        &mut ranges,
    );

    ranges
}

fn push_text_list_range(
    block_start: Option<usize>,
    block_end: usize,
    block_text: &mut String,
    existing_list_ranges: &[Range<usize>],
    ranges: &mut Vec<(Range<usize>, String)>,
) {
    if let Some(block_start) = block_start
        && !existing_list_ranges
            .iter()
            .any(|range| block_start >= range.start && block_end <= range.end)
    {
        ranges.push((block_start..block_end, block_text.trim_end().to_string()));
    }
    block_text.clear();
}

fn remove_inline_nodes_inside_fenced_code(
    nodes: Vec<MarkdownDecoratorNode>,
    multi_snapshot: &MultiBufferSnapshot,
) -> Vec<MarkdownDecoratorNode> {
    let fenced_code_ranges: Vec<Range<usize>> = nodes
        .iter()
        .filter(|node| node.kind == MarkdownNodeKind::FencedCode)
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
                node.kind,
                MarkdownNodeKind::Bold
                    | MarkdownNodeKind::Italic
                    | MarkdownNodeKind::BoldItalic
                    | MarkdownNodeKind::Strikethrough
                    | MarkdownNodeKind::InlineCode
                    | MarkdownNodeKind::Link
                    | MarkdownNodeKind::Image
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
                nodes.push(parse_plain_replacement_block(
                    node,
                    multi_snapshot,
                    MarkdownNodeKind::HorizontalRule,
                    None,
                ));
            }
            "pipe_table" => {
                nodes.push(parse_plain_replacement_block(
                    node,
                    multi_snapshot,
                    MarkdownNodeKind::Table,
                    Some(snapshot.chars_for_range(node.byte_range()).collect()),
                ));
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }
            "html_block" => {
                nodes.push(parse_plain_replacement_block(
                    node,
                    multi_snapshot,
                    MarkdownNodeKind::HtmlBlock,
                    Some(snapshot.chars_for_range(node.byte_range()).collect()),
                ));
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

fn parse_plain_replacement_block(
    node: Node<'_>,
    multi_snapshot: &MultiBufferSnapshot,
    kind: MarkdownNodeKind,
    preview_text: Option<String>,
) -> MarkdownDecoratorNode {
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    let cursor_anchor = full_range.start;
    MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        kind,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text,
        cursor_anchor: Some(cursor_anchor),
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
    let cursor_anchor = full_range.start;
    Some(MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        kind: MarkdownNodeKind::Paragraph,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: Some(preview_text),
        cursor_anchor: Some(cursor_anchor),
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
    let mut alt_text = String::new();
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
            MarkdownEvent::Text if in_image => {
                alt_text.push_str(&source[range.clone()]);
            }
            MarkdownEvent::SubstitutedText(text) if in_image => {
                alt_text.push_str(&text);
            }
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

    let image_url = image_destination?;
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        kind: MarkdownNodeKind::Image,
        background_range: None,
        image_url: Some(image_url),
        code_language: None,
        preview_text: (!alt_text.trim().is_empty()).then_some(alt_text.trim().to_string()),
        cursor_anchor: Some(cursor_anchor),
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
    let cursor_anchor = multi_snapshot.anchor_after(MultiBufferOffset(marker_end));
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
        kind: MarkdownNodeKind::Heading(level),
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: Some(preview_text.trim().to_string()),
        cursor_anchor: Some(cursor_anchor),
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
    let fallback_cursor_anchor = multi_snapshot.anchor_before(MultiBufferOffset(node_range.start));
    let mut preview_text = snapshot
        .chars_for_range(node_range.start..underline_range.start)
        .collect::<String>();
    let text_start = node_range.start
        + preview_text
            .len()
            .saturating_sub(preview_text.trim_start().len());
    preview_text = preview_text.trim().to_string();
    let cursor_anchor = if preview_text.is_empty() {
        fallback_cursor_anchor
    } else {
        multi_snapshot.anchor_after(MultiBufferOffset(text_start))
    };

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![byte_range_to_anchor_range(underline_range, multi_snapshot)],
        kind: MarkdownNodeKind::Heading(level),
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: Some(preview_text),
        cursor_anchor: Some(cursor_anchor),
    })
}

fn parse_task_list_marker(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let checked = node.kind() == "task_list_marker_checked";
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    let cursor_anchor = full_range.start;
    Some(MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        kind: MarkdownNodeKind::Checkbox { checked },
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
    })
}

fn parse_fenced_code_block(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let mut content_start: Option<usize> = None;
    let mut content_end: Option<usize> = None;
    let mut code_language: Option<String> = None;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            match child.kind() {
                "info_string" => {
                    let language = snapshot
                        .chars_for_range(child.byte_range())
                        .collect::<String>()
                        .trim()
                        .split_whitespace()
                        .next()
                        .map(str::to_string);
                    code_language = language;
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

    let (decorator_ranges, background_range, preview_text) =
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
            let bg = byte_range_to_anchor_range(start..end, multi_snapshot);
            let text = snapshot.chars_for_range(start..end).collect::<String>();
            (ranges, Some(bg), Some(text))
        } else {
            // No content node — fold the entire block, no background range.
            (
                vec![byte_range_to_anchor_range(
                    node_range.clone(),
                    multi_snapshot,
                )],
                None,
                None,
            )
        };

    if decorator_ranges.is_empty() {
        return None;
    }

    let full_range = byte_range_to_anchor_range(node_range, multi_snapshot);
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        kind: MarkdownNodeKind::FencedCode,
        background_range,
        image_url: None,
        code_language,
        preview_text,
        cursor_anchor: Some(cursor_anchor),
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

    // `block_quote_marker` and `block_continuation` include the following space when present.
    let decorator_ranges: Vec<Range<Anchor>> = marker_byte_ranges
        .iter()
        .map(|range| byte_range_to_anchor_range(range.clone(), multi_snapshot))
        .collect();

    let node_range = node.byte_range();
    let mut preview_text = snapshot
        .chars_for_range(node_range.clone())
        .collect::<String>();
    for marker_range in marker_byte_ranges.iter().rev() {
        let start = marker_range.start.saturating_sub(node_range.start);
        let end = marker_range.end.saturating_sub(node_range.start);
        if start <= end && end <= preview_text.len() {
            preview_text.replace_range(start..end, "");
        }
    }

    let full_range = byte_range_to_anchor_range(node_range.clone(), multi_snapshot);
    let background_range = Some(byte_range_to_anchor_range(node_range, multi_snapshot));
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        kind: MarkdownNodeKind::Blockquote,
        background_range,
        image_url: None,
        code_language: None,
        preview_text: Some(preview_text),
        cursor_anchor: Some(cursor_anchor),
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

    let kind = if has_nested_emphasis {
        MarkdownNodeKind::BoldItalic
    } else if is_strong {
        MarkdownNodeKind::Bold
    } else {
        MarkdownNodeKind::Italic
    };

    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        kind,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
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
    let cursor_anchor = full_range.start;
    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        kind: MarkdownNodeKind::Strikethrough,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
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
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        kind: MarkdownNodeKind::InlineCode,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
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
    let cursor_anchor = full_range.start;
    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![
            byte_range_to_anchor_range(node_range.start..node_range.start + 1, multi_snapshot),
            byte_range_to_anchor_range(node_range.end - 1..node_range.end, multi_snapshot),
        ],
        kind: MarkdownNodeKind::Link,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
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
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: vec![open_bracket, tail],
        kind: MarkdownNodeKind::Link,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
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
    let image_url: Option<String> = link_dest_range.as_ref().map(|dest_range| {
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
    let cursor_anchor = full_range.start;

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        kind: MarkdownNodeKind::Image,
        background_range: None,
        image_url,
        code_language: None,
        preview_text: None,
        cursor_anchor: Some(cursor_anchor),
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
    fn test_highlight_style_bold() {
        assert_eq!(MarkdownNodeKind::Bold, MarkdownNodeKind::Bold);
        assert_ne!(MarkdownNodeKind::Bold, MarkdownNodeKind::Italic);
    }

    #[test]
    fn test_node_kind_equality() {
        assert_eq!(MarkdownNodeKind::Heading(1), MarkdownNodeKind::Heading(1));
        assert_ne!(MarkdownNodeKind::Heading(1), MarkdownNodeKind::Heading(2));
        assert_eq!(
            MarkdownNodeKind::Checkbox { checked: true },
            MarkdownNodeKind::Checkbox { checked: true }
        );
        assert_ne!(
            MarkdownNodeKind::Checkbox { checked: true },
            MarkdownNodeKind::Checkbox { checked: false }
        );
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
    fn test_parse_list_items() {
        assert_eq!(
            parse_markdown_list_items("- one\n  - [x] nested\n2. two"),
            vec![
                RenderedMarkdownListItem {
                    indent_columns: 0,
                    marker: "•".to_string(),
                    text: "one".to_string(),
                },
                RenderedMarkdownListItem {
                    indent_columns: 2,
                    marker: "[x]".to_string(),
                    text: "nested".to_string(),
                },
                RenderedMarkdownListItem {
                    indent_columns: 0,
                    marker: "2.".to_string(),
                    text: "two".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_text_list_node_detection() {
        let text = "- one\n- two\n\nSome other text";
        let ranges = text_list_ranges(text, &[], &[]);
        assert_eq!(ranges, vec![(0..12, "- one\n- two".to_string())]);
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
}
