use std::any::TypeId;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, App, AppContext as _, AvailableSpace, ClickEvent, ElementId, Empty, Entity,
    Focusable, ImageSource, MouseButton, Resource, RetainAllImageCache, SharedUri, Task,
    WeakEntity, px, size,
};
use markdown::parser::{
    MarkdownLivePreviewBlock, MarkdownLivePreviewBlockKind, MarkdownLivePreviewInlineMarkerKind,
    MarkdownLivePreviewLayout, markdown_live_preview_layout,
};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
};
use settings::Settings;
use text::Point;
use theme_settings::ThemeSettings;
use ui::{Checkbox, Context, ToggleState, Window, div, prelude::*};
use util::ResultExt;

use crate::display_map::{Crease, FoldId, FoldPlaceholder};
use crate::{
    Anchor, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, Editor, EditorSettings,
    MultiBufferOffset, RenderBlock, SelectionEffects, ToOffset, ToPoint,
};

/// How long to wait after the last edit before re-parsing the document.
const REPARSE_DEBOUNCE: Duration = Duration::from_millis(50);
const IMAGE_FALLBACK_LINES: u32 = 8;

/// Type tag used to distinguish the live preview's folds from user folds.
struct MarkdownLivePreviewFold;

#[derive(Default)]
pub(crate) struct MarkdownLivePreviewState {
    rich_blocks: Vec<RichBlockState>,
    inline_markers: Vec<InlineMarkerState>,
    active_rows: Vec<Range<u32>>,
    parse_generation: usize,
    parse_task: Option<Task<()>>,
    image_cache: Option<Entity<RetainAllImageCache>>,
}

/// A block (image, table, code block, or rule) rendered as a replacement block.
/// The block is only inserted into the editor while no selection touches its rows.
struct RichBlockState {
    range: Range<Anchor>,
    block: MarkdownLivePreviewBlock,
    markdown: Entity<Markdown>,
    height: u32,
    block_id: Option<CustomBlockId>,
}

struct InlineMarkerState {
    range: Range<Anchor>,
    kind: MarkdownLivePreviewInlineMarkerKind,
}

impl Editor {
    /// Schedules a debounced re-parse of the buffer. Multiple calls within the
    /// debounce window coalesce into a single parse.
    pub(crate) fn schedule_markdown_live_preview_reparse(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.should_render_markdown_live_preview(cx) {
            self.remove_markdown_live_preview(cx);
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let executor = cx.background_executor().clone();
        let state = self.markdown_live_preview.get_or_insert_default();
        state.parse_generation += 1;
        let generation = state.parse_generation;
        state.parse_task = Some(cx.spawn_in(window, async move |editor, cx| {
            executor.timer(REPARSE_DEBOUNCE).await;
            let layout = cx
                .background_spawn(async move {
                    let text = snapshot.text();
                    markdown_live_preview_layout(&text)
                })
                .await;
            editor
                .update_in(cx, |editor, window, cx| {
                    let Some(state) = editor.markdown_live_preview.as_ref() else {
                        return;
                    };
                    if state.parse_generation != generation {
                        return;
                    }
                    editor.apply_markdown_live_preview_layout(layout, window, cx);
                })
                .log_err();
        }));
    }

    pub(crate) fn reconcile_markdown_live_preview_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.should_render_markdown_live_preview(cx) {
            self.remove_markdown_live_preview(cx);
            return;
        }

        let Some(state) = self.markdown_live_preview.as_ref() else {
            self.schedule_markdown_live_preview_reparse(window, cx);
            return;
        };
        let active_rows = self.active_markdown_live_preview_rows(cx);
        if state.active_rows == active_rows {
            return;
        }
        self.sync_markdown_live_preview(active_rows, window, cx);
    }

