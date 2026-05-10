use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use collections::{HashMap, HashSet};
use gpui::{
    App, Asset, AssetLogger, ClickEvent, ElementId, Focusable, ImageAssetLoader, ImageSource,
    MouseButton, Resource, RetainAllImageCache, SharedUri, Task, WeakEntity,
};
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

const MIN_IMAGE_BLOCK_LINES: u32 = 4;
const IMAGE_FALLBACK_LINES: u32 = 24;

#[derive(Default)]
pub(crate) struct MarkdownLivePreviewState {
    blocks: Vec<MarkdownLivePreviewBlockState>,
    image_cache: Option<gpui::Entity<RetainAllImageCache>>,
    image_dimensions_by_url: HashMap<String, (u32, u32)>,
    pending_image_dimension_urls: HashSet<String>,
    image_blocks_by_url: HashMap<String, CustomBlockId>,
    image_layout: MarkdownLivePreviewImageLayout,
    _image_dimension_tasks: Vec<Task<()>>,
}

#[derive(Clone, Copy)]
struct MarkdownLivePreviewImageLayout {
    content_width: f32,
    line_height: f32,
}

impl Default for MarkdownLivePreviewImageLayout {
    fn default() -> Self {
        Self {
            content_width: 720.,
            line_height: 22.,
        }
    }
}

struct MarkdownLivePreviewBlockState {
    block_id: CustomBlockId,
    replacement_range: Range<usize>,
    source: String,
    image_destination: Option<String>,
}

