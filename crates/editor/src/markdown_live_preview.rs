use std::ops::Range;
use std::sync::Arc;

use collections::{HashMap, HashSet};
use gpui::{App, ClickEvent, ElementId, Focusable, MouseButton, WeakEntity};
use markdown::parser::{
    MarkdownLivePreviewBlock, MarkdownLivePreviewBlockKind, markdown_live_preview_blocks,
};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle,
};
use settings::Settings;
use text::Point;
use ui::{Context, Window, div, prelude::*};

use crate::{
    BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, Editor, EditorSettings,
    MultiBufferOffset, RenderBlock, SelectionEffects,
};

#[derive(Default)]
pub(crate) struct MarkdownLivePreviewState {
    blocks: Vec<MarkdownLivePreviewBlockState>,
}

struct MarkdownLivePreviewBlockState {
    block_id: CustomBlockId,
    source_range: Range<usize>,
    source: String,
}

impl Editor {
    pub(crate) fn reconcile_markdown_live_preview(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.should_render_markdown_live_preview(cx) {
            self.remove_markdown_live_preview_blocks(cx);
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let text = snapshot.text();
        let active_rows = self.active_markdown_live_preview_rows(cx);
        let editor = cx.entity().downgrade();
        let blocks = markdown_live_preview_blocks(&text);
        let mut old_blocks = self
            .markdown_live_preview
            .take()
            .unwrap_or_default()
            .blocks
            .into_iter()
            .map(|block| {
                (
                    live_preview_block_key(&block.source_range, &block.source),
                    block,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut retained_blocks = Vec::new();

        let mut block_properties = Vec::new();
        let mut new_block_metadata = Vec::new();
        for block in blocks {
            let start = snapshot.offset_to_point(MultiBufferOffset(block.source_range.start));
            let end = snapshot.offset_to_point(MultiBufferOffset(block.source_range.end));
            if start == end || rows_intersect(start.row..end.row.saturating_add(1), &active_rows) {
                continue;
            }

            let source = block.source.to_string();
            let key = live_preview_block_key(&block.source_range, &source);
            if let Some(old_block) = old_blocks.remove(&key) {
                retained_blocks.push(old_block);
                continue;
            }

            let start_anchor = snapshot.anchor_before(start);
            let end_anchor = snapshot.anchor_after(end);
            let source_range = block.source_range.clone();
            let markdown = cx.new(|cx| {
                Markdown::new_with_options(
                    block.source.clone(),
                    None,
                    None,
                    MarkdownOptions {
                        parse_html: true,
                        parse_heading_slugs: false,
                        render_mermaid_diagrams: false,
                        ..Default::default()
                    },
                    cx,
                )
            });
            let height = block_height(&block);
            block_properties.push(BlockProperties {
                placement: BlockPlacement::Replace(start_anchor..=end_anchor),
                height: Some(height),
                style: BlockStyle::Spacer,
                render: render_markdown_live_preview_block(block, markdown, editor.clone(), height),
                priority: 0,
            });
            new_block_metadata.push((source_range, source));
        }

        let blocks_to_remove = old_blocks
            .into_values()
            .map(|block| block.block_id)
            .collect::<HashSet<_>>();
        if !blocks_to_remove.is_empty() {
            self.remove_blocks(blocks_to_remove, None, cx);
        }

        if !block_properties.is_empty() {
            let block_ids = self.insert_blocks(block_properties, None, cx);
            retained_blocks.extend(block_ids.into_iter().zip(new_block_metadata).map(
                |(block_id, (source_range, source))| MarkdownLivePreviewBlockState {
                    block_id,
                    source_range,
                    source,
                },
            ));
        }

        if retained_blocks.is_empty() {
            return;
        }

        let state = self
            .markdown_live_preview
            .get_or_insert_with(Default::default);
        state.blocks = retained_blocks;
    }

    pub(crate) fn remove_markdown_live_preview_blocks(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.markdown_live_preview.take() else {
            return;
        };

        let block_ids = state
            .blocks
            .into_iter()
            .map(|block| block.block_id)
            .collect::<HashSet<_>>();
        if !block_ids.is_empty() {
            self.remove_blocks(block_ids, None, cx);
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
        self.selections
            .all::<Point>(&display_snapshot)
            .into_iter()
            .map(|selection| {
                let range = selection.range();
                let start = range.start.row.min(range.end.row);
                let end = range.start.row.max(range.end.row).saturating_add(1);
                start..end
            })
            .collect()
    }
}

fn rows_intersect(rows: Range<u32>, active_rows: &[Range<u32>]) -> bool {
    active_rows
        .iter()
        .any(|active| rows.start < active.end && active.start < rows.end)
}

fn live_preview_block_key(source_range: &Range<usize>, source: &str) -> (usize, usize, String) {
    (source_range.start, source_range.end, source.to_string())
}

fn block_height(block: &MarkdownLivePreviewBlock) -> u32 {
    let source_lines = block.source.lines().count().max(1) as u32;
    match block.kind {
        MarkdownLivePreviewBlockKind::Heading(1) => 3,
        MarkdownLivePreviewBlockKind::Heading(_) => 2,
        MarkdownLivePreviewBlockKind::Paragraph => source_lines.saturating_add(1),
        MarkdownLivePreviewBlockKind::BlockQuote => source_lines.saturating_add(1).max(2),
        MarkdownLivePreviewBlockKind::CodeBlock => source_lines.saturating_add(1).max(3),
        MarkdownLivePreviewBlockKind::Table => source_lines.saturating_add(2).max(3),
        MarkdownLivePreviewBlockKind::Rule => 1,
        _ => source_lines.max(1),
    }
}

fn render_markdown_live_preview_block(
    block: MarkdownLivePreviewBlock,
    markdown: gpui::Entity<Markdown>,
    editor: WeakEntity<Editor>,
    height: u32,
) -> RenderBlock {
    let element_id = ElementId::from(block.source_range.start);
    Arc::new(move |cx| {
        let editor_for_click = editor.clone();
        let source_range_for_click = block.source_range.clone();
        let right_padding = cx.margins.right + cx.em_width * 4.;
        let content_width = (cx.max_width - right_padding).max(cx.em_width);
        div()
            .id(element_id.clone())
            .w(cx.max_width)
            .max_w_full()
            .min_w_0()
            .h((height as f32) * cx.line_height)
            .py_0p5()
            .overflow_x_hidden()
            .overflow_hidden()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_: &ClickEvent, window, cx| {
                let Some(editor) = editor_for_click.upgrade() else {
                    return;
                };
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer.read(cx).snapshot(cx);
                    let point =
                        snapshot.offset_to_point(MultiBufferOffset(source_range_for_click.start));
                    editor.change_selections(
                        SelectionEffects::scroll(crate::scroll::Autoscroll::fit()),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([point..point]);
                        },
                    );
                    window.focus(&editor.focus_handle(cx), cx);
                });
            })
            .child(div().w(content_width).min_w_0().overflow_x_hidden().child({
                let mut style = MarkdownStyle::themed(MarkdownFont::Preview, cx.window, cx.app);
                style.container_style.margin = gpui::EdgesRefinement::default();
                style.container_style.padding = gpui::EdgesRefinement::default();
                style.code_block.margin = gpui::EdgesRefinement::default();
                style.heading.margin = gpui::EdgesRefinement::default();

                MarkdownElement::new(markdown.clone(), style)
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        border: true,
                    })
                    .on_source_click({
                        let editor = editor.clone();
                        let source_start = block.source_range.start;
                        move |source_offset, _, window, cx| {
                            let Some(editor) = editor.upgrade() else {
                                return false;
                            };
                            editor.update(cx, |editor, cx| {
                                let snapshot = editor.buffer.read(cx).snapshot(cx);
                                let point = snapshot.offset_to_point(MultiBufferOffset(
                                    source_start + source_offset,
                                ));
                                editor.change_selections(
                                    SelectionEffects::scroll(crate::scroll::Autoscroll::fit()),
                                    window,
                                    cx,
                                    |selections| selections.select_ranges([point..point]),
                                );
                                window.focus(&editor.focus_handle(cx), cx);
                            });
                            true
                        }
                    })
            }))
            .into_any_element()
    })
}