    fn apply_markdown_live_preview_layout(
        &mut self,
        layout: MarkdownLivePreviewLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let language_registry = self
            .project()
            .map(|project| project.read(cx).languages().clone());
        let mut state = self.markdown_live_preview.take().unwrap_or_default();

        // Anchors track edits, so a block whose anchors still resolve to the newly
        // parsed range with identical source can be reused, keeping its Markdown
        // entity and any inserted block.
        let mut blocks_to_remove = HashSet::default();
        let mut old_blocks: HashMap<(usize, usize), RichBlockState> = HashMap::default();
        for old_block in state.rich_blocks.drain(..) {
            let key = (
                old_block.range.start.to_offset(&snapshot).0,
                old_block.range.end.to_offset(&snapshot).0,
            );
            if let Some(collided) = old_blocks.insert(key, old_block)
                && let Some(block_id) = collided.block_id
            {
                blocks_to_remove.insert(block_id);
            }
        }

        let mut rich_blocks = Vec::with_capacity(layout.blocks.len());
        for block in layout.blocks {
            let key = (block.replacement_range.start, block.replacement_range.end);
            if let Some(old_block) = old_blocks.remove(&key) {
                if old_block.block.source == block.source && old_block.block.kind == block.kind {
                    rich_blocks.push(RichBlockState { block, ..old_block });
                    continue;
                }
                if let Some(block_id) = old_block.block_id {
                    blocks_to_remove.insert(block_id);
                }
            }

            let range = snapshot.anchor_before(MultiBufferOffset(block.replacement_range.start))
                ..snapshot.anchor_after(MultiBufferOffset(block.replacement_range.end));
            let markdown = cx.new(|cx| {
                Markdown::new_with_options(
                    block.source.clone(),
                    language_registry.clone(),
                    None,
                    MarkdownOptions {
                        parse_html: true,
                        parse_heading_slugs: false,
                        render_mermaid_diagrams: true,
                        ..Default::default()
                    },
                    cx,
                )
            });
            rich_blocks.push(RichBlockState {
                range,
                height: initial_block_height(&block),
                markdown,
                block,
                block_id: None,
            });
        }
        blocks_to_remove.extend(
            old_blocks
                .into_values()
                .filter_map(|old_block| old_block.block_id),
        );
        if !blocks_to_remove.is_empty() {
            self.remove_blocks(blocks_to_remove, None, cx);
        }
        state.rich_blocks = rich_blocks;

        // Bias the anchors inward so text typed at either edge of a marker is
        // not swallowed by the fold before the next parse lands.
        state.inline_markers = layout
            .inline_markers
            .into_iter()
            .map(|marker| InlineMarkerState {
                range: snapshot.anchor_after(MultiBufferOffset(marker.range.start))
                    ..snapshot.anchor_before(MultiBufferOffset(marker.range.end)),
                kind: marker.kind,
            })
            .collect();

        self.markdown_live_preview = Some(state);
        let active_rows = self.active_markdown_live_preview_rows(cx);
        self.sync_markdown_live_preview(active_rows, window, cx);
    }

    /// Inserts or removes blocks and folds so that everything outside the active
    /// rows is rendered, and everything on the active rows shows its source.
    fn sync_markdown_live_preview(
        &mut self,
        active_rows: Vec<Range<u32>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut state) = self.markdown_live_preview.take() else {
            return;
        };
        state.active_rows = active_rows.clone();
        let editor = cx.entity().downgrade();
        let display_snapshot = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let snapshot = display_snapshot.buffer_snapshot();
        let image_cache = state
            .image_cache
            .get_or_insert_with(|| RetainAllImageCache::new(cx))
            .clone();
        let base_directory = self
            .target_file_abs_path(cx)
            .and_then(|path| path.parent().map(Path::to_path_buf));

