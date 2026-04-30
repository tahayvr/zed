use std::any::TypeId;
use std::ops::Range;
use std::sync::Arc;

use collections::HashSet;
use futures::FutureExt;
use gpui::{
    App, Context, FontStyle, FontWeight, HighlightStyle, ImageSource, IntoElement, ParentElement,
    Styled, StyledImage, StyledText, Task, WeakEntity, Window, div, px,
};
use gpui::prelude::InteractiveElement as _;
use language::{BufferSnapshot, Language, LanguageName, Node, TreeCursor};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset};
use settings::Settings;
use theme::ActiveTheme;

use crate::display_map::{
    BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId, FoldPlaceholder,
    HighlightKey, RenderBlock,
};
use crate::editor_settings::EditorSettings;
use crate::Editor;

/// Fallback editor-line height for image previews when dimensions are unavailable.
const IMAGE_BLOCK_FALLBACK_HEIGHT_LINES: u32 = 18;
const IMAGE_BLOCK_MIN_HEIGHT_LINES: u32 = 6;
const IMAGE_BLOCK_MAX_HEIGHT_LINES: u32 = 32;
const IMAGE_BLOCK_MAX_WIDTH_PX: f32 = 900.0;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownNodeKind {
    Heading(u8),
    Bold,
    Italic,
    BoldItalic,
    InlineCode,
    Link,
    Image,
    Checkbox { checked: bool },
    FencedCode,
    Blockquote,
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

    // Remove any non-typed folds sitting at our decorator positions. This handles folds
    // that were incorrectly persisted to the DB by an older version of the code and
    // subsequently restored on file open (those have no type_tag and thus survive
    // remove_folds_with_type below).
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

    apply_blocks(editor, &nodes, &cursor_offsets, &multi_snapshot, cx);
}

