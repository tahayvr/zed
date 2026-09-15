use collections::{BTreeMap, HashMap, HashSet};
use gpui::SharedString;
use linkify::LinkFinder;
pub use pulldown_cmark::TagEnd as MarkdownTagEnd;
use pulldown_cmark::{
    Alignment, CowStr, HeadingLevel, LinkType, MetadataBlockKind, Options, Parser,
};
use std::{ops::Range, sync::Arc};
use util::markdown::generate_heading_slug;

use crate::{html, path_range::PathWithRange};

pub const PARSE_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_SMART_PUNCTUATION)
    .union(Options::ENABLE_HEADING_ATTRIBUTES)
    .union(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS)
    .union(Options::ENABLE_OLD_FOOTNOTES)
    .union(Options::ENABLE_GFM)
    .union(Options::ENABLE_SUPERSCRIPT)
    .union(Options::ENABLE_SUBSCRIPT);

#[derive(Default)]
struct ParseState {
    events: Vec<(Range<usize>, MarkdownEvent)>,
    root_block_starts: Vec<usize>,
    depth: usize,
}

#[derive(Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct ParsedMarkdownData {
    pub events: Vec<(Range<usize>, MarkdownEvent)>,
    pub language_names: HashSet<SharedString>,
    pub language_paths: HashSet<Arc<str>>,
    pub root_block_starts: Vec<usize>,
    pub html_blocks: BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    pub metadata_blocks: BTreeMap<usize, ParsedMetadataBlock>,
    pub heading_slugs: HashMap<SharedString, usize>,
    pub footnote_definitions: HashMap<SharedString, usize>,
    /// Source spans of link reference definitions (`[id]: https://example.com`), which are
    /// consumed by the parser and never appear in any event range.
    pub link_definition_spans: Vec<Range<usize>>,
    pub has_untagged_code_block: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParsedMetadataBlock {
    pub content_range: Range<usize>,
    pub rows: Option<Vec<MetadataRow>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetadataRow {
    pub key: Range<usize>,
    pub value: Range<usize>,
}

impl ParseState {
    fn push_event(&mut self, range: Range<usize>, event: MarkdownEvent) {
        match &event {
            MarkdownEvent::Start(_) => {
                if self.depth == 0 {
                    self.root_block_starts.push(range.start);
                    self.events.push((range.clone(), MarkdownEvent::RootStart));
                }
                self.depth += 1;
                self.events.push((range, event));
            }
            MarkdownEvent::End(_) => {
                self.events.push((range.clone(), event));
                if self.depth > 0 {
                    self.depth -= 1;
                    if self.depth == 0 {
                        let root_block_index = self.root_block_starts.len() - 1;
                        self.events
                            .push((range, MarkdownEvent::RootEnd(root_block_index)));
                    }
                }
            }
            MarkdownEvent::Rule => {
                if self.depth == 0 && !range.is_empty() {
                    self.root_block_starts.push(range.start);
                    let root_block_index = self.root_block_starts.len() - 1;
                    self.events.push((range.clone(), MarkdownEvent::RootStart));
                    self.events.push((range.clone(), event));
                    self.events
                        .push((range, MarkdownEvent::RootEnd(root_block_index)));
                } else {
                    self.events.push((range, event));
                }
            }
            _ => {
                self.events.push((range, event));
            }
        }
    }
}

const MAX_DUPLICATE_HEADING_SLUGS: usize = 128;

fn build_heading_slugs(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> HashMap<SharedString, usize> {
    let mut slugs = HashMap::default();
    let mut slug_counts: HashMap<String, usize> = HashMap::default();
    let mut inside_heading = false;
    let mut heading_text = String::new();
    let mut heading_source_start: Option<usize> = None;

    for (range, event) in events {
        match event {
            MarkdownEvent::Start(MarkdownTag::Heading { .. }) => {
                inside_heading = true;
                heading_text.clear();
                heading_source_start = None;
            }
            MarkdownEvent::End(MarkdownTagEnd::Heading(_)) => {
                if inside_heading {
                    let source_offset = heading_source_start.unwrap_or(range.start);
                    let base_slug = generate_heading_slug(&heading_text);
                    let count = slug_counts.entry(base_slug.clone()).or_insert(0);
                    let mut slug = if *count == 0 {
                        base_slug.clone()
                    } else {
                        format!("{base_slug}-{count}")
                    };
                    *count += 1;
                    while slugs.contains_key(slug.as_str()) {
                        let Some(count) = slug_counts.get_mut(&base_slug) else {
                            slug.clear();
                            break;
                        };
                        if *count >= MAX_DUPLICATE_HEADING_SLUGS {
                            slug.clear();
                            break;
                        }
                        slug = format!("{base_slug}-{count}");
                        *count += 1;
                    }
                    if !slug.is_empty() {
                        slugs.insert(SharedString::from(slug), source_offset);
                    }
                    inside_heading = false;
                }
            }
            MarkdownEvent::Text | MarkdownEvent::Code if inside_heading => {
                if heading_source_start.is_none() {
                    heading_source_start = Some(range.start);
                }
                heading_text.push_str(&source[range.clone()]);
            }
            MarkdownEvent::SubstitutedCode(substituted) if inside_heading => {
                if heading_source_start.is_none() {
                    heading_source_start = Some(range.start);
                }
                heading_text.push_str(substituted);
            }
            MarkdownEvent::SubstitutedText(substituted) if inside_heading => {
                if heading_source_start.is_none() {
                    heading_source_start = Some(range.start);
                }
                heading_text.push_str(substituted);
            }
            _ => {}
        }
    }

    slugs
}

fn parse_metadata_table_rows(source: &str, source_range: Range<usize>) -> Option<Vec<MetadataRow>> {
    let mut rows = Vec::new();
    let mut line_start = source_range.start;

    for line in source[source_range].split_inclusive('\n') {
        let line_end = line_start + line.len();
        let content_end = line_start + line.trim_end_matches(['\r', '\n']).len();
        let content_range = line_start..content_end;
        let line_text = &source[content_range.clone()];

        if line_text.is_empty()
            || line_text
                .chars()
                .next()
                .is_some_and(|character| character.is_whitespace())
        {
            return None;
        }

        let delimiter = line_text.find(':')?;
        let key = trim_metadata_range(source, content_range.start..content_range.start + delimiter);
        let value = trim_metadata_range(
            source,
            content_range.start + delimiter + 1..content_range.end,
        );
        if key.is_empty() || value.is_empty() {
            return None;
        }

        rows.push(MetadataRow { key, value });
        line_start = line_end;
    }

    if rows.is_empty() { None } else { Some(rows) }
}

fn trim_metadata_range(source: &str, range: Range<usize>) -> Range<usize> {
    let text = &source[range.clone()];
    let start_offset = text.len() - text.trim_start().len();
    let end_offset = text.trim_end().len();
    let start = range.start + start_offset;
    let end = (range.start + end_offset).max(start);
    start..end
}

fn is_br_tag(html: &str) -> bool {
    let Some(inner) = html
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
    else {
        return false;
    };
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    inner
        .split_ascii_whitespace()
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("br"))
}

pub(crate) fn parse_markdown_with_options(
    text: &str,
    parse_html: bool,
    parse_heading_slugs: bool,
    parse_metadata_blocks: bool,
) -> ParsedMarkdownData {
    let mut state = ParseState::default();
    let mut language_names = HashSet::default();
    let mut language_paths = HashSet::default();
    let mut has_untagged_code_block = false;
    let mut html_blocks = BTreeMap::default();
    let mut metadata_blocks = BTreeMap::default();
    let mut within_link = false;
    let mut within_code_block = false;
    let mut within_metadata = false;
    let mut within_table = false;
    let mut current_metadata_block_start = None;
    let mut metadata_block_content_range: Option<Range<usize>> = None;
    let parse_options = if parse_metadata_blocks {
        PARSE_OPTIONS.union(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS)
    } else {
        PARSE_OPTIONS
    };
    let parser = Parser::new_ext(text, parse_options);
    let mut link_definition_spans = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone())
        .collect::<Vec<_>>();
    link_definition_spans.sort_by_key(|span| span.start);
    let mut parser = parser.into_offset_iter().peekable();
    while let Some((pulldown_event, range)) = parser.next() {
        if within_metadata && !parse_metadata_blocks {
            if let pulldown_cmark::Event::End(pulldown_cmark::TagEnd::MetadataBlock(_)) =
                pulldown_event
            {
                within_metadata = false;
                current_metadata_block_start = None;
                metadata_block_content_range = None;
            }
            continue;
        }
        match pulldown_event {
            pulldown_cmark::Event::Start(tag) => {
                if let pulldown_cmark::Tag::HtmlBlock = &tag {
                    state.push_event(range.clone(), MarkdownEvent::Start(MarkdownTag::HtmlBlock));

                    if parse_html {
                        if let Some(block) =
                            html::html_parser::parse_html_block(&text[range.clone()], range.clone())
                        {
                            html_blocks.insert(range.start, block);

                            while let Some((event, end_range)) = parser.next() {
                                if let pulldown_cmark::Event::End(
                                    pulldown_cmark::TagEnd::HtmlBlock,
                                ) = event
                                {
                                    state.push_event(
                                        end_range,
                                        MarkdownEvent::End(MarkdownTagEnd::HtmlBlock),
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    continue;
                }

                let tag = match tag {
                    pulldown_cmark::Tag::Link {
                        link_type,
                        dest_url,
                        title,
                        id,
                    } => {
                        within_link = true;
                        MarkdownTag::Link {
                            link_type,
                            dest_url: SharedString::from(dest_url.into_string()),
                            title: SharedString::from(title.into_string()),
                            id: SharedString::from(id.into_string()),
                        }
                    }
                    pulldown_cmark::Tag::MetadataBlock(kind) => {
                        within_metadata = true;
                        current_metadata_block_start = Some(range.start);
                        metadata_block_content_range = None;
                        if !parse_metadata_blocks {
                            continue;
                        }
                        MarkdownTag::MetadataBlock(kind)
                    }
                    pulldown_cmark::Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Indented) => {
                        within_code_block = true;
                        MarkdownTag::CodeBlock {
                            kind: CodeBlockKind::Indented,
                            metadata: CodeBlockMetadata {
                                content_range: range.clone(),
                                line_count: 1,
                                is_fenced_closed: false,
                            },
                        }
                    }
                    pulldown_cmark::Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(
                        ref info,
                    )) => {
                        within_code_block = true;
                        let code_block_source = &text[range.clone()];
                        let content_range = extract_code_block_content_range(code_block_source);
                        let is_fenced_closed = content_range.end < code_block_source.len();
                        let content_range =
                            content_range.start + range.start..content_range.end + range.start;

                        // Valid to use bytes since multi-byte UTF-8 doesn't use ASCII chars.
                        let line_count = text[content_range.clone()]
                            .bytes()
                            .filter(|c| *c == b'\n')
                            .count();

                        let metadata = CodeBlockMetadata {
                            content_range,
                            line_count,
                            is_fenced_closed,
                        };

                        let info = info.trim();
                        let kind = if info.is_empty() {
                            has_untagged_code_block = true;
                            CodeBlockKind::Fenced
                            // Languages should never contain a slash, and PathRanges always should.
                            // (Models are told to specify them relative to a workspace root.)
                        } else if info.contains('/') {
                            let path_range = PathWithRange::new(info);
                            language_paths.insert(path_range.path.clone());
                            CodeBlockKind::FencedSrc(path_range)
                        } else {
                            let language = SharedString::from(info.to_string());
                            language_names.insert(language.clone());
                            CodeBlockKind::FencedLang(language)
                        };

                        MarkdownTag::CodeBlock { kind, metadata }
                    }
                    pulldown_cmark::Tag::Paragraph => MarkdownTag::Paragraph,
                    pulldown_cmark::Tag::Heading {
                        level,
                        id,
                        classes,
                        attrs,
                    } => {
                        let id = id.map(|id| SharedString::from(id.into_string()));
                        let classes = classes
                            .into_iter()
                            .map(|c| SharedString::from(c.into_string()))
                            .collect();
                        let attrs = attrs
                            .into_iter()
                            .map(|(key, value)| {
                                (
                                    SharedString::from(key.into_string()),
                                    value.map(|v| SharedString::from(v.into_string())),
                                )
                            })
                            .collect();
                        MarkdownTag::Heading {
                            level,
                            id,
                            classes,
                            attrs,
                        }
                    }
                    pulldown_cmark::Tag::BlockQuote(kind) => MarkdownTag::BlockQuote(kind),
                    pulldown_cmark::Tag::List(start_number) => MarkdownTag::List(start_number),
                    pulldown_cmark::Tag::Item => MarkdownTag::Item,
                    pulldown_cmark::Tag::FootnoteDefinition(label) => {
                        MarkdownTag::FootnoteDefinition(SharedString::from(label.to_string()))
                    }
                    pulldown_cmark::Tag::Table(alignments) => {
                        within_table = true;
                        MarkdownTag::Table(alignments)
                    }
                    pulldown_cmark::Tag::TableHead => MarkdownTag::TableHead,
                    pulldown_cmark::Tag::TableRow => MarkdownTag::TableRow,
                    pulldown_cmark::Tag::TableCell => MarkdownTag::TableCell,
                    pulldown_cmark::Tag::Emphasis => MarkdownTag::Emphasis,
                    pulldown_cmark::Tag::Strong => MarkdownTag::Strong,
                    pulldown_cmark::Tag::Strikethrough => MarkdownTag::Strikethrough,
                    pulldown_cmark::Tag::Superscript => MarkdownTag::Superscript,
                    pulldown_cmark::Tag::Subscript => MarkdownTag::Subscript,
                    pulldown_cmark::Tag::Image {
                        link_type,
                        dest_url,
                        title,
                        id,
                    } => MarkdownTag::Image {
                        link_type,
                        dest_url: SharedString::from(dest_url.into_string()),
                        title: SharedString::from(title.into_string()),
                        id: SharedString::from(id.into_string()),
                    },
                    pulldown_cmark::Tag::HtmlBlock => MarkdownTag::HtmlBlock, // this is handled above separately
                    pulldown_cmark::Tag::DefinitionList => MarkdownTag::DefinitionList,
                    pulldown_cmark::Tag::DefinitionListTitle => MarkdownTag::DefinitionListTitle,
                    pulldown_cmark::Tag::DefinitionListDefinition => {
                        MarkdownTag::DefinitionListDefinition
                    }
                };
                state.push_event(range, MarkdownEvent::Start(tag))
            }
            pulldown_cmark::Event::End(tag) => {
                if let pulldown_cmark::TagEnd::Link = tag {
                    within_link = false;
                } else if let pulldown_cmark::TagEnd::CodeBlock = tag {
                    within_code_block = false;
                } else if let pulldown_cmark::TagEnd::MetadataBlock(_) = tag {
                    within_metadata = false;
                    let block_start = current_metadata_block_start.take();
                    let content_range = metadata_block_content_range.take();
                    if parse_metadata_blocks
                        && let (Some(block_start), Some(content_range)) =
                            (block_start, content_range)
                    {
                        metadata_blocks.insert(
                            block_start,
                            ParsedMetadataBlock {
                                rows: parse_metadata_table_rows(text, content_range.clone()),
                                content_range,
                            },
                        );
                    }
                    if !parse_metadata_blocks {
                        continue;
                    }
                } else if let pulldown_cmark::TagEnd::Table = tag {
                    within_table = false;
                }
                state.push_event(range, MarkdownEvent::End(tag));
            }
            pulldown_cmark::Event::Text(parsed) => {
                fn event_for(
                    text: &str,
                    range: Range<usize>,
                    str: &str,
                ) -> (Range<usize>, MarkdownEvent) {
                    if str == &text[range.clone()] {
                        (range, MarkdownEvent::Text)
                    } else {
                        (range, MarkdownEvent::SubstitutedText(str.to_owned()))
                    }
                }

                if within_metadata {
                    match &mut metadata_block_content_range {
                        Some(content_range) => {
                            content_range.start = content_range.start.min(range.start);
                            content_range.end = content_range.end.max(range.end);
                        }
                        None => metadata_block_content_range = Some(range.clone()),
                    }
                    state.push_event(range, MarkdownEvent::Text);
                    continue;
                }

                if within_code_block {
                    let (range, event) = event_for(text, range, &parsed);
                    state.push_event(range, event);
                    continue;
                }

                #[derive(Debug)]
                struct TextRange<'a> {
                    source_range: Range<usize>,
                    merged_range: Range<usize>,
                    parsed: CowStr<'a>,
                }

                let mut last_len = parsed.len();
                let mut ranges = vec![TextRange {
                    source_range: range.clone(),
                    merged_range: 0..last_len,
                    parsed,
                }];

                while match parser.peek() {
                    Some((pulldown_cmark::Event::Text(_), _)) => true,
                    Some((pulldown_cmark::Event::InlineHtml(html), _)) => {
                        parse_html && !is_br_tag(html)
                    }
                    _ => false,
                } {
                    let Some((next_event, next_range)) = parser.next() else {
                        unreachable!()
                    };
                    let next_text = match next_event {
                        pulldown_cmark::Event::Text(next_event) => next_event,
                        pulldown_cmark::Event::InlineHtml(_) => CowStr::Borrowed(""),
                        _ => unreachable!(),
                    };
                    let next_len = last_len + next_text.len();
                    ranges.push(TextRange {
                        source_range: next_range.clone(),
                        merged_range: last_len..next_len,
                        parsed: next_text,
                    });
                    last_len = next_len;
                }

                let mut merged_text =
                    String::with_capacity(ranges.last().unwrap().merged_range.end);
                for range in &ranges {
                    merged_text.push_str(&range.parsed);
                }

                let mut ranges = ranges.into_iter().peekable();

                if !within_link && !within_code_block {
                    let mut finder = LinkFinder::new();
                    finder.kinds(&[linkify::LinkKind::Url]);

                    // Find links in the merged text
                    for link in finder.links(&merged_text) {
                        let link_start_in_merged = link.start();
                        let link_end_in_merged = link.end();

                        while ranges
                            .peek()
                            .is_some_and(|range| range.merged_range.end <= link_start_in_merged)
                        {
                            let range = ranges.next().unwrap();
                            let (range, event) = event_for(text, range.source_range, &range.parsed);
                            state.push_event(range, event);
                        }

                        let Some(range) = ranges.peek_mut() else {
                            continue;
                        };
                        let prefix_len = link_start_in_merged - range.merged_range.start;
                        if prefix_len > 0 {
                            let (head, tail) = range.parsed.split_at(prefix_len);
                            let (event_range, event) = event_for(
                                text,
                                range.source_range.start..range.source_range.start + prefix_len,
                                head,
                            );
                            state.push_event(event_range, event);
                            range.parsed = CowStr::Boxed(tail.into());
                            range.merged_range.start += prefix_len;
                            range.source_range.start += prefix_len;
                        }

                        let link_start_in_source = range.source_range.start;
                        let mut link_end_in_source = range.source_range.end;
                        let mut link_events = Vec::new();

                        while ranges
                            .peek()
                            .is_some_and(|range| range.merged_range.end <= link_end_in_merged)
                        {
                            let range = ranges.next().unwrap();
                            link_end_in_source = range.source_range.end;
                            link_events.push(event_for(text, range.source_range, &range.parsed));
                        }

                        if let Some(range) = ranges.peek_mut() {
                            let prefix_len = link_end_in_merged - range.merged_range.start;
                            if prefix_len > 0 {
                                let (head, tail) = range.parsed.split_at(prefix_len);
                                link_events.push(event_for(
                                    text,
                                    range.source_range.start..range.source_range.start + prefix_len,
                                    head,
                                ));
                                range.parsed = CowStr::Boxed(tail.into());
                                range.merged_range.start += prefix_len;
                                range.source_range.start += prefix_len;
                                link_end_in_source = range.source_range.start;
                            }
                        }
                        let link_range = link_start_in_source..link_end_in_source;

                        state.push_event(
                            link_range.clone(),
                            MarkdownEvent::Start(MarkdownTag::Link {
                                link_type: LinkType::Autolink,
                                dest_url: SharedString::from(link.as_str().to_string()),
                                title: SharedString::default(),
                                id: SharedString::default(),
                            }),
                        );
                        for (range, event) in link_events {
                            state.push_event(range, event);
                        }
                        state.push_event(
                            link_range.clone(),
                            MarkdownEvent::End(MarkdownTagEnd::Link),
                        );
                    }
                }

                for range in ranges {
                    let (range, event) = event_for(text, range.source_range, &range.parsed);
                    state.push_event(range, event);
                }
            }
            pulldown_cmark::Event::Code(parsed) => {
                let content_range = extract_code_content_range(&text[range.clone()]);
                let content_range =
                    content_range.start + range.start..content_range.end + range.start;
                let source = &text[content_range.clone()];
                let event = if within_table && source.contains(r"\|") {
                    MarkdownEvent::SubstitutedCode(parsed.to_string())
                } else {
                    MarkdownEvent::Code
                };
                state.push_event(content_range, event)
            }
            pulldown_cmark::Event::Html(_) => state.push_event(range, MarkdownEvent::Html),
            pulldown_cmark::Event::InlineHtml(html) => {
                if parse_html && is_br_tag(&html) {
                    state.push_event(range, MarkdownEvent::HardBreak)
                } else {
                    state.push_event(range, MarkdownEvent::InlineHtml)
                }
            }
            pulldown_cmark::Event::FootnoteReference(label) => state.push_event(
                range,
                MarkdownEvent::FootnoteReference(SharedString::from(label.to_string())),
            ),
            pulldown_cmark::Event::SoftBreak => state.push_event(range, MarkdownEvent::SoftBreak),
            pulldown_cmark::Event::HardBreak => state.push_event(range, MarkdownEvent::HardBreak),
            pulldown_cmark::Event::Rule => state.push_event(range, MarkdownEvent::Rule),
            pulldown_cmark::Event::TaskListMarker(checked) => {
                state.push_event(range, MarkdownEvent::TaskListMarker(checked))
            }
            pulldown_cmark::Event::InlineMath(_) | pulldown_cmark::Event::DisplayMath(_) => {}
        }
    }

    let heading_slugs = if parse_heading_slugs {
        build_heading_slugs(text, &state.events)
    } else {
        HashMap::default()
    };
    let footnote_definitions = build_footnote_definitions(&state.events);

    ParsedMarkdownData {
        events: state.events,
        language_names,
        language_paths,
        root_block_starts: state.root_block_starts,
        html_blocks,
        metadata_blocks,
        heading_slugs,
        footnote_definitions,
        link_definition_spans,
        has_untagged_code_block,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownLivePreviewBlock {
    pub source_range: Range<usize>,
    pub replacement_range: Range<usize>,
    pub source: SharedString,
    pub display_text: SharedString,
    pub image_destination: Option<SharedString>,
    pub kind: MarkdownLivePreviewBlockKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkdownLivePreviewBlockKind {
    Paragraph,
    Image,
    Heading(u8),
    List,
    Item,
    BlockQuote,
    CodeBlock { is_indented: bool, is_mermaid: bool },
    Table,
    Rule,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownLivePreviewLayout {
    /// Blocks that are rendered as replacement blocks in the editor.
    pub blocks: Vec<MarkdownLivePreviewBlock>,
    /// Inline syntax markers that are hidden or substituted in place.
    pub inline_markers: Vec<MarkdownLivePreviewInlineMarker>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownLivePreviewInlineMarker {
    pub range: Range<usize>,
    pub kind: MarkdownLivePreviewInlineMarkerKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkdownLivePreviewInlineMarkerKind {
    /// Syntax that is hidden entirely, such as emphasis delimiters or heading hashes.
    Hidden,
    /// An unordered list marker, rendered as a bullet.
    Bullet,
    /// A task list marker, rendered as a checkbox.
    TaskListMarker { checked: bool },
    /// A block quote marker, rendered as a vertical bar.
    BlockQuote,
}

pub fn markdown_live_preview_layout(text: &str) -> MarkdownLivePreviewLayout {
    let parsed = parse_markdown_with_options(text, false, false, false);
    let root_blocks = live_preview_root_blocks(text, &parsed);

    let mut inline_markers = Vec::new();
    for (block, event_range) in &root_blocks {
        if block.kind.is_rich() {
            continue;
        }
        live_preview_inline_markers(
            text,
            &parsed.events[event_range.clone()],
            &mut inline_markers,
        );
    }
    inline_markers.sort_by_key(|marker| (marker.range.start, marker.range.end));

    let mut deduplicated_markers: Vec<MarkdownLivePreviewInlineMarker> =
        Vec::with_capacity(inline_markers.len());
    for marker in inline_markers {
        if marker.range.start >= marker.range.end {
            continue;
        }
        if let Some(previous) = deduplicated_markers.last()
            && marker.range.start < previous.range.end
        {
            continue;
        }
        deduplicated_markers.push(marker);
    }

    MarkdownLivePreviewLayout {
        blocks: root_blocks
            .into_iter()
            .map(|(block, _)| block)
            .filter(|block| block.kind.is_rich())
            .collect(),
        inline_markers: deduplicated_markers,
    }
}

pub fn markdown_live_preview_blocks(text: &str) -> Vec<MarkdownLivePreviewBlock> {
    let parsed = parse_markdown_with_options(text, false, false, false);
    live_preview_root_blocks(text, &parsed)
        .into_iter()
        .map(|(block, _)| block)
        .collect()
}

/// Splits the document into root-level blocks. Each block is paired with the range of
/// parser events (indices into `parsed.events`) that produced it.
fn live_preview_root_blocks(
    text: &str,
    parsed: &ParsedMarkdownData,
) -> Vec<(MarkdownLivePreviewBlock, Range<usize>)> {
    let mut blocks = Vec::new();
    let mut block_start_event_index = None;

    for (event_ix, (range, event)) in parsed.events.iter().enumerate() {
        match event {
            MarkdownEvent::RootStart => block_start_event_index = Some(event_ix + 1),
            MarkdownEvent::RootEnd(_) => {
                let Some(start_event_index) = block_start_event_index.take() else {
                    continue;
                };
                let events = &parsed.events[start_event_index..event_ix];
                let Some(source_start) = events.iter().map(|(range, _)| range.start).min() else {
                    continue;
                };
                let source_end = range.end.max(
                    events
                        .iter()
                        .map(|(range, _)| range.end)
                        .max()
                        .unwrap_or(range.end),
                );
                let kind = live_preview_block_kind(events);
                let source_start = if kind.is_code_block() {
                    live_preview_line_start(text, source_start)
                } else {
                    source_start
                };
                let source_range = trim_live_preview_source_range(
                    text,
                    source_start..source_end,
                    !kind.is_code_block(),
                );
                if source_range.start >= source_range.end {
                    continue;
                }

                let display_text = live_preview_block_text(text, events, kind);
                blocks.push((
                    MarkdownLivePreviewBlock {
                        source_range: source_range.clone(),
                        replacement_range: source_range.clone(),
                        source: SharedString::from(&text[source_range]),
                        display_text: SharedString::from(display_text),
                        image_destination: live_preview_block_image_destination(events),
                        kind,
                    },
                    start_event_index..event_ix,
                ));
            }
            _ => {}
        }
    }

    for ix in 0..blocks.len().saturating_sub(1) {
        let next_start = blocks[ix + 1].0.source_range.start;
        let separator = &text[blocks[ix].0.source_range.end..next_start];
        if separator.chars().all(char::is_whitespace)
            && let Some((last_separator_character_start, _)) = separator.char_indices().next_back()
        {
            blocks[ix].0.replacement_range.end =
                blocks[ix].0.source_range.end + last_separator_character_start;
        }
    }

    blocks
}

fn live_preview_inline_markers(
    text: &str,
    events: &[(Range<usize>, MarkdownEvent)],
    markers: &mut Vec<MarkdownLivePreviewInlineMarker>,
) {
    use MarkdownLivePreviewInlineMarkerKind as Kind;

    let mut push = |range: Range<usize>, kind: Kind| {
        if range.start < range.end {
            markers.push(MarkdownLivePreviewInlineMarker { range, kind });
        }
    };

    let mut open_tags: Vec<(usize, &MarkdownTag, &Range<usize>)> = Vec::new();
    for (event_ix, (range, event)) in events.iter().enumerate() {
        match event {
            MarkdownEvent::Start(tag) => open_tags.push((event_ix, tag, range)),
            MarkdownEvent::End(_) => {
                let Some((start_ix, tag, tag_range)) = open_tags.pop() else {
                    continue;
                };
                let inner = &events[start_ix + 1..event_ix];
                let content_range = inner
                    .iter()
                    .map(|(range, _)| range.start)
                    .min()
                    .zip(inner.iter().map(|(range, _)| range.end).max());

                match tag {
                    MarkdownTag::Emphasis
                    | MarkdownTag::Strong
                    | MarkdownTag::Strikethrough
                    | MarkdownTag::Link { .. } => {
                        let Some((content_start, content_end)) = content_range else {
                            continue;
                        };
                        push(tag_range.start..content_start, Kind::Hidden);
                        push(
                            trim_end_whitespace(text, content_end..tag_range.end),
                            Kind::Hidden,
                        );
                    }
                    MarkdownTag::Heading { .. } => {
                        let Some((content_start, content_end)) = content_range else {
                            continue;
                        };
                        push(tag_range.start..content_start, Kind::Hidden);
                        // Closing hashes of an ATX heading, or the underline of a setext heading.
                        push(
                            trim_end_whitespace(text, content_end..tag_range.end),
                            Kind::Hidden,
                        );
                    }
                    MarkdownTag::Item => {
                        let marker_start = tag_range.start;
                        let task_marker = inner.iter().find_map(|(range, event)| match event {
                            MarkdownEvent::TaskListMarker(checked) => Some((range, *checked)),
                            _ => None,
                        });
                        if let Some((task_range, checked)) = task_marker
                            && task_range.start >= marker_start
                            && text[marker_start..task_range.start]
                                .chars()
                                .all(|character| {
                                    character.is_ascii_digit()
                                        || character.is_whitespace()
                                        || matches!(character, '-' | '*' | '+' | '.' | ')')
                                })
                        {
                            push(
                                marker_start..task_range.end,
                                Kind::TaskListMarker { checked },
                            );
                        } else if matches!(
                            text[marker_start..].chars().next(),
                            Some('-' | '*' | '+')
                        ) {
                            push(marker_start..marker_start + 1, Kind::Bullet);
                        }
                    }
                    MarkdownTag::BlockQuote(_) => {
                        let mut line_start = tag_range.start;
                        for line in text[tag_range.clone()].split_inclusive('\n') {
                            let bytes = line.as_bytes();
                            let mut ix = 0;
                            while ix < bytes.len() && ix < 3 && bytes[ix] == b' ' {
                                ix += 1;
                            }
                            while ix < bytes.len() && bytes[ix] == b'>' {
                                push(line_start + ix..line_start + ix + 1, Kind::BlockQuote);
                                ix += 1;
                                if ix < bytes.len() && bytes[ix] == b' ' {
                                    ix += 1;
                                }
                            }
                            line_start += line.len();
                        }
                    }
                    _ => {}
                }
            }
            MarkdownEvent::Code | MarkdownEvent::SubstitutedCode(_) => {
                if let Some((leading, trailing)) = code_span_delimiters(text, range.clone()) {
                    push(leading, Kind::Hidden);
                    push(trailing, Kind::Hidden);
                }
            }
            _ => {}
        }
    }
}

fn trim_end_whitespace(text: &str, range: Range<usize>) -> Range<usize> {
    let trimmed_len = text[range.clone()].trim_end().len();
    range.start..range.start + trimmed_len
}

/// Finds the backtick delimiters surrounding an inline code span whose content
/// occupies `content_range`. Returns the leading and trailing delimiter ranges.
fn code_span_delimiters(
    text: &str,
    content_range: Range<usize>,
) -> Option<(Range<usize>, Range<usize>)> {
    let bytes = text.as_bytes();

    let mut leading_end = content_range.start;
    if leading_end > 0 && bytes[leading_end - 1] == b' ' {
        leading_end -= 1;
    }
    let mut leading_start = leading_end;
    while leading_start > 0 && bytes[leading_start - 1] == b'`' {
        leading_start -= 1;
    }

    let mut trailing_start = content_range.end;
    if trailing_start < bytes.len() && bytes[trailing_start] == b' ' {
        trailing_start += 1;
    }
    let mut trailing_end = trailing_start;
    while trailing_end < bytes.len() && bytes[trailing_end] == b'`' {
        trailing_end += 1;
    }

    let leading_ticks = leading_end - leading_start;
    let trailing_ticks = trailing_end - trailing_start;
    if leading_ticks == 0 || leading_ticks != trailing_ticks {
        return None;
    }

    // Only swallow the padding space when the delimiter actually follows it.
    let leading = if leading_end == content_range.start {
        leading_start..leading_end
    } else {
        leading_start..content_range.start
    };
    let trailing = if trailing_start == content_range.end {
        trailing_start..trailing_end
    } else {
        content_range.end..trailing_end
    };
    Some((leading, trailing))
}

impl MarkdownLivePreviewBlockKind {
    /// Whether the block is rendered as a replacement block instead of being
    /// shown inline with its syntax markers hidden.
    pub fn is_rich(self) -> bool {
        matches!(
            self,
            Self::Image | Self::CodeBlock { .. } | Self::Table | Self::Rule
        )
    }

    pub fn is_code_block(self) -> bool {
        matches!(self, Self::CodeBlock { .. })
    }

    pub fn is_indented_code_block(self) -> bool {
        matches!(
            self,
            Self::CodeBlock {
                is_indented: true,
                ..
            }
        )
    }

    pub fn is_mermaid_code_block(self) -> bool {
        matches!(
            self,
            Self::CodeBlock {
                is_mermaid: true,
                ..
            }
        )
    }
}

fn live_preview_line_start(source: &str, offset: usize) -> usize {
    source[..offset]
        .rfind('\n')
        .map_or(0, |newline_ix| newline_ix + 1)
}

fn trim_live_preview_source_range(
    source: &str,
    range: Range<usize>,
    trim_leading_whitespace: bool,
) -> Range<usize> {
    let mut start = range.start;
    let mut end = range.end;

    if trim_leading_whitespace {
        while start < end {
            let Some(character) = source[start..end].chars().next() else {
                break;
            };
            if !character.is_whitespace() {
                break;
            }
            start += character.len_utf8();
        }
    }

    while start < end {
        let Some(character) = source[start..end].chars().next_back() else {
            break;
        };
        if !character.is_whitespace() {
            break;
        }
        end -= character.len_utf8();
    }

    start..end
}

fn live_preview_block_image_destination(
    events: &[(Range<usize>, MarkdownEvent)],
) -> Option<SharedString> {
    events.iter().find_map(|(_, event)| match event {
        MarkdownEvent::Start(MarkdownTag::Image { dest_url, .. }) => Some(dest_url.clone()),
        _ => None,
    })
}

fn live_preview_block_kind(
    events: &[(Range<usize>, MarkdownEvent)],
) -> MarkdownLivePreviewBlockKind {
    if events
        .iter()
        .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::Image { .. })))
    {
        return MarkdownLivePreviewBlockKind::Image;
    }

    for (_, event) in events {
        match event {
            MarkdownEvent::Rule => return MarkdownLivePreviewBlockKind::Rule,
            MarkdownEvent::Start(MarkdownTag::Paragraph) => {
                return MarkdownLivePreviewBlockKind::Paragraph;
            }
            MarkdownEvent::Start(MarkdownTag::Heading { level, .. }) => {
                return MarkdownLivePreviewBlockKind::Heading(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
            }
            MarkdownEvent::Start(MarkdownTag::List(_)) => {
                return MarkdownLivePreviewBlockKind::List;
            }
            MarkdownEvent::Start(MarkdownTag::Item) => return MarkdownLivePreviewBlockKind::Item,
            MarkdownEvent::Start(MarkdownTag::BlockQuote(_)) => {
                return MarkdownLivePreviewBlockKind::BlockQuote;
            }
            MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, .. }) => {
                return MarkdownLivePreviewBlockKind::CodeBlock {
                    is_indented: matches!(kind, CodeBlockKind::Indented),
                    is_mermaid: matches!(kind, CodeBlockKind::FencedLang(language) if is_mermaid_code_block_language(language)),
                };
            }
            MarkdownEvent::Start(MarkdownTag::Table(_)) => {
                return MarkdownLivePreviewBlockKind::Table;
            }
            _ => {}
        }
    }

    MarkdownLivePreviewBlockKind::Other
}

fn is_mermaid_code_block_language(language: &str) -> bool {
    language.split_whitespace().next() == Some("mermaid")
}

fn live_preview_block_text(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
    kind: MarkdownLivePreviewBlockKind,
) -> String {
    if let Some((_, MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, metadata }))) = events
        .iter()
        .find(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::CodeBlock { .. })))
    {
        let text = source[metadata.content_range.clone()]
            .trim_end_matches('\n')
            .to_string();
        return if matches!(kind, CodeBlockKind::Indented) {
            text.lines()
                .map(|line| line.strip_prefix("    ").unwrap_or(line))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text
        };
    }

    let mut text = String::new();
    for (range, event) in events {
        match event {
            MarkdownEvent::Text | MarkdownEvent::Code => text.push_str(&source[range.clone()]),
            MarkdownEvent::SubstitutedText(substituted) => text.push_str(substituted),
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => text.push('\n'),
            MarkdownEvent::TaskListMarker(checked) => {
                text.push_str(if *checked { "[x] " } else { "[ ] " });
            }
            MarkdownEvent::Rule => text.push_str("---"),
            _ => {}
        }
    }

    match kind {
        MarkdownLivePreviewBlockKind::List | MarkdownLivePreviewBlockKind::Item => text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => text.trim().to_string(),
    }
}