        let mut blocks_to_remove = HashSet::default();
        let mut blocks_to_insert = Vec::new();
        for (ix, rich_block) in state.rich_blocks.iter_mut().enumerate() {
            let start = rich_block.range.start.to_point(snapshot);
            let end = rich_block.range.end.to_point(snapshot);
            let visible =
                start != end && !rows_intersect(start.row..end.row.saturating_add(1), &active_rows);
            match (visible, rich_block.block_id) {
                (true, None) => blocks_to_insert.push((
                    ix,
                    BlockProperties {
                        placement: BlockPlacement::Replace(
                            rich_block.range.start..=rich_block.range.end,
                        ),
                        height: Some(rich_block.height),
                        style: BlockStyle::Spacer,
                        render: render_rich_block(
                            rich_block.markdown.clone(),
                            rich_block.range.start,
                            editor.clone(),
                            base_directory.clone(),
                            image_cache.clone(),
                        ),
                        priority: 0,
                    },
                )),
                (false, Some(block_id)) => {
                    blocks_to_remove.insert(block_id);
                    rich_block.block_id = None;
                }
                _ => {}
            }
        }
        if !blocks_to_remove.is_empty() {
            self.remove_blocks(blocks_to_remove, None, cx);
        }
        if !blocks_to_insert.is_empty() {
            let (indices, properties): (Vec<_>, Vec<_>) = blocks_to_insert.into_iter().unzip();
            let block_ids = self.insert_blocks(properties, None, cx);
            for (ix, block_id) in indices.into_iter().zip(block_ids) {
                if let Some(rich_block) = state.rich_blocks.get_mut(ix) {
                    rich_block.block_id = Some(block_id);
                }
            }
        }

        // Diff the desired folds against the folds actually present, so that user
        // actions like "unfold all" are repaired rather than fought.
        let type_id = TypeId::of::<MarkdownLivePreviewFold>();
        let existing_folds = display_snapshot
            .folds_in_range(MultiBufferOffset(0)..snapshot.len())
            .filter(|fold| fold.placeholder.type_tag == Some(type_id))
            .map(|fold| {
                (
                    fold.range.start.to_offset(snapshot).0,
                    fold.range.end.to_offset(snapshot).0,
                )
            })
            .collect::<HashSet<_>>();

        let mut desired_folds = HashMap::default();
        for marker in &state.inline_markers {
            let start = marker.range.start.to_offset(snapshot);
            let end = marker.range.end.to_offset(snapshot);
            if start >= end {
                continue;
            }
            let start_row = start.to_point(snapshot).row;
            let end_row = end.to_point(snapshot).row;
            if rows_intersect(start_row..end_row.saturating_add(1), &active_rows) {
                continue;
            }
            desired_folds.insert((start.0, end.0), marker);
        }

        let folds_to_remove = existing_folds
            .iter()
            .filter(|key| !desired_folds.contains_key(key))
            .map(|(start, end)| MultiBufferOffset(*start)..MultiBufferOffset(*end))
            .collect::<Vec<_>>();
        let creases = desired_folds
            .iter()
            .filter(|(key, _)| !existing_folds.contains(key))
            .map(|(_, marker)| {
                Crease::simple(
                    marker.range.clone(),
                    inline_marker_placeholder(marker.kind, editor.clone()),
                )
            })
            .collect::<Vec<_>>();

        if !folds_to_remove.is_empty() {
            self.remove_folds_with_type(&folds_to_remove, type_id, false, cx);
        }
        if !creases.is_empty() {
            self.fold_creases(creases, false, window, cx);
        }