fn uses_replacement_preview(kind: MarkdownNodeKind) -> bool {
    matches!(
        kind,
        MarkdownNodeKind::Heading(_)
            | MarkdownNodeKind::Image
            | MarkdownNodeKind::FencedCode
            | MarkdownNodeKind::Blockquote
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
    editor.clear_highlights_with(&mut |key| matches!(key, HighlightKey::MarkdownLivePreview(_)), cx);
    editor.clear_background_highlights(HighlightKey::MarkdownLivePreviewBackground, cx);

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

    editor.clear_highlights_with(&mut |key| matches!(key, HighlightKey::MarkdownLivePreview(_)), cx);
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

    for node in nodes {
        let node_start = node.full_range.start.to_offset(multi_snapshot).0;
        let node_end = node.full_range.end.to_offset(multi_snapshot).0;
        let cursor_inside = cursor_offsets
            .iter()
            .any(|&offset| offset >= node_start && offset <= node_end);
        if cursor_inside {
            continue;
        }

        match node.kind {
            MarkdownNodeKind::Heading(level) => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(heading_block_height(level)),
                    style: BlockStyle::Flex,
                    render: render_heading_block(text, level, node.full_range.start, weak_editor.clone()),
                    priority: 0,
                });
            }
            MarkdownNodeKind::Image => {
                let Some(url_text) = node.image_url.as_deref() else {
                    continue;
                };
                let resolved_url = resolve_image_url(url_text, base_dir.as_deref());
                let image_height = image_block_height_lines(&resolved_url);
                let render = render_image_block(resolved_url, node.full_range.start, weak_editor.clone());
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(image_height),
                    style: BlockStyle::Flex,
                    render,
                    priority: 0,
                });
            }
            MarkdownNodeKind::FencedCode => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = preview_text_height(&text, 2);
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
                    render: render_code_block(text, language, node.full_range.start, weak_editor.clone()),
                    priority: 0,
                });
            }
            MarkdownNodeKind::Blockquote => {
                let Some(text) = node.preview_text.clone() else {
                    continue;
                };
                let height = preview_text_height(&text, 1);
                new_blocks.push(BlockProperties {
                    placement: BlockPlacement::Replace(node.full_range.start..=node.full_range.end),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: render_blockquote_block(text, node.full_range.start, weak_editor.clone()),
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

fn preview_text_height(text: &str, extra_lines: u32) -> u32 {
    text.lines().count().max(1) as u32 + extra_lines
}

fn heading_block_height(level: u8) -> u32 {
    match level {
        1 | 2 => 2,
        _ => 1,
    }
}

fn image_block_height_lines(resolved_url: &str) -> u32 {
    if resolved_url.contains("://") {
        return IMAGE_BLOCK_FALLBACK_HEIGHT_LINES;
    }

    let Ok((width, height)) = image::image_dimensions(resolved_url) else {
        return IMAGE_BLOCK_FALLBACK_HEIGHT_LINES;
    };
    if width == 0 || height == 0 {
        return IMAGE_BLOCK_FALLBACK_HEIGHT_LINES;
    }

    let rendered_width = (width as f32).min(IMAGE_BLOCK_MAX_WIDTH_PX);
    let rendered_height = rendered_width * height as f32 / width as f32;
    let estimated_line_height = 20.0;
    ((rendered_height / estimated_line_height).ceil() as u32 + 1)
        .clamp(IMAGE_BLOCK_MIN_HEIGHT_LINES, IMAGE_BLOCK_MAX_HEIGHT_LINES)
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
    source_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let resolved_url: Arc<str> = resolved_url.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        if resolved_url.is_empty() {
            return gpui::Empty.into_any_element();
        }
        // Use URI source for http(s) URLs; path source for everything else.
        let source: ImageSource = if resolved_url.contains("://") {
            ImageSource::from(resolved_url.to_string())
        } else {
            ImageSource::from(std::path::PathBuf::from(resolved_url.as_ref()))
        };
        let max_width = cx.max_width.min(px(IMAGE_BLOCK_MAX_WIDTH_PX));
        let mut container = div()
            .ml(cx.anchor_x)
            .w_full()
            .max_w(max_width)
            .h_full()
            .py_0p5();
        attach_source_click_handler(&mut container, source_anchor, weak_editor.clone());
        container
            .child(gpui::img(source).object_fit(gpui::ObjectFit::Contain).w_full().h_full())
            .into_any_element()
    })
}

fn render_code_block(
    text: String,
    language: Option<Arc<Language>>,
    source_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let text_style = cx.editor_style.text.clone();
        let mut container = div()
            .ml(cx.anchor_x)
            .max_w(cx.max_width)
            .w_full()
            .h_full()
            .px_2()
            .py_1()
            .rounded_lg()
            .bg(colors.editor_background)
            .border_1()
            .border_color(colors.border_variant)
            .font(cx.editor_style.text.font())
            .text_color(colors.text)
            .line_height(cx.line_height);
        attach_source_click_handler(&mut container, source_anchor, weak_editor.clone());
        container
            .children(code_lines(text.as_ref(), language.as_ref(), &text_style, cx.app))
            .into_any_element()
    })
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
                .h_5()
                .child(StyledText::new(line.to_string()).with_default_highlights(text_style, highlights))
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
    source_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let mut element = div()
            .ml(cx.anchor_x)
            .max_w(cx.max_width)
            .w_full()
            .h_full()
            .font(cx.editor_style.text.font())
            .text_color(colors.text)
            .font_weight(FontWeight::BOLD)
            .line_height(cx.line_height)
            .child(text.as_ref().to_string());
        attach_source_click_handler(&mut element, source_anchor, weak_editor.clone());

        match level {
            1 => element.text_3xl(),
            2 => element.text_2xl(),
            3 => element.text_xl(),
            4 => element.text_lg(),
            5 => element.text_base(),
            _ => element.text_sm(),
        }
        .into_any_element()
    })
}