fn build_footnote_definitions(
    events: &[(Range<usize>, MarkdownEvent)],
) -> HashMap<SharedString, usize> {
    let mut definitions = HashMap::default();
    let mut current_label: Option<SharedString> = None;

    for (range, event) in events {
        match event {
            MarkdownEvent::Start(MarkdownTag::FootnoteDefinition(label)) => {
                current_label = Some(label.clone());
            }
            MarkdownEvent::End(MarkdownTagEnd::FootnoteDefinition) => {
                current_label = None;
            }
            MarkdownEvent::Text if current_label.is_some() => {
                if let Some(label) = current_label.take() {
                    definitions.entry(label).or_insert(range.start);
                }
            }
            _ => {}
        }
    }

    definitions
}

pub fn parse_links_only(text: &str) -> Vec<(Range<usize>, MarkdownEvent)> {
    let mut events = Vec::new();
    let mut finder = LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    let mut text_range = Range {
        start: 0,
        end: text.len(),
    };
    for link in finder.links(text) {
        let link_range = link.start()..link.end();

        if link_range.start > text_range.start {
            events.push((text_range.start..link_range.start, MarkdownEvent::Text));
        }

        events.push((
            link_range.clone(),
            MarkdownEvent::Start(MarkdownTag::Link {
                link_type: LinkType::Autolink,
                dest_url: SharedString::from(link.as_str().to_string()),
                title: SharedString::default(),
                id: SharedString::default(),
            }),
        ));
        events.push((link_range.clone(), MarkdownEvent::Text));
        events.push((link_range.clone(), MarkdownEvent::End(MarkdownTagEnd::Link)));

        text_range.start = link_range.end;
    }

    if text_range.end > text_range.start {
        events.push((text_range, MarkdownEvent::Text));
    }

    events
}