        self.markdown_live_preview = Some(state);
    }

    fn set_markdown_live_preview_block_height(
        &mut self,
        block_id: CustomBlockId,
        height: u32,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.markdown_live_preview.as_mut() else {
            return;
        };
        let Some(rich_block) = state
            .rich_blocks
            .iter_mut()
            .find(|rich_block| rich_block.block_id == Some(block_id))
        else {
            return;
        };
        if rich_block.height == height {
            return;
        }
        rich_block.height = height;
        self.resize_blocks([(block_id, height)].into_iter().collect(), None, cx);
    }

    pub(crate) fn remove_markdown_live_preview(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.markdown_live_preview.take() else {
            return;
        };

        let block_ids = state
            .rich_blocks
            .into_iter()
            .filter_map(|rich_block| rich_block.block_id)
            .collect::<HashSet<_>>();
        if !block_ids.is_empty() {
            self.remove_blocks(block_ids, None, cx);
        }

        if !state.inline_markers.is_empty() {
            let snapshot = self.buffer.read(cx).snapshot(cx);
            self.remove_folds_with_type(
                &[MultiBufferOffset(0)..snapshot.len()],
                TypeId::of::<MarkdownLivePreviewFold>(),
                false,
                cx,
            );
        }
    }

    fn should_render_markdown_live_preview(&self, cx: &App) -> bool {
        if !self.mode.is_full() || !EditorSettings::get_global(cx).markdown.live_preview {
            return false;
        }

        let buffer = self.buffer.read(cx);
        let Some(singleton) = buffer.as_singleton() else {
            return false;
        };

        singleton
            .read(cx)
            .language()
            .is_some_and(|language| language.name() == "Markdown")
    }

    fn active_markdown_live_preview_rows(&self, cx: &mut Context<Self>) -> Vec<Range<u32>> {
        let display_snapshot = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let mut rows = self
            .selections
            .all::<Point>(&display_snapshot)
            .into_iter()
            .map(|selection| {
                let range = selection.range();
                let start = range.start.row.min(range.end.row);
                let end = range.start.row.max(range.end.row).saturating_add(1);
                start..end
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|range| (range.start, range.end));
        rows
    }
}

fn rows_intersect(rows: Range<u32>, active_rows: &[Range<u32>]) -> bool {
    active_rows
        .iter()
        .any(|active| rows.start < active.end && active.start < rows.end)
}

/// The height a block is given until it has been measured during rendering.
fn initial_block_height(block: &MarkdownLivePreviewBlock) -> u32 {
    let source_lines = block.source.lines().count().max(1) as u32;
    match block.kind {
        MarkdownLivePreviewBlockKind::Image => IMAGE_FALLBACK_LINES,
        MarkdownLivePreviewBlockKind::CodeBlock { is_indented, .. } => {
            if is_indented {
                source_lines.saturating_add(2)
            } else {
                source_lines
            }
        }
        MarkdownLivePreviewBlockKind::Table => source_lines,
        MarkdownLivePreviewBlockKind::Rule => 1,
        _ => source_lines,
    }
}

fn inline_marker_placeholder(
    kind: MarkdownLivePreviewInlineMarkerKind,
    editor: WeakEntity<Editor>,
) -> FoldPlaceholder {
    let render: Arc<dyn Fn(FoldId, Range<Anchor>, &mut App) -> AnyElement + Send + Sync> =
        match kind {
            MarkdownLivePreviewInlineMarkerKind::Hidden => {
                Arc::new(|_, _, _| Empty.into_any_element())
            }
            MarkdownLivePreviewInlineMarkerKind::Bullet => Arc::new(|_, _, cx| {
                div()
                    .font(ThemeSettings::get_global(cx).buffer_font.clone())
                    .text_color(cx.theme().colors().text_muted)
                    .child("•")
                    .into_any_element()
            }),
            MarkdownLivePreviewInlineMarkerKind::BlockQuote => Arc::new(|_, _, cx| {
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(3.))
                            .h_full()
                            .rounded_sm()
                            .bg(cx.theme().colors().border),
                    )
                    .into_any_element()
            }),
            MarkdownLivePreviewInlineMarkerKind::TaskListMarker { checked } => {
                Arc::new(move |fold_id, fold_range, _cx| {
                    let editor = editor.clone();
                    div()
                        .h_full()
                        .flex()
                        .items_center()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Checkbox::new(fold_id, ToggleState::from(checked))
                                .fill()
                                .on_click_ext(move |_, _, window, cx| {
                                    cx.stop_propagation();
                                    let Some(editor) = editor.upgrade() else {
                                        return;
                                    };
                                    editor.update(cx, |editor, cx| {
                                        toggle_task_list_marker(
                                            editor,
                                            fold_range.clone(),
                                            checked,
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                        )
                        .into_any_element()
                })
            }
        };

    FoldPlaceholder {
        render,
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<MarkdownLivePreviewFold>()),
        collapsed_text: None,
    }
}

/// Flips the `[ ]` / `[x]` marker inside a folded task list marker and re-folds it
/// with the new state so the checkbox updates before the next parse lands.
fn toggle_task_list_marker(
    editor: &mut Editor,
    fold_range: Range<Anchor>,
    checked: bool,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let snapshot = editor.buffer.read(cx).snapshot(cx);
    let start = fold_range.start.to_offset(&snapshot);
    let end = fold_range.end.to_offset(&snapshot);
    let text = snapshot.text_for_range(start..end).collect::<String>();
    let Some(marker_ix) = ["[ ]", "[x]", "[X]"]
        .into_iter()
        .find_map(|marker| text.find(marker))
    else {
        return;
    };
    let marker_range =
        MultiBufferOffset(start.0 + marker_ix)..MultiBufferOffset(start.0 + marker_ix + 3);
    let replacement = if checked { "[ ]" } else { "[x]" };

    let type_id = TypeId::of::<MarkdownLivePreviewFold>();
    editor.remove_folds_with_type(&[start..end], type_id, false, cx);
    editor.edit([(marker_range, replacement)], cx);
    let placeholder = inline_marker_placeholder(
        MarkdownLivePreviewInlineMarkerKind::TaskListMarker { checked: !checked },
        cx.entity().downgrade(),
    );
    editor.fold_creases(
        vec![Crease::simple(fold_range, placeholder)],
        false,
        window,
        cx,
    );
}

fn resolve_markdown_live_preview_image(
    dest_url: &str,
    base_directory: Option<&Path>,
) -> Option<ImageSource> {
    if dest_url.starts_with("data:") {
        return None;
    }

    if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
        return Some(ImageSource::Resource(Resource::Uri(SharedUri::from(
            dest_url.to_string(),
        ))));
    }

    let path = if Path::new(dest_url).is_absolute() {
        PathBuf::from(dest_url)
    } else {
        base_directory?.join(dest_url)
    };
    Some(ImageSource::Resource(Resource::Path(Arc::from(
        path.as_path(),
    ))))
}