fn render_blockquote_block(
    text: String,
    source_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) -> RenderBlock {
    let text: Arc<str> = text.into();
    Arc::new(move |cx: &mut crate::display_map::BlockContext| {
        let colors = cx.app.theme().colors();
        let mut container = div()
            .ml(cx.anchor_x)
            .max_w(cx.max_width)
            .w_full()
            .h_full()
            .pl_4()
            .border_l_4()
            .border_color(colors.border)
            .font(cx.editor_style.text.font())
            .text_color(colors.text_muted)
            .line_height(cx.line_height);
        attach_source_click_handler(&mut container, source_anchor, weak_editor.clone());
        container
            .children(text_lines(text.as_ref()))
            .into_any_element()
    })
}

fn attach_source_click_handler(
    element: &mut gpui::Div,
    source_anchor: Anchor,
    weak_editor: WeakEntity<Editor>,
) {
    element.interactivity().on_click(move |_, window, cx| {
        let _ = weak_editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([source_anchor..source_anchor]);
            });
        });
    });
}

fn text_lines(text: &str) -> Vec<gpui::AnyElement> {
    text.lines()
        .map(|line| div().h_5().child(line.to_string()).into_any_element())
        .collect()
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
        MarkdownNodeKind::FencedCode => Some(HighlightStyle {
            background_color: Some(colors.element_background),
            ..Default::default()
        }),
        MarkdownNodeKind::Blockquote => Some(HighlightStyle {
            color: Some(colors.text_muted),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        }),
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

    remove_inline_nodes_inside_fenced_code(nodes, multi_snapshot)
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
                if let Some(decorator) =
                    parse_emphasis_node(node, snapshot, multi_snapshot, false)
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
            "inline_link" => {
                if let Some(decorator) = parse_inline_link(node, snapshot, multi_snapshot) {
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
        kind: MarkdownNodeKind::Heading(level),
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: Some(preview_text.trim().to_string()),
    })
}

fn parse_task_list_marker(
    node: Node<'_>,
    _snapshot: &BufferSnapshot,
    multi_snapshot: &MultiBufferSnapshot,
) -> Option<MarkdownDecoratorNode> {
    let checked = node.kind() == "task_list_marker_checked";
    let full_range = byte_range_to_anchor_range(node.byte_range(), multi_snapshot);
    Some(MarkdownDecoratorNode {
        full_range: full_range.clone(),
        decorator_ranges: vec![full_range],
        kind: MarkdownNodeKind::Checkbox { checked },
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
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

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        kind: MarkdownNodeKind::FencedCode,
        background_range,
        image_url: None,
        code_language,
        preview_text,
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

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        kind: MarkdownNodeKind::Blockquote,
        background_range,
        image_url: None,
        code_language: None,
        preview_text: Some(preview_text),
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
                    delimiter_ranges
                        .push(byte_range_to_anchor_range(child.byte_range(), multi_snapshot));
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

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges: delimiter_ranges,
        kind,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
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
                delimiter_ranges
                    .push(byte_range_to_anchor_range(child.byte_range(), multi_snapshot));
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
        kind: MarkdownNodeKind::InlineCode,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
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
        kind: MarkdownNodeKind::Link,
        background_range: None,
        image_url: None,
        code_language: None,
        preview_text: None,
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

    let decorator_ranges = if let (Some(desc), Some(dest)) =
        (image_desc_range.clone(), link_dest_range)
    {
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
        vec![byte_range_to_anchor_range(node_range.clone(), multi_snapshot)]
    };

    let full_range = byte_range_to_anchor_range(node_range, multi_snapshot);

    Some(MarkdownDecoratorNode {
        full_range,
        decorator_ranges,
        kind: MarkdownNodeKind::Image,
        background_range: None,
        image_url,
        code_language: None,
        preview_text: None,
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
            image_block_height_lines("https://example.com/image.png"),
            IMAGE_BLOCK_FALLBACK_HEIGHT_LINES
        );
    }

    #[test]
    fn test_image_block_height_uses_fallback_for_missing_local_images() {
        assert_eq!(
            image_block_height_lines("/tmp/definitely-missing-image.png"),
            IMAGE_BLOCK_FALLBACK_HEIGHT_LINES
        );
    }
}