/// A static-lifetime equivalent of pulldown_cmark::Event so we can cache the
/// parse result for rendering without resorting to unsafe lifetime coercion.
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownEvent {
    /// Start of a tagged element. Events that are yielded after this event
    /// and before its corresponding `End` event are inside this element.
    /// Start and end events are guaranteed to be balanced.
    Start(MarkdownTag),
    /// End of a tagged element.
    End(MarkdownTagEnd),
    /// Text that uses the associated range from the markdown source.
    Text,
    /// Text that differs from the markdown source - typically due to substitution of HTML entities
    /// and smart punctuation.
    SubstitutedText(String),
    /// An inline code node.
    Code,
    /// An inline code node that differs from the markdown source due to escape decoding.
    SubstitutedCode(String),
    /// An HTML node.
    Html,
    /// An inline HTML node.
    InlineHtml,
    /// A reference to a footnote with given label, which may or may not be defined
    /// by an event with a `Tag::FootnoteDefinition` tag. Definitions and references to them may
    /// occur in any order.
    FootnoteReference(SharedString),
    /// A soft line break.
    SoftBreak,
    /// A hard line break.
    HardBreak,
    /// A horizontal ruler.
    Rule,
    /// A task list marker, rendered as a checkbox in HTML. Contains a true when it is checked.
    TaskListMarker(bool),
    /// Start of a root-level block (a top-level structural element like a paragraph, heading, list, etc.).
    RootStart,
    /// End of a root-level block. Contains the root block index.
    RootEnd(usize),
}