fn render_rich_block(
    markdown: Entity<Markdown>,
    source_start: Anchor,
    editor: WeakEntity<Editor>,
    base_directory: Option<PathBuf>,
    image_cache: Entity<RetainAllImageCache>,
) -> RenderBlock {
    Arc::new(move |cx| {
        let right_padding = cx.margins.right + cx.em_width * 4.;
        let content_width = (cx.max_width - right_padding).max(cx.em_width);
        let block_id = cx.block_id;
        let line_height = cx.line_height;

        let build_markdown = |window: &mut Window, app: &mut App| -> AnyElement {
            let mut style = MarkdownStyle::themed(MarkdownFont::Preview, window, app);
            style.container_style.margin = gpui::EdgesRefinement::default();
            style.container_style.padding = gpui::EdgesRefinement::default();
            style.height_is_multiple_of_line_height = true;
            style.prevent_mouse_interaction = true;
            style.table_columns_min_size = false;

            MarkdownElement::new(markdown.clone(), style)
                .code_block_renderer(CodeBlockRenderer::Default {
                    copy_button_visibility: CopyButtonVisibility::Hidden,
                    wrap_button_visibility: WrapButtonVisibility::Hidden,
                    border: true,
                })
                .image_resolver({
                    let base_directory = base_directory.clone();
                    move |dest_url, _cx| {
                        resolve_markdown_live_preview_image(dest_url, base_directory.as_deref())
                    }
                })
                .on_source_click({
                    let editor = editor.clone();
                    move |source_offset, _, window, cx| {
                        let Some(editor) = editor.upgrade() else {
                            return false;
                        };
                        editor.update(cx, |editor, cx| {
                            let snapshot = editor.buffer.read(cx).snapshot(cx);
                            let offset = source_start.to_offset(&snapshot);
                            let point = snapshot
                                .offset_to_point(MultiBufferOffset(offset.0 + source_offset));
                            move_cursor_to(editor, point, window, cx);
                        });
                        true
                    }
                })
                .into_any_element()
        };

        // Blocks have a fixed height in lines, so measure the rendered content and
        // resize the block when the estimate was wrong. The resize is deferred
        // because we are in the middle of the editor's own layout pass.
        let mut probe = div()
            .id(("markdown-live-preview-measure", block_id_index(block_id)))
            .w(content_width)
            .child(build_markdown(cx.window, cx.app))
            .into_any_element();
        let measured = probe.layout_as_root(
            size(
                AvailableSpace::Definite(content_width),
                AvailableSpace::MinContent,
            ),
            cx.window,
            cx.app,
        );
        let measured_lines = (f32::from(measured.height) / f32::from(line_height))
            .ceil()
            .max(1.) as u32;
        if measured_lines != cx.height
            && let Some(custom_block_id) = custom_block_id(block_id)
        {
            let editor = editor.clone();
            cx.app.defer(move |cx| {
                editor
                    .update(cx, |editor, cx| {
                        editor.set_markdown_live_preview_block_height(
                            custom_block_id,
                            measured_lines,
                            cx,
                        );
                    })
                    .log_err();
            });
        }

        let markdown_element = build_markdown(cx.window, cx.app);
        let editor_for_click = editor.clone();
        div()
            .id(ElementId::from(block_id))
            .w(cx.max_width)
            .max_w_full()
            .min_w_0()
            .h((cx.height as f32) * line_height)
            .bg(cx.editor_style.background)
            .overflow_hidden()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_: &ClickEvent, window, cx| {
                let Some(editor) = editor_for_click.upgrade() else {
                    return;
                };
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer.read(cx).snapshot(cx);
                    let point = source_start.to_point(&snapshot);
                    move_cursor_to(editor, point, window, cx);
                });
            })
            .child(
                div()
                    .image_cache(image_cache.clone())
                    .w(content_width)
                    .min_w_0()
                    .overflow_x_hidden()
                    .child(markdown_element),
            )
            .into_any_element()
    })
}