impl Editor {
    pub(crate) fn reconcile_markdown_live_preview(
        &mut self,
        window: &mut Window,
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
        let language_registry = self
            .project()
            .map(|project| project.read(cx).languages().clone());
        let base_directory = self
            .target_file_abs_path(cx)
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let mut state = self.markdown_live_preview.take().unwrap_or_default();
        let image_cache = state
            .image_cache
            .get_or_insert_with(|| RetainAllImageCache::new(cx))
            .clone();
        state.image_layout = MarkdownLivePreviewImageLayout {
            content_width: self
                .last_position_map
                .as_ref()
                .map(|position_map| f32::from(position_map.text_hitbox.size.width).max(320.))
                .unwrap_or(720.),
            line_height: f32::from(self.style(cx).text.line_height_in_pixels(window.rem_size()))
                .max(1.),
        };
        state.image_blocks_by_url.clear();
        let image_dimensions_by_url = state.image_dimensions_by_url.clone();
        let image_layout = state.image_layout;
        let mut old_blocks = std::mem::take(&mut state.blocks)
            .into_iter()
            .map(|block| {
                (
                    live_preview_block_key(&block.replacement_range, &block.source),
                    block,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut retained_blocks = Vec::new();

        let mut block_properties = Vec::new();
        let mut new_block_metadata = Vec::new();
        for block in blocks {
            let start = snapshot.offset_to_point(MultiBufferOffset(block.replacement_range.start));
            let end = snapshot.offset_to_point(MultiBufferOffset(block.replacement_range.end));
            if start == end || rows_intersect(start.row..end.row.saturating_add(1), &active_rows) {
                continue;
            }

            let source = block.source.to_string();
            let key = live_preview_block_key(&block.replacement_range, &source);
            if let Some(old_block) = old_blocks.remove(&key) {
                retained_blocks.push(old_block);
                continue;
            }

            let start_anchor = snapshot.anchor_before(start);
            let end_anchor = snapshot.anchor_after(end);
            let replacement_range = block.replacement_range.clone();
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
            let image_url = image_dimension_url_for_block(&block, base_directory.as_deref());
            let height = block_height(
                &block,
                base_directory.as_deref(),
                image_layout,
                image_url
                    .as_deref()
                    .and_then(|url| image_dimensions_by_url.get(url).copied()),
            );
            let image_destination = block.image_destination.as_ref().map(ToString::to_string);
            block_properties.push(BlockProperties {
                placement: BlockPlacement::Replace(start_anchor..=end_anchor),
                height: Some(height),
                style: BlockStyle::Spacer,
                render: render_markdown_live_preview_block(
                    block,
                    markdown,
                    editor.clone(),
                    height,
                    image_url.clone(),
                    base_directory.clone(),
                    image_cache.clone(),
                ),
                priority: 0,
            });
            new_block_metadata.push((replacement_range, source, image_destination, image_url));
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
                |(block_id, (replacement_range, source, image_destination, image_url))| {
                    if let Some(image_url) = image_url {
                        state.image_blocks_by_url.insert(image_url, block_id);
                    }
                    MarkdownLivePreviewBlockState {
                        block_id,
                        replacement_range,
                        source,
                        image_destination,
                    }
                },
            ));
        }

        for block in &retained_blocks {
            if let Some(image_url) = block
                .image_destination
                .as_deref()
                .and_then(|destination| image_dimension_url(destination, base_directory.as_deref()))
            {
                state.image_blocks_by_url.insert(image_url, block.block_id);
            }
        }

        let image_block_heights = state
            .image_blocks_by_url
            .iter()
            .filter_map(|(image_url, block_id)| {
                state
                    .image_dimensions_by_url
                    .get(image_url)
                    .map(|dimensions| {
                        (
                            *block_id,
                            image_height_for_dimensions(*dimensions, image_layout),
                        )
                    })
            })
            .collect::<HashMap<_, _>>();
        if !image_block_heights.is_empty() {
            self.resize_blocks(image_block_heights, None, cx);
        }

        let image_urls = state
            .image_blocks_by_url
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for image_url in image_urls {
            if state.image_dimensions_by_url.contains_key(&image_url)
                || !state.pending_image_dimension_urls.insert(image_url.clone())
            {
                continue;
            }

            let Some(resource) = image_resource_for_dimension_url(&image_url) else {
                state.pending_image_dimension_urls.remove(&image_url);
                continue;
            };

            state
                ._image_dimension_tasks
                .push(cx.spawn(async move |editor, cx| {
                    let load = cx.update(|cx| {
                        let load = AssetLogger::<ImageAssetLoader>::load(resource, cx);
                        cx.background_spawn(load)
                    });
                    let image = load.await;
                    editor
                        .update(cx, |editor, cx| {
                            let Some(state) = editor.markdown_live_preview.as_mut() else {
                                return;
                            };
                            state.pending_image_dimension_urls.remove(&image_url);

                            let Ok(image) = image else {
                                return;
                            };
                            let size = image.size(0);
                            if size.width.0 <= 0 || size.height.0 <= 0 {
                                return;
                            }

                            let dimensions = (size.width.0 as u32, size.height.0 as u32);
                            state
                                .image_dimensions_by_url
                                .insert(image_url.clone(), dimensions);
                            let image_layout = state.image_layout;
                            let Some(block_id) = state.image_blocks_by_url.get(&image_url).copied()
                            else {
                                return;
                            };

                            editor.resize_blocks(
                                [(
                                    block_id,
                                    image_height_for_dimensions(dimensions, image_layout),
                                )]
                                .into_iter()
                                .collect(),
                                None,
                                cx,
                            );
                        })
                        .ok();
                }));
        }

        if retained_blocks.is_empty() {
            self.markdown_live_preview = Some(state);
            return;
        }

        state.blocks = retained_blocks;
        self.markdown_live_preview = Some(state);
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

fn block_height(
    block: &MarkdownLivePreviewBlock,
    base_directory: Option<&Path>,
    image_layout: MarkdownLivePreviewImageLayout,
    image_dimensions: Option<(u32, u32)>,
) -> u32 {
    let source_lines = block.source.lines().count().max(1) as u32;
    match block.kind {
        MarkdownLivePreviewBlockKind::Heading(1) => 2,
        MarkdownLivePreviewBlockKind::Heading(2) | MarkdownLivePreviewBlockKind::Heading(3) => 2,
        MarkdownLivePreviewBlockKind::Heading(_) => 2,
        MarkdownLivePreviewBlockKind::Image => image_dimensions.map_or_else(
            || image_block_height(block, base_directory, image_layout),
            |dimensions| image_height_for_dimensions(dimensions, image_layout),
        ),
        MarkdownLivePreviewBlockKind::Paragraph => source_lines,
        MarkdownLivePreviewBlockKind::BlockQuote => source_lines.max(2),
        MarkdownLivePreviewBlockKind::CodeBlock { .. } => code_block_height(block, source_lines),
        MarkdownLivePreviewBlockKind::Table => source_lines.saturating_add(1).max(3),
        MarkdownLivePreviewBlockKind::Rule => 1,
        _ => source_lines.max(1),
    }
}

fn code_block_height(block: &MarkdownLivePreviewBlock, source_lines: u32) -> u32 {
    if block.kind.is_indented_code_block() {
        source_lines.saturating_add(2).max(3)
    } else {
        source_lines.max(2)
    }
}

fn image_block_height(
    block: &MarkdownLivePreviewBlock,
    base_directory: Option<&Path>,
    _image_layout: MarkdownLivePreviewImageLayout,
) -> u32 {
    if block
        .image_destination
        .as_deref()
        .and_then(|destination| image_dimension_url(destination, base_directory))
        .is_some()
    {
        IMAGE_FALLBACK_LINES
    } else {
        MIN_IMAGE_BLOCK_LINES
    }
}

fn image_height_for_dimensions(
    (width, height): (u32, u32),
    image_layout: MarkdownLivePreviewImageLayout,
) -> u32 {
    if width == 0 || height == 0 {
        return 24;
    }

    let scale = (image_layout.content_width / width as f32).min(1.);
    let rendered_height = height as f32 * scale;
    ((rendered_height + image_layout.line_height) / image_layout.line_height)
        .ceil()
        .max(MIN_IMAGE_BLOCK_LINES as f32) as u32
}

fn image_dimension_url_for_block(
    block: &MarkdownLivePreviewBlock,
    base_directory: Option<&Path>,
) -> Option<String> {
    image_dimension_url(block.image_destination.as_deref()?, base_directory)
}

fn image_dimension_url(destination: &str, base_directory: Option<&Path>) -> Option<String> {
    if destination.starts_with("data:") {
        return None;
    }

    if destination.starts_with("http://") || destination.starts_with("https://") {
        return Some(destination.to_string());
    }

    resolve_markdown_live_preview_image_path(destination, base_directory)
        .map(|path| path.to_string_lossy().into_owned())
}

fn image_resource_for_dimension_url(url: &str) -> Option<Resource> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(Resource::Uri(SharedUri::from(url.to_string())))
    } else {
        Some(Resource::Path(Arc::from(Path::new(url))))
    }
}

fn resolve_markdown_live_preview_image_path(
    dest_url: &str,
    base_directory: Option<&Path>,
) -> Option<PathBuf> {
    if dest_url.starts_with("data:")
        || dest_url.starts_with("http://")
        || dest_url.starts_with("https://")
    {
        return None;
    }

    let path = if Path::new(dest_url).is_absolute() {
        PathBuf::from(dest_url)
    } else {
        base_directory?.join(dest_url)
    };

    path.exists().then_some(path)
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

    let path = resolve_markdown_live_preview_image_path(dest_url, base_directory)?;
    Some(ImageSource::Resource(Resource::Path(Arc::from(
        path.as_path(),
    ))))
}

fn render_markdown_live_preview_block(
    block: MarkdownLivePreviewBlock,
    markdown: gpui::Entity<Markdown>,
    editor: WeakEntity<Editor>,
    height: u32,
    image_url: Option<String>,
    base_directory: Option<PathBuf>,
    image_cache: gpui::Entity<RetainAllImageCache>,
) -> RenderBlock {
    let element_id = ElementId::from(block.source_range.start);
    let is_mermaid_code_block = block.kind.is_mermaid_code_block();
    Arc::new(move |cx| {
        let editor_for_click = editor.clone();
        let source_range_for_click = block.source_range.clone();
        let right_padding = cx.margins.right + cx.em_width * 4.;
        let content_width = (cx.max_width - right_padding).max(cx.em_width);
        let height = if matches!(block.kind, MarkdownLivePreviewBlockKind::Image) {
            image_url
                .as_ref()
                .and_then(|image_url| {
                    editor.upgrade().and_then(|editor| {
                        editor
                            .read(cx.app)
                            .markdown_live_preview
                            .as_ref()
                            .and_then(|state| state.image_dimensions_by_url.get(image_url).copied())
                    })
                })
                .map_or(height, |dimensions| {
                    image_height_for_dimensions(
                        dimensions,
                        MarkdownLivePreviewImageLayout {
                            content_width: f32::from(content_width),
                            line_height: f32::from(cx.line_height),
                        },
                    )
                })
        } else {
            height
        };
        div()
            .id(element_id.clone())
            .w(cx.max_width)
            .max_w_full()
            .min_w_0()
            .bg(cx.editor_style.background)
            .overflow_x_hidden()
            .when(!is_mermaid_code_block, |element| {
                element
                    .h((height as f32) * cx.line_height)
                    .overflow_hidden()
            })
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
            .child(
                div()
                    .image_cache(image_cache.clone())
                    .w(content_width)
                    .min_w_0()
                    .overflow_x_hidden()
                    .child({
                        let mut style =
                            MarkdownStyle::themed(MarkdownFont::Preview, cx.window, cx.app);
                        style.container_style.margin = gpui::EdgesRefinement::default();
                        style.container_style.padding = gpui::EdgesRefinement::default();
                        style.height_is_multiple_of_line_height = true;
                        style.prevent_mouse_interaction = true;
                        style.table_columns_min_size = false;

                        MarkdownElement::new(markdown.clone(), style)
                            .code_block_renderer(CodeBlockRenderer::Default {
                                copy_button_visibility: CopyButtonVisibility::Hidden,
                                border: true,
                            })
                            .image_resolver({
                                let base_directory = base_directory.clone();
                                move |dest_url| {
                                    resolve_markdown_live_preview_image(
                                        dest_url,
                                        base_directory.as_deref(),
                                    )
                                }
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
                                            SelectionEffects::scroll(
                                                crate::scroll::Autoscroll::fit(),
                                            ),
                                            window,
                                            cx,
                                            |selections| selections.select_ranges([point..point]),
                                        );
                                        window.focus(&editor.focus_handle(cx), cx);
                                    });
                                    true
                                }
                            })
                            .on_checkbox_toggle({
                                let editor = editor.clone();
                                let source_start = block.source_range.start;
                                move |source_range, new_checked, _window, cx| {
                                    let Some(editor) = editor.upgrade() else {
                                        return;
                                    };

                                    editor.update(cx, |editor, cx| {
                                        let source_range = source_start + source_range.start
                                            ..source_start + source_range.end;
                                        let existing_marker: String = editor
                                            .buffer()
                                            .read(cx)
                                            .snapshot(cx)
                                            .text_for_range(
                                                MultiBufferOffset(source_range.start)
                                                    ..MultiBufferOffset(source_range.end),
                                            )
                                            .collect();
                                        let replacement = if new_checked { "[x]" } else { "[ ]" };

                                        if matches!(existing_marker.as_str(), "[ ]" | "[x]" | "[X]")
                                        {
                                            editor.edit(
                                                [(
                                                    MultiBufferOffset(source_range.start)
                                                        ..MultiBufferOffset(source_range.end),
                                                    replacement,
                                                )],
                                                cx,
                                            );
                                        }
                                    });
                                }
                            })
                    }),
            )
            .into_any_element()
    })
}