/// Tags for elements that can contain other elements.
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownTag {
    /// A paragraph of text and other inline elements.
    Paragraph,

    /// A heading, with optional identifier, classes and custom attributes.
    /// The identifier is prefixed with `#` and the last one in the attributes
    /// list is chosen, classes are prefixed with `.` and custom attributes
    /// have no prefix and can optionally have a value (`myattr` o `myattr=myvalue`).
    Heading {
        level: HeadingLevel,
        id: Option<SharedString>,
        classes: Vec<SharedString>,
        /// The first item of the tuple is the attr and second one the value.
        attrs: Vec<(SharedString, Option<SharedString>)>,
    },

    BlockQuote(Option<pulldown_cmark::BlockQuoteKind>),

    /// A code block.
    CodeBlock {
        kind: CodeBlockKind,
        metadata: CodeBlockMetadata,
    },

    /// A HTML block.
    HtmlBlock,

    /// A list. If the list is ordered the field indicates the number of the first item.
    /// Contains only list items.
    List(Option<u64>), // TODO: add delim and tight for ast (not needed for html)

    /// A list item.
    Item,

    /// A footnote definition. The value contained is the footnote's label by which it can
    /// be referred to.
    FootnoteDefinition(SharedString),

    /// A table. Contains a vector describing the text-alignment for each of its columns.
    Table(Vec<Alignment>),

    /// A table header. Contains only `TableCell`s. Note that the table body starts immediately
    /// after the closure of the `TableHead` tag. There is no `TableBody` tag.
    TableHead,

    /// A table row. Is used both for header rows as body rows. Contains only `TableCell`s.
    TableRow,
    TableCell,

    // span-level tags
    Emphasis,
    Strong,
    Strikethrough,
    Superscript,
    Subscript,

    /// A link.
    Link {
        link_type: LinkType,
        dest_url: SharedString,
        title: SharedString,
        /// Identifier of reference links, e.g. `world` in the link `[hello][world]`.
        id: SharedString,
    },

    /// An image. The first field is the link type, the second the destination URL and the third is a title,
    /// the fourth is the link identifier.
    Image {
        link_type: LinkType,
        dest_url: SharedString,
        title: SharedString,
        /// Identifier of reference links, e.g. `world` in the link `[hello][world]`.
        id: SharedString,
    },

    /// A metadata block.
    MetadataBlock(MetadataBlockKind),

    DefinitionList,
    DefinitionListTitle,
    DefinitionListDefinition,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CodeBlockKind {
    Indented,
    /// "Fenced" means "surrounded by triple backticks."
    /// There can optionally be either a language after the backticks (like in traditional Markdown)
    /// or, if an agent is specifying a path for a source location in the project, it can be a PathRange,
    /// e.g. ```path/to/foo.rs#L123-456 instead of ```rust
    Fenced,
    FencedLang(SharedString),
    FencedSrc(PathWithRange),
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct CodeBlockMetadata {
    pub content_range: Range<usize>,
    pub line_count: usize,
    pub is_fenced_closed: bool,
}

fn extract_code_content_range(text: &str) -> Range<usize> {
    let text_len = text.len();
    if text_len == 0 {
        return 0..0;
    }

    let start_ticks = text.chars().take_while(|&c| c == '`').count();

    if start_ticks == 0 || start_ticks > text_len {
        return 0..text_len;
    }

    let end_ticks = text.chars().rev().take_while(|&c| c == '`').count();

    if end_ticks != start_ticks || text_len < start_ticks + end_ticks {
        return 0..text_len;
    }

    start_ticks..text_len - end_ticks
}

pub(crate) fn extract_code_block_content_range(text: &str) -> Range<usize> {
    let mut range = 0..text.len();
    let Some(fence_character) = text
        .as_bytes()
        .first()
        .copied()
        .filter(|character| matches!(character, b'`' | b'~'))
    else {
        return range;
    };
    let opening_fence_len = text
        .bytes()
        .take_while(|character| *character == fence_character)
        .count();
    if opening_fence_len < 3 {
        return range;
    }

    range.start += opening_fence_len;
    if let Some(newline_ix) = text[range.clone()].find('\n') {
        range.start += newline_ix + 1;
    }

    let text_without_line_ending =
        text.trim_end_matches(|character| matches!(character, '\r' | '\n'));
    let closing_line_start = text_without_line_ending
        .rfind('\n')
        .map_or(0, |newline_ix| newline_ix + 1);
    if closing_line_start >= range.start {
        let closing_line = &text_without_line_ending[closing_line_start..];
        let closing_fence = closing_line.trim_start_matches(' ');
        let indentation_len = closing_line.len() - closing_fence.len();
        let closing_fence_len = closing_fence
            .bytes()
            .take_while(|character| *character == fence_character)
            .count();
        let trailing_characters = &closing_fence[closing_fence_len..];
        if indentation_len <= 3
            && closing_fence_len >= opening_fence_len
            && trailing_characters
                .bytes()
                .all(|character| matches!(character, b' ' | b'\t'))
        {
            range.end = closing_line_start;
        }
    }

    if range.start > range.end {
        range.end = range.start;
    }
    range
}

#[cfg(test)]
mod tests {
    use super::MarkdownEvent::*;
    use super::MarkdownTag::*;
    use super::*;

    const CONDITIONAL_OPTIONS: Options = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    const UNWANTED_OPTIONS: Options = Options::ENABLE_MATH
        .union(Options::ENABLE_DEFINITION_LIST)
        .union(Options::ENABLE_WIKILINKS);

    #[test]
    fn all_options_considered() {
        // The purpose of this is to fail when new options are added to pulldown_cmark, so that they
        // can be evaluated for inclusion.
        assert_eq!(
            PARSE_OPTIONS
                .union(CONDITIONAL_OPTIONS)
                .union(UNWANTED_OPTIONS),
            Options::all()
        );
    }

    #[test]
    fn wanted_and_unwanted_options_disjoint() {
        assert_eq!(
            PARSE_OPTIONS
                .union(CONDITIONAL_OPTIONS)
                .intersection(UNWANTED_OPTIONS),
            Options::empty()
        );
    }

    #[test]
    fn test_yaml_style_metadata_block() {
        assert_eq!(
            parse_markdown_with_options("---\ntitle: Post\n---\n# Heading", false, false, true),
            ParsedMarkdownData {
                events: vec![
                    (0..19, RootStart),
                    (0..19, Start(MetadataBlock(MetadataBlockKind::YamlStyle))),
                    (4..16, Text),
                    (
                        0..19,
                        End(MarkdownTagEnd::MetadataBlock(MetadataBlockKind::YamlStyle))
                    ),
                    (0..19, RootEnd(0)),
                    (20..29, RootStart),
                    (
                        20..29,
                        Start(Heading {
                            level: HeadingLevel::H1,
                            id: None,
                            classes: Vec::new(),
                            attrs: Vec::new(),
                        })
                    ),
                    (22..29, Text),
                    (20..29, End(MarkdownTagEnd::Heading(HeadingLevel::H1))),
                    (20..29, RootEnd(1)),
                ],
                root_block_starts: vec![0, 20],
                metadata_blocks: BTreeMap::from_iter([(
                    0,
                    ParsedMetadataBlock {
                        content_range: 4..16,
                        rows: Some(vec![MetadataRow {
                            key: 4..9,
                            value: 11..15,
                        }]),
                    },
                )]),
                ..Default::default()
            }
        )
    }

    #[test]
    fn test_metadata_block_text_is_verbatim() {
        let parsed =
            parse_markdown_with_options("---\nurl: https://zed.dev\n---\nBody", false, false, true);
        assert!(
            parsed
                .events
                .iter()
                .all(|(_, event)| !matches!(event, Start(Link { .. })))
        );
    }

    #[test]
    fn test_metadata_blocks_store_table_rows() {
        let parsed = parse_markdown_with_options(
            "---\ntitle: Post\nauthor: Zed\n---\nBody",
            false,
            false,
            true,
        );

        assert_eq!(
            parsed.metadata_blocks,
            BTreeMap::from_iter([(
                0,
                ParsedMetadataBlock {
                    content_range: 4..28,
                    rows: Some(vec![
                        MetadataRow {
                            key: 4..9,
                            value: 11..15,
                        },
                        MetadataRow {
                            key: 16..22,
                            value: 24..27,
                        },
                    ]),
                },
            )])
        );
    }

    #[test]
    fn test_metadata_blocks_store_fallback_for_nested_yaml() {
        let parsed =
            parse_markdown_with_options("---\ntags:\n  - zed\n---\nBody", false, false, true);

        assert_eq!(
            parsed.metadata_blocks,
            BTreeMap::from_iter([(
                0,
                ParsedMetadataBlock {
                    content_range: 4..18,
                    rows: None,
                },
            )])
        );
    }

    #[test]
    fn test_metadata_table_rows_parse_simple_colon_pairs() {
        let source = "title: Post\nauthor: Zed\n";
        let Some(rows) = parse_metadata_table_rows(source, 0..source.len()) else {
            panic!("expected metadata rows");
        };
        let pairs = rows
            .into_iter()
            .map(|row| (&source[row.key], &source[row.value]))
            .collect::<Vec<_>>();

        assert_eq!(pairs, vec![("title", "Post"), ("author", "Zed")]);
    }

    #[test]
    fn test_metadata_table_rows_reject_non_simple_colon_pairs() {
        for source in [
            "tags:\n  - zed\n",
            "title = Post\n",
            "title:\n",
            "title:   \n",
            ": Post\n",
            " title: Post\n",
            "\n",
        ] {
            assert!(parse_metadata_table_rows(source, 0..source.len()).is_none());
        }
    }

    #[test]
    fn test_trim_metadata_range_returns_valid_empty_range() {
        let source = "key:   \n";
        let trimmed = trim_metadata_range(source, 4..7);

        assert_eq!(trimmed, 7..7);
        assert!(source[trimmed].is_empty());
    }

    #[test]
    fn test_html_comments() {
        assert_eq!(
            parse_markdown_with_options(
                "  <!--\nrdoc-file=string.c\n-->\nReturns",
                false,
                false,
                false
            ),
            ParsedMarkdownData {
                events: vec![
                    (2..30, RootStart),
                    (2..30, Start(HtmlBlock)),
                    (2..2, SubstitutedText("  ".into())),
                    (2..7, Html),
                    (7..26, Html),
                    (26..30, Html),
                    (2..30, End(MarkdownTagEnd::HtmlBlock)),
                    (2..30, RootEnd(0)),
                    (30..37, RootStart),
                    (30..37, Start(Paragraph)),
                    (30..37, Text),
                    (30..37, End(MarkdownTagEnd::Paragraph)),
                    (30..37, RootEnd(1)),
                ],
                root_block_starts: vec![2, 30],
                ..Default::default()
            }
        )
    }

    #[test]
    fn test_plain_urls_and_escaped_text() {
        assert_eq!(
            parse_markdown_with_options(
                "&nbsp;&nbsp; https://some.url some \\`&#9658;\\` text",
                false,
                false,
                false,
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..51, RootStart),
                    (0..51, Start(Paragraph)),
                    (0..6, SubstitutedText("\u{a0}".into())),
                    (6..12, SubstitutedText("\u{a0}".into())),
                    (12..13, Text),
                    (
                        13..29,
                        Start(Link {
                            link_type: LinkType::Autolink,
                            dest_url: "https://some.url".into(),
                            title: "".into(),
                            id: "".into(),
                        })
                    ),
                    (13..29, Text),
                    (13..29, End(MarkdownTagEnd::Link)),
                    (29..35, Text),
                    (36..37, Text), // Escaped backtick
                    (37..44, SubstitutedText("►".into())),
                    (45..46, Text), // Escaped backtick
                    (46..51, Text),
                    (0..51, End(MarkdownTagEnd::Paragraph)),
                    (0..51, RootEnd(0)),
                ],
                root_block_starts: vec![0],
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_incomplete_link() {
        assert_eq!(
            parse_markdown_with_options(
                "You can use the [GitHub Search API](https://docs.github.com/en",
                false,
                false,
                false,
            )
            .events,
            vec![
                (0..62, RootStart),
                (0..62, Start(Paragraph)),
                (0..16, Text),
                (16..17, Text),
                (17..34, Text),
                (34..35, Text),
                (35..36, Text),
                (
                    36..62,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://docs.github.com/en".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (36..62, Text),
                (36..62, End(MarkdownTagEnd::Link)),
                (0..62, End(MarkdownTagEnd::Paragraph)),
                (0..62, RootEnd(0)),
            ],
        );
    }

    #[test]
    fn test_smart_punctuation() {
        assert_eq!(
            parse_markdown_with_options(
                "-- --- ... \"double quoted\" 'single quoted' ----------",
                false,
                false,
                false,
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..53, RootStart),
                    (0..53, Start(Paragraph)),
                    (0..2, SubstitutedText("–".into())),
                    (2..3, Text),
                    (3..6, SubstitutedText("—".into())),
                    (6..7, Text),
                    (7..10, SubstitutedText("…".into())),
                    (10..11, Text),
                    (11..12, SubstitutedText("\u{201c}".into())),
                    (12..25, Text),
                    (25..26, SubstitutedText("\u{201d}".into())),
                    (26..27, Text),
                    (27..28, SubstitutedText("\u{2018}".into())),
                    (28..41, Text),
                    (41..42, SubstitutedText("\u{2019}".into())),
                    (42..43, Text),
                    (43..53, SubstitutedText("–––––".into())),
                    (0..53, End(MarkdownTagEnd::Paragraph)),
                    (0..53, RootEnd(0)),
                ],
                root_block_starts: vec![0],
                ..Default::default()
            }
        )
    }

    #[test]
    fn test_code_block_metadata() {
        assert_eq!(
            parse_markdown_with_options(
                "```rust\nfn main() {\n let a = 1;\n}\n```",
                false,
                false,
                false
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..37, RootStart),
                    (
                        0..37,
                        Start(CodeBlock {
                            kind: CodeBlockKind::FencedLang("rust".into()),
                            metadata: CodeBlockMetadata {
                                content_range: 8..34,
                                line_count: 3,
                                is_fenced_closed: true,
                            }
                        })
                    ),
                    (8..34, Text),
                    (0..37, End(MarkdownTagEnd::CodeBlock)),
                    (0..37, RootEnd(0)),
                ],
                language_names: {
                    let mut h = HashSet::default();
                    h.insert("rust".into());
                    h
                },
                root_block_starts: vec![0],
                ..Default::default()
            }
        );
        assert_eq!(
            parse_markdown_with_options("    fn main() {}", false, false, false),
            ParsedMarkdownData {
                events: vec![
                    (4..16, RootStart),
                    (
                        4..16,
                        Start(CodeBlock {
                            kind: CodeBlockKind::Indented,
                            metadata: CodeBlockMetadata {
                                content_range: 4..16,
                                line_count: 1,
                                is_fenced_closed: false,
                            }
                        })
                    ),
                    (4..16, Text),
                    (4..16, End(MarkdownTagEnd::CodeBlock)),
                    (4..16, RootEnd(0)),
                ],
                root_block_starts: vec![4],
                ..Default::default()
            }
        );

        for markdown in ["```mermaid\ngraph TD;\n~~~", "~~~~mermaid\ngraph TD;\n~~~"] {
            let parsed = parse_markdown_with_options(markdown, false, false, false);
            let metadata = parsed.events.iter().find_map(|(_, event)| match event {
                Start(CodeBlock { metadata, .. }) => Some(metadata),
                _ => None,
            });
            assert_eq!(
                metadata.map(|metadata| metadata.is_fenced_closed),
                Some(false)
            );
        }
    }

    fn assert_code_block_does_not_emit_links(markdown: &str) {
        let parsed = parse_markdown_with_options(markdown, false, false, false);
        let mut code_block_depth = 0;
        let mut code_block_count = 0;
        let mut saw_text_inside_code_block = false;

        for (_, event) in &parsed.events {
            match event {
                Start(CodeBlock { .. }) => {
                    code_block_depth += 1;
                    code_block_count += 1;
                }
                End(MarkdownTagEnd::CodeBlock) => {
                    assert!(
                        code_block_depth > 0,
                        "encountered a code block end without a matching start"
                    );
                    code_block_depth -= 1;
                }
                Start(Link { .. }) | End(MarkdownTagEnd::Link) => {
                    assert_eq!(
                        code_block_depth, 0,
                        "code blocks should not emit link events"
                    );
                }
                Text | SubstitutedText(_) if code_block_depth > 0 => {
                    saw_text_inside_code_block = true;
                }
                _ => {}
            }
        }

        assert_eq!(code_block_count, 1, "expected exactly one code block");
        assert_eq!(code_block_depth, 0, "unterminated code block");
        assert!(
            saw_text_inside_code_block,
            "expected text inside the code block"
        );
    }

    #[test]
    fn test_code_blocks_do_not_autolink_urls() {
        assert_code_block_does_not_emit_links("```txt\nhttps://example.com\n```");
        assert_code_block_does_not_emit_links("    https://example.com");
        assert_code_block_does_not_emit_links(
            "```txt\r\nhttps:/\\/example.com\r\nhttps://example&#46;com\r\n```",
        );
        assert_code_block_does_not_emit_links(
            "    https:/\\/example.com\r\n    https://example&#46;com",
        );
    }

    #[test]
    fn test_metadata_blocks_are_root_blocks() {
        assert_eq!(
            parse_markdown_with_options(
                "+++\ntitle = \"Example\"\n+++\n\nParagraph",
                false,
                false,
                true
            ),
            ParsedMarkdownData {
                events: vec![
                    (0..25, RootStart),
                    (0..25, Start(MetadataBlock(MetadataBlockKind::PlusesStyle))),
                    (4..22, Text),
                    (
                        0..25,
                        End(MarkdownTagEnd::MetadataBlock(
                            MetadataBlockKind::PlusesStyle
                        ))
                    ),
                    (0..25, RootEnd(0)),
                    (27..36, RootStart),
                    (27..36, Start(Paragraph)),
                    (27..36, Text),
                    (27..36, End(MarkdownTagEnd::Paragraph)),
                    (27..36, RootEnd(1)),
                ],
                root_block_starts: vec![0, 27],
                metadata_blocks: BTreeMap::from_iter([(
                    0,
                    ParsedMetadataBlock {
                        content_range: 4..22,
                        rows: None,
                    },
                )]),
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_metadata_blocks_are_omitted_by_default() {
        assert_eq!(
            parse_markdown_with_options(
                "+++\ntitle = \"Example\"\n+++\n\nParagraph",
                false,
                false,
                false
            ),
            ParsedMarkdownData {
                events: vec![
                    (27..36, RootStart),
                    (27..36, Start(Paragraph)),
                    (27..36, Text),
                    (27..36, End(MarkdownTagEnd::Paragraph)),
                    (27..36, RootEnd(0)),
                ],
                root_block_starts: vec![27],
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_table_checkboxes_remain_text_in_cells() {
        let markdown = "\
| Done | Task    |
|------|---------|
| [x]  | Fix bug |
| [ ]  | Add feature |";
        let parsed = parse_markdown_with_options(markdown, false, false, false);

        let mut in_table = false;
        let mut saw_task_list_marker = false;
        let mut cell_texts = Vec::new();
        let mut current_cell = String::new();

        for (range, event) in &parsed.events {
            match event {
                Start(Table(_)) => in_table = true,
                End(MarkdownTagEnd::Table) => in_table = false,
                Start(TableCell) => current_cell.clear(),
                End(MarkdownTagEnd::TableCell) => {
                    if in_table {
                        cell_texts.push(current_cell.clone());
                    }
                }
                Text if in_table => current_cell.push_str(&markdown[range.clone()]),
                TaskListMarker(_) if in_table => saw_task_list_marker = true,
                _ => {}
            }
        }

        let checkbox_cells: Vec<&str> = cell_texts
            .iter()
            .map(|cell| cell.trim())
            .filter(|cell| *cell == "[x]" || *cell == "[X]" || *cell == "[ ]")
            .collect();

        assert!(
            !saw_task_list_marker,
            "Table checkboxes should remain text, not task-list markers"
        );
        assert_eq!(checkbox_cells, vec!["[x]", "[ ]"]);
    }

    #[test]
    fn test_extract_code_content_range() {
        let input = "```let x = 5;```";
        assert_eq!(extract_code_content_range(input), 3..13);

        let input = "``let x = 5;``";
        assert_eq!(extract_code_content_range(input), 2..12);

        let input = "`let x = 5;`";
        assert_eq!(extract_code_content_range(input), 1..11);

        let input = "plain text";
        assert_eq!(extract_code_content_range(input), 0..10);

        let input = "``let x = 5;`";
        assert_eq!(extract_code_content_range(input), 0..13);
    }

    #[test]
    fn test_inline_code_substitutes_escaped_pipes() {
        let markdown = r"| Pattern |
| --- |
| `a\|b` |";
        let parsed = parse_markdown_with_options(markdown, false, false, false);
        let code_range = {
            let start = markdown.find(r"a\|b").expect("inline code source");
            start..start + r"a\|b".len()
        };

        assert!(
            parsed
                .events
                .iter()
                .any(|(range, event)| range == &code_range
                    && event == &SubstitutedCode("a|b".into())),
            "expected escaped pipe in table inline code to render as decoded inline code: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_inline_code_keeps_escaped_pipes_outside_tables() {
        let markdown = r"`a\|b`";
        let parsed = parse_markdown_with_options(markdown, false, false, false);

        assert!(
            parsed
                .events
                .iter()
                .any(|(range, event)| range == &(1..5) && event == &Code),
            "expected escaped pipe outside a table to remain normal inline code: {:?}",
            parsed.events
        );
    }

    #[test]
    fn test_extract_code_block_content_range() {
        let input = "```rust\nlet x = 5;\n```";
        assert_eq!(extract_code_block_content_range(input), 8..19);

        let input = "plain text";
        assert_eq!(extract_code_block_content_range(input), 0..10);

        let input = "```python\nprint('hello')\nprint('world')\n```";
        assert_eq!(extract_code_block_content_range(input), 10..40);

        let input = "~~~~mermaid\ngraph TD;\n~~~~";
        let content_range = extract_code_block_content_range(input);
        assert_eq!(&input[content_range], "graph TD;\n");

        let input = "~~~mermaid\ngraph TD;\n    ~~~~";
        let content_range = extract_code_block_content_range(input);
        assert_eq!(&input[content_range], "graph TD;\n    ~~~~");

        let input = "~~~mermaid\ngraph TD;\n   ~~~~ \t";
        let content_range = extract_code_block_content_range(input);
        assert_eq!(&input[content_range], "graph TD;\n");

        // Malformed input
        let input = "`````";
        assert_eq!(extract_code_block_content_range(input), 5..5);
    }

    #[test]
    fn test_footnotes() {
        let parsed = parse_markdown_with_options(
            "Text with a footnote[^1] and some more text.\n\n[^1]: This is the footnote content.",
            false,
            false,
            false,
        );
        assert_eq!(
            parsed.events,
            vec![
                (0..45, RootStart),
                (0..45, Start(Paragraph)),
                (0..20, Text),
                (20..24, FootnoteReference("1".into())),
                (24..44, Text),
                (0..45, End(MarkdownTagEnd::Paragraph)),
                (0..45, RootEnd(0)),
                (46..81, RootStart),
                (46..81, Start(FootnoteDefinition("1".into()))),
                (52..81, Start(Paragraph)),
                (52..81, Text),
                (52..81, End(MarkdownTagEnd::Paragraph)),
                (46..81, End(MarkdownTagEnd::FootnoteDefinition)),
                (46..81, RootEnd(1)),
            ]
        );
        assert_eq!(parsed.footnote_definitions.len(), 1);
        assert_eq!(parsed.footnote_definitions.get("1").copied(), Some(52));
    }

    #[test]
    fn test_footnote_definitions_multiple() {
        let parsed = parse_markdown_with_options(
            "Text[^a] and[^b].\n\n[^a]: First.\n\n[^b]: Second.",
            false,
            false,
            false,
        );
        assert_eq!(parsed.footnote_definitions.len(), 2);
        assert!(parsed.footnote_definitions.contains_key("a"));
        assert!(parsed.footnote_definitions.contains_key("b"));
    }

    #[test]
    fn test_links_split_across_fragments() {
        // This test verifies that links split across multiple text fragments due to escaping or other issues
        // are correctly detected and processed
        // Note: In real usage, pulldown_cmark creates separate text events for the escaped character
        // We're verifying our parser can handle this correctly
        assert_eq!(
            parse_markdown_with_options(
                "https:/\\/example.com is equivalent to https://example&#46;com!",
                false,
                false,
                false,
            )
            .events,
            vec![
                (0..62, RootStart),
                (0..62, Start(Paragraph)),
                (
                    0..20,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://example.com".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (0..7, Text),
                (8..20, Text),
                (0..20, End(MarkdownTagEnd::Link)),
                (20..38, Text),
                (
                    38..61,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://example.com".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (38..53, Text),
                (53..58, SubstitutedText(".".into())),
                (58..61, Text),
                (38..61, End(MarkdownTagEnd::Link)),
                (61..62, Text),
                (0..62, End(MarkdownTagEnd::Paragraph)),
                (0..62, RootEnd(0)),
            ],
        );

        assert_eq!(
            parse_markdown_with_options(
                "Visit https://example.com/cat\\/é&#8205;☕ for coffee!",
                false,
                false,
                false,
            )
            .events,
            [
                (0..55, RootStart),
                (0..55, Start(Paragraph)),
                (0..6, Text),
                (
                    6..43,
                    Start(Link {
                        link_type: LinkType::Autolink,
                        dest_url: "https://example.com/cat/é\u{200d}☕".into(),
                        title: "".into(),
                        id: "".into()
                    })
                ),
                (6..29, Text),
                (30..33, Text),
                (33..40, SubstitutedText("\u{200d}".into())),
                (40..43, Text),
                (6..43, End(MarkdownTagEnd::Link)),
                (43..55, Text),
                (0..55, End(MarkdownTagEnd::Paragraph)),
                (0..55, RootEnd(0)),
            ]
        );
    }

    #[test]
    fn test_heading_slugs() {
        let parsed = parse_markdown_with_options(
            "# Hello World\n\n## Code `block`\n\n### Third Level\n\n#### Fourth Level\n\n## Hello World",
            false,
            true,
            false,
        );
        assert_eq!(parsed.heading_slugs.len(), 5);
        assert!(parsed.heading_slugs.contains_key("hello-world"));
        assert!(parsed.heading_slugs.contains_key("code-block"));
        assert!(parsed.heading_slugs.contains_key("third-level"));
        assert!(parsed.heading_slugs.contains_key("fourth-level"));
        assert!(parsed.heading_slugs.contains_key("hello-world-1"));
    }

    #[test]
    fn test_heading_source_index_for_slug() {
        let parsed = parse_markdown_with_options(
            "# Duplicate\n\nText\n\n## Duplicate\n\nMore text",
            false,
            true,
            false,
        );
        let first = parsed.heading_slugs.get("duplicate").copied();
        let second = parsed.heading_slugs.get("duplicate-1").copied();
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(first.expect("first slug missing") < second.expect("second slug missing"));
    }

    #[test]
    fn test_heading_slug_collision_with_dedup_suffix() {
        let parsed = parse_markdown_with_options("# Foo\n\n## Foo\n\n## Foo 1", false, true, false);
        assert_eq!(parsed.heading_slugs.len(), 3);
        assert!(parsed.heading_slugs.contains_key("foo"));
        assert!(parsed.heading_slugs.contains_key("foo-1"));
        assert!(parsed.heading_slugs.contains_key("foo-1-1"));
    }

    #[test]
    fn test_gfm_alert_block_quote_kinds() {
        use pulldown_cmark::BlockQuoteKind;

        let markdown = "\n> [!NOTE]\n> A note.\n\n> [!TIP]\n> A tip.\n\n> [!IMPORTANT]\n> Important.\n\n> [!WARNING]\n> A warning.\n\n> [!CAUTION]\n> A caution.\n\n> Plain quote.\n";
        let parsed = parse_markdown_with_options(markdown, false, false, false);

        let block_quote_kinds: Vec<_> = parsed
            .events
            .iter()
            .filter_map(|(_, event)| match event {
                Start(BlockQuote(kind)) => Some(*kind),
                _ => None,
            })
            .collect();

        assert_eq!(
            block_quote_kinds,
            vec![
                Some(BlockQuoteKind::Note),
                Some(BlockQuoteKind::Tip),
                Some(BlockQuoteKind::Important),
                Some(BlockQuoteKind::Warning),
                Some(BlockQuoteKind::Caution),
                None,
            ]
        );
    }

    #[test]
    fn test_markdown_live_preview_blocks_extract_display_text() {
        let blocks = markdown_live_preview_blocks("# Heading\n\nThis is **strong** and `code`.\n");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, MarkdownLivePreviewBlockKind::Heading(1));
        assert_eq!(blocks[0].display_text, "Heading");
        assert_eq!(blocks[0].source_range, 0..9);
        assert_eq!(blocks[0].replacement_range, 0..10);
        assert_eq!(blocks[1].kind, MarkdownLivePreviewBlockKind::Paragraph);
        assert_eq!(blocks[1].display_text, "This is strong and code.");
        assert!(!blocks[0].source.ends_with('\n'));
        assert!(!blocks[1].source.ends_with('\n'));
    }

    #[test]
    fn test_markdown_live_preview_blocks_leave_last_separator_character_editable() {
        let blocks = markdown_live_preview_blocks("First paragraph.\n\n\nSecond paragraph.\n");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].source_range, 0..16);
        assert_eq!(blocks[0].replacement_range, 0..18);
        assert_eq!(blocks[1].source_range, 19..36);
    }

    #[test]
    fn test_markdown_live_preview_blocks_preserve_code_content() {
        let blocks = markdown_live_preview_blocks("```rust\nlet value = 1;\n```\n");

        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].kind,
            MarkdownLivePreviewBlockKind::CodeBlock {
                is_indented: false,
                is_mermaid: false,
            }
        );
        assert_eq!(blocks[0].display_text, "let value = 1;");
    }

    #[test]
    fn test_markdown_live_preview_blocks_preserve_indented_code_source() {
        let blocks =
            markdown_live_preview_blocks("    plain indented code\n    no syntax highlighting\n");

        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].kind,
            MarkdownLivePreviewBlockKind::CodeBlock {
                is_indented: true,
                is_mermaid: false,
            }
        );
        assert_eq!(
            blocks[0].display_text,
            "plain indented code\nno syntax highlighting"
        );
        assert_eq!(
            blocks[0].source.as_ref(),
            "    plain indented code\n    no syntax highlighting"
        );
    }

    #[test]
    fn test_markdown_live_preview_blocks_classify_mermaid_code_blocks() {
        let blocks = markdown_live_preview_blocks("```mermaid 150\ngraph TD;\n```\n");

        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].kind,
            MarkdownLivePreviewBlockKind::CodeBlock {
                is_indented: false,
                is_mermaid: true,
            }
        );
    }

    #[test]
    fn test_markdown_live_preview_blocks_classify_images() {
        let blocks = markdown_live_preview_blocks("![Alt text](image.png \"Title\")\n");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, MarkdownLivePreviewBlockKind::Image);
        assert_eq!(blocks[0].image_destination.as_deref(), Some("image.png"));
    }

    #[test]
    fn test_markdown_live_preview_blocks_preserve_image_destinations_with_spaces() {
        let blocks = markdown_live_preview_blocks("![Alt text](<my image.png> \"Title\")\n");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, MarkdownLivePreviewBlockKind::Image);
        assert_eq!(blocks[0].image_destination.as_deref(), Some("my image.png"));
    }

    #[test]
    fn test_markdown_live_preview_layout_hides_inline_markers() {
        use MarkdownLivePreviewInlineMarkerKind::Hidden;

        let layout = markdown_live_preview_layout(
            "# Title\n\nSome **bold** and *em* and `code` and [link](http://x).\n",
        );

        assert!(layout.blocks.is_empty());
        let markers = layout
            .inline_markers
            .iter()
            .map(|marker| (marker.range.clone(), marker.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            markers,
            vec![
                (0..2, Hidden),
                (14..16, Hidden),
                (20..22, Hidden),
                (27..28, Hidden),
                (30..31, Hidden),
                (36..37, Hidden),
                (41..42, Hidden),
                (47..48, Hidden),
                (52..63, Hidden),
            ]
        );
    }

    #[test]
    fn test_markdown_live_preview_layout_hides_setext_underline() {
        let layout = markdown_live_preview_layout("Title\n=====\n\nBody\n");
        let markers = layout
            .inline_markers
            .iter()
            .map(|marker| (marker.range.clone(), marker.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            markers,
            vec![(5..11, MarkdownLivePreviewInlineMarkerKind::Hidden)]
        );
    }

    #[test]
    fn test_markdown_live_preview_layout_substitutes_list_markers() {
        use MarkdownLivePreviewInlineMarkerKind::{Bullet, TaskListMarker};

        let layout = markdown_live_preview_layout("- item\n- [ ] task\n- [x] done\n1. ordered\n");
        let markers = layout
            .inline_markers
            .iter()
            .map(|marker| (marker.range.clone(), marker.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            markers,
            vec![
                (0..1, Bullet),
                (7..12, TaskListMarker { checked: false }),
                (18..23, TaskListMarker { checked: true }),
            ]
        );
    }

    #[test]
    fn test_markdown_live_preview_layout_substitutes_block_quote_markers() {
        use MarkdownLivePreviewInlineMarkerKind::BlockQuote;

        let layout = markdown_live_preview_layout("> quote\n> > nested\n");
        let markers = layout
            .inline_markers
            .iter()
            .map(|marker| (marker.range.clone(), marker.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            markers,
            vec![(0..1, BlockQuote), (8..9, BlockQuote), (10..11, BlockQuote)]
        );
    }

    #[test]
    fn test_markdown_live_preview_layout_only_replaces_rich_blocks() {
        let layout = markdown_live_preview_layout(
            "Para with **bold**\n\n```\n**not bold**\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n![img](x.png)\n\n---\n",
        );

        let kinds = layout
            .blocks
            .iter()
            .map(|block| block.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                MarkdownLivePreviewBlockKind::CodeBlock {
                    is_indented: false,
                    is_mermaid: false,
                },
                MarkdownLivePreviewBlockKind::Table,
                MarkdownLivePreviewBlockKind::Image,
                MarkdownLivePreviewBlockKind::Rule,
            ]
        );
        let markers = layout
            .inline_markers
            .iter()
            .map(|marker| marker.range.clone())
            .collect::<Vec<_>>();
        assert_eq!(markers, vec![10..12, 16..18]);
    }

    #[test]
    fn test_markdown_live_preview_blocks_preserve_gfm_alert_body() {
        let blocks =
            markdown_live_preview_blocks("> [!NOTE]\n> A note.\n\n> [!WARNING]\n> A warning.\n");

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, MarkdownLivePreviewBlockKind::BlockQuote);
        assert_eq!(blocks[0].source, "> [!NOTE]\n> A note.");
        assert_eq!(blocks[1].kind, MarkdownLivePreviewBlockKind::BlockQuote);
        assert_eq!(blocks[1].source, "> [!WARNING]\n> A warning.");
    }

    #[test]
    fn test_br_tag_emits_hard_break() {
        for input in [
            "hello<br>world",
            "hello<br/>world",
            "hello<br />world",
            "hello<br >world",
            "hello<BR>world",
            "hello<br class=\"x\">world",
            "hello<br class=\"x\"/>world",
        ] {
            let parsed = parse_markdown_with_options(input, true, false, false);
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            let has_empty_substituted_text = parsed.events.iter().any(|(_, event)| {
                matches!(event, MarkdownEvent::SubstitutedText(text) if text.is_empty())
            });
            assert!(has_hard_break, "<br> in \"{input}\" should emit HardBreak");
            assert!(
                !has_empty_substituted_text,
                "<br> in \"{input}\" should not produce empty SubstitutedText"
            );
        }
    }

    #[test]
    fn test_br_tag_not_a_hard_break_without_parse_html() {
        for input in ["hello<br>world", "hello<br/>world", "hello<br />world"] {
            let parsed = parse_markdown_with_options(input, false, false, false);
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            let has_inline_html = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml));
            assert!(
                !has_hard_break,
                "<br> in \"{input}\" should not emit HardBreak when parse_html is disabled"
            );
            assert!(
                has_inline_html,
                "<br> in \"{input}\" should be preserved as InlineHtml when parse_html is disabled"
            );
        }
    }

    #[test]
    fn test_br_prefixed_tag_is_not_a_hard_break() {
        for input in ["a<break>b", "a<brick>b", "a<b>bold</b>c"] {
            let parsed = parse_markdown_with_options(input, true, false, false);
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            assert!(
                !has_hard_break,
                "\"{input}\" should not be treated as a <br> hard break"
            );
        }
    }

    #[test]
    fn test_unrecognized_inline_html_preserved_as_inline_html() {
        for input in ["a<span>b</span>c", "a<em>b</em>c", "a<strong>b</strong>c"] {
            let parsed = parse_markdown_with_options(input, false, false, false);
            let has_inline_html = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::InlineHtml));
            let has_hard_break = parsed
                .events
                .iter()
                .any(|(_, event)| matches!(event, MarkdownEvent::HardBreak));
            assert!(
                has_inline_html,
                "unrecognized inline HTML \"{input}\" should emit InlineHtml"
            );
            assert!(
                !has_hard_break,
                "unrecognized inline HTML \"{input}\" should not emit HardBreak"
            );
        }
    }
}