fn move_cursor_to(
    editor: &mut Editor,
    point: Point,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    editor.change_selections(
        SelectionEffects::scroll(crate::scroll::Autoscroll::fit()),
        window,
        cx,
        |selections| selections.select_ranges([point..point]),
    );
    window.focus(&editor.focus_handle(cx), cx);
}

fn custom_block_id(block_id: crate::display_map::BlockId) -> Option<CustomBlockId> {
    match block_id {
        crate::display_map::BlockId::Custom(id) => Some(id),
        _ => None,
    }
}

fn block_id_index(block_id: crate::display_map::BlockId) -> usize {
    custom_block_id(block_id).map_or(0, |id| id.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tests::init_test;
    use crate::test::editor_test_context::EditorTestContext;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use language::markdown_lang;
    use settings::SettingsStore;

    fn set_live_preview(cx: &mut TestAppContext, enabled: bool) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, &|settings: &mut settings::SettingsContent| {
                    settings
                        .editor
                        .markdown
                        .get_or_insert_default()
                        .live_preview = Some(enabled);
                });
            });
        });
    }

    async fn markdown_test_context(cx: &mut TestAppContext) -> EditorTestContext {
        init_test(cx, |_| {});
        set_live_preview(cx, true);
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_lang()), cx));
        cx
    }

    fn settle(cx: &mut EditorTestContext) {
        cx.executor().advance_clock(REPARSE_DEBOUNCE * 2);
        cx.run_until_parked();
    }

    fn display_text(cx: &mut EditorTestContext) -> String {
        cx.update_editor(|editor, _, cx| editor.display_text(cx))
    }

    fn move_cursor(cx: &mut EditorTestContext, point: Point) {
        cx.update_editor(|editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([point..point])
            });
        });
        cx.run_until_parked();
    }

    fn inserted_block_count(cx: &mut EditorTestContext) -> usize {
        cx.update_editor(|editor, _, _| {
            editor.markdown_live_preview.as_ref().map_or(0, |state| {
                state
                    .rich_blocks
                    .iter()
                    .filter(|block| block.block_id.is_some())
                    .count()
            })
        })
    }

    #[gpui::test]
    async fn test_inline_markers_are_hidden_except_on_cursor_rows(cx: &mut TestAppContext) {
        let mut cx = markdown_test_context(cx).await;
        cx.set_state("# Titleˇ\n\nSome **bold** text\n\n- item\n");
        settle(&mut cx);

        assert_eq!(
            display_text(&mut cx),
            "# Title\n\nSome ⋯bold⋯ text\n\n⋯ item\n"
        );

        move_cursor(&mut cx, Point::new(2, 0));
        assert_eq!(
            display_text(&mut cx),
            "⋯Title\n\nSome **bold** text\n\n⋯ item\n"
        );

        move_cursor(&mut cx, Point::new(4, 2));
        assert_eq!(
            display_text(&mut cx),
            "⋯Title\n\nSome ⋯bold⋯ text\n\n- item\n"
        );
    }

    #[gpui::test]
    async fn test_rich_blocks_are_revealed_when_cursor_enters(cx: &mut TestAppContext) {
        let mut cx = markdown_test_context(cx).await;
        cx.set_state("Introˇ\n\n```rust\nlet x = 1;\n```\n\nOutro\n");
        settle(&mut cx);
        assert_eq!(inserted_block_count(&mut cx), 1);

        move_cursor(&mut cx, Point::new(3, 4));
        assert_eq!(inserted_block_count(&mut cx), 0);

        move_cursor(&mut cx, Point::new(6, 0));
        assert_eq!(inserted_block_count(&mut cx), 1);
    }

    #[gpui::test]
    async fn test_editing_above_a_block_reuses_it(cx: &mut TestAppContext) {
        let mut cx = markdown_test_context(cx).await;
        cx.set_state("Introˇ\n\n```rust\nlet x = 1;\n```\n");
        settle(&mut cx);

        let (entity_id, block_id) = cx.update_editor(|editor, _, _| {
            let block = &editor.markdown_live_preview.as_ref().unwrap().rich_blocks[0];
            (block.markdown.entity_id(), block.block_id)
        });
        assert!(block_id.is_some());

        cx.update_editor(|editor, window, cx| editor.handle_input(" edited", window, cx));
        settle(&mut cx);

        cx.update_editor(|editor, _, _| {
            let block = &editor.markdown_live_preview.as_ref().unwrap().rich_blocks[0];
            assert_eq!(block.markdown.entity_id(), entity_id);
            assert_eq!(block.block_id, block_id);
        });
        cx.assert_editor_state("Intro editedˇ\n\n```rust\nlet x = 1;\n```\n");
    }

    #[gpui::test]
    async fn test_toggling_task_list_marker_edits_buffer(cx: &mut TestAppContext) {
        let mut cx = markdown_test_context(cx).await;
        cx.set_state("ˇ\n- [ ] task\n");
        settle(&mut cx);
        assert_eq!(display_text(&mut cx), "\n⋯ task\n");

        let fold_range = cx.update_editor(|editor, _, _| {
            editor
                .markdown_live_preview
                .as_ref()
                .unwrap()
                .inline_markers[0]
                .range
                .clone()
        });
        cx.update_editor(|editor, window, cx| {
            toggle_task_list_marker(editor, fold_range, false, window, cx);
        });
        settle(&mut cx);

        cx.assert_editor_state("ˇ\n- [x] task\n");
        assert_eq!(display_text(&mut cx), "\n⋯ task\n");
    }

    #[gpui::test]
    async fn test_disabling_setting_removes_preview(cx: &mut TestAppContext) {
        let mut cx = markdown_test_context(cx).await;
        cx.set_state("ˇ\n# Title\n\n```\ncode\n```\n");
        settle(&mut cx);
        assert_eq!(inserted_block_count(&mut cx), 1);
        assert!(display_text(&mut cx).contains('⋯'));

        set_live_preview(&mut cx, false);
        settle(&mut cx);

        assert_eq!(display_text(&mut cx), "\n# Title\n\n```\ncode\n```\n");
        cx.update_editor(|editor, _, _| assert!(editor.markdown_live_preview.is_none()));
    }
}
