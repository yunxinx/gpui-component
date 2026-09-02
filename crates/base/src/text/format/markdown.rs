use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::SharedString;
use markdown::mdast::{self, Node};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory as _};
use unicode_script::{Script, UnicodeScript as _};

use crate::text::{
    document::ParsedDocument,
    markdown_ext::MarkdownParseContext,
    node::{
        self, BlockNode, CodeBlock, ImageNode, InlineNode, LinkMark, NodeContext, Paragraph, Span,
        Table, TableRow, TextMark,
    },
};

/// Parse Markdown into a tree of nodes.
pub(crate) fn parse(source: &str, cx: &mut NodeContext) -> Result<ParsedDocument, SharedString> {
    let options = cx.markdown_extensions.configured_parse_options();
    let prepared_source = cx.markdown_extensions.prepared_source(source)?;
    parse_mdast_with_cjk_compatibility(
        source,
        prepared_source,
        &options,
        cx.markdown_extensions.cjk_emphasis_compatibility_enabled(),
    )
    .map(|(prepared_source, root)| ast_to_document(source, &prepared_source, root, cx))
    .map_err(|e| e.to_string().into())
}

/// Parse the prepared source, then optionally retry with the narrow CJK
/// punctuation-adjacent emphasis exception.
///
/// The compatibility pass replaces the offending CJK punctuation with private
/// placeholders of the same byte length, reparses, verifies that exactly the
/// expected emphasis/strong nodes appeared, and restores the original
/// characters in the resulting text nodes. Any failure along the way falls back
/// to the strict CommonMark result.
fn parse_mdast_with_cjk_compatibility(
    source: &str,
    prepared_source: String,
    options: &markdown::ParseOptions,
    enabled: bool,
) -> Result<(String, Node), markdown::message::Message> {
    let root = markdown::to_mdast(&prepared_source, options)?;
    if !enabled {
        return Ok((prepared_source, root));
    }

    let Some(preparation) = prepare_cjk_attention_source(source, &prepared_source, &root) else {
        return Ok((prepared_source, root));
    };
    let Ok(mut compatible_root) = markdown::to_mdast(&preparation.source, options) else {
        return Ok((prepared_source, root));
    };
    if !cjk_attention_nodes_match(&compatible_root, &preparation.matches) {
        return Ok((prepared_source, root));
    }
    if !restore_cjk_attention_placeholders(
        &mut compatible_root,
        source,
        &preparation.source,
        preparation.placeholder,
    ) {
        return Ok((prepared_source, root));
    }

    // The compatibility placeholders are an internal parsing aid. Extension
    // callbacks must continue to observe the caller's prepared source rather
    // than that private view.
    Ok((prepared_source, compatible_root))
}

fn parse_table_row(
    table: &mut Table,
    node: &mdast::TableRow,
    parse_cx: &MarkdownParseContext<'_>,
    cx: &mut NodeContext,
) {
    let mut row = TableRow::default();
    node.children.iter().for_each(|c| {
        match c {
            Node::TableCell(cell) => {
                parse_table_cell(&mut row, cell, parse_cx, cx);
            }
            _ => {}
        };
    });
    table.children.push(row);
}

fn parse_table_cell(
    row: &mut node::TableRow,
    node: &mdast::TableCell,
    parse_cx: &MarkdownParseContext<'_>,
    cx: &mut NodeContext,
) {
    let mut paragraph = Paragraph::default();
    node.children.iter().for_each(|c| {
        parse_paragraph(&mut paragraph, c, parse_cx, cx);
    });
    let table_cell = node::TableCell {
        children: paragraph,
        ..Default::default()
    };
    row.children.push(table_cell);
}

/// Push a text run with its existing `marks` plus `new_mark` across the full
/// run.
///
/// If the last mark already covers the full run, merge into it. Otherwise add a
/// new full-run mark. Empty runs are skipped so callers can flush freely.
fn push_merged(
    paragraph: &mut Paragraph,
    text: String,
    marks: Vec<(Range<usize>, TextMark)>,
    new_mark: TextMark,
) {
    if text.is_empty() {
        return;
    }

    let mut node = InlineNode::new(text).marks(marks);
    let len = node.text.len();
    if let Some(last) = node.marks.last_mut()
        && last.0.start == 0
        && last.0.end == len
    {
        last.1.merge(new_mark);
    } else {
        node.marks.push((0..len, new_mark));
    }
    paragraph.push(node);
}

/// Parse `children` and apply `mark` across each emitted text run.
///
/// Nested child marks are kept and shifted to match the combined text for the
/// current run, which lets nested emphasis like `**_x_**` render as both bold
/// and italic. Inline images split the run and are emitted as sibling image
/// nodes. The return value is the plain text from all children, for callers that
/// need to pass text back to their parent node.
fn merge_children_with_mark(
    paragraph: &mut Paragraph,
    children: &[mdast::Node],
    mark: TextMark,
    parse_cx: &MarkdownParseContext<'_>,
    cx: &mut NodeContext,
) -> String {
    let mut text = String::new();
    let mut merged_text = String::new();
    let mut merged_marks = Vec::new();

    for child in children {
        let mut child_paragraph = Paragraph::default();
        let child_text = parse_paragraph(&mut child_paragraph, child, parse_cx, cx);
        text.push_str(&child_text);

        for mut node in child_paragraph.children {
            if node.custom.is_some() {
                push_merged(
                    paragraph,
                    std::mem::take(&mut merged_text),
                    std::mem::take(&mut merged_marks),
                    mark.clone(),
                );
                node.merge_full_mark(mark.clone());
                paragraph.push(node);
                continue;
            }

            let merged_offset = merged_text.len();
            merged_text.push_str(&node.text);

            for (range, child_mark) in node.marks {
                merged_marks.push((
                    range.start + merged_offset..range.end + merged_offset,
                    child_mark,
                ));
            }

            if let Some(mut image) = node.image {
                if let Some(link_mark) = mark.link.clone() {
                    image.link = Some(link_mark);
                }

                // GPUI InteractiveText does not support inline images, so
                // flush the accumulated text run and emit the image as its
                // own sibling InlineNode.
                push_merged(
                    paragraph,
                    std::mem::take(&mut merged_text),
                    std::mem::take(&mut merged_marks),
                    mark.clone(),
                );
                let image = if let Some(identifier) = node.image_reference_identifier {
                    InlineNode::reference_image(image, identifier)
                } else {
                    InlineNode::image(image)
                };
                paragraph.push(image);
            }
        }
    }

    push_merged(paragraph, merged_text, merged_marks, mark);
    text
}

fn append_inline_html_blocks(paragraph: &mut Paragraph, blocks: Vec<BlockNode>) -> Option<String> {
    let mut text = String::new();

    for block in blocks {
        match block {
            BlockNode::Root { children, .. } => {
                text.push_str(&append_inline_html_blocks(paragraph, children)?);
            }
            BlockNode::Paragraph(html_paragraph) => {
                text.push_str(&html_paragraph.text());
                for child in html_paragraph.children {
                    paragraph.push(child);
                }
            }
            BlockNode::Break { .. } => {
                text.push('\n');
                paragraph.push(InlineNode::new("\n"));
            }
            _ => return None,
        }
    }

    Some(text)
}

fn parse_paragraph(
    paragraph: &mut Paragraph,
    node: &mdast::Node,
    parse_cx: &MarkdownParseContext<'_>,
    cx: &mut NodeContext,
) -> String {
    let span = node.position().map(|pos| Span {
        start: cx.offset + pos.start.offset,
        end: cx.offset + pos.end.offset,
    });
    if let Some(span) = span {
        paragraph.set_span(span);
    }

    let mut text = String::new();

    if let Some(mut custom) = cx.markdown_extensions.parse_inline(node, parse_cx) {
        let source = parse_cx.node_source(node).unwrap_or_default();
        custom.ensure_fallback(source);
        custom.set_span(span);
        text.push_str(custom.as_text());
        paragraph.push(InlineNode::custom(custom));
        return text;
    }

    match node {
        Node::Paragraph(val) => {
            val.children.iter().for_each(|c| {
                text.push_str(&parse_paragraph(paragraph, c, parse_cx, cx));
            });
        }
        Node::Text(val) => {
            text = val.value.clone();
            paragraph.push_str(&val.value)
        }
        Node::Emphasis(val) => {
            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark::default().italic(),
                parse_cx,
                cx,
            );
        }
        Node::Strong(val) => {
            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark::default().bold(),
                parse_cx,
                cx,
            );
        }
        Node::Delete(val) => {
            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark::default().strikethrough(),
                parse_cx,
                cx,
            );
        }
        Node::InlineCode(val) => {
            text = val.value.clone();
            paragraph.push(
                InlineNode::new(&text).marks(vec![(0..text.len(), TextMark::default().code())]),
            );
        }
        Node::Link(val) => {
            let link_mark = Some(LinkMark {
                url: val.url.clone().into(),
                title: val.title.clone().map(|s| s.into()),
                ..Default::default()
            });

            text = merge_children_with_mark(
                paragraph,
                &val.children,
                TextMark {
                    link: link_mark,
                    ..Default::default()
                },
                parse_cx,
                cx,
            );
        }
        Node::Image(raw) => {
            let alt = parse_cx
                .authoritative_image_alt(node)
                .cloned()
                .unwrap_or_else(|| raw.alt.clone().into());
            paragraph.push_image(ImageNode {
                url: raw.url.clone().into(),
                title: raw.title.clone().map(|t| t.into()),
                alt: Some(alt),
                ..Default::default()
            });
        }
        Node::ImageReference(raw) => {
            if let Some(reference) = cx.link_refs.get(raw.identifier.as_str()) {
                let alt = parse_cx
                    .authoritative_image_alt(node)
                    .cloned()
                    .unwrap_or_else(|| raw.alt.clone().into());
                paragraph.push_reference_image(
                    ImageNode {
                        url: reference.url.clone().into(),
                        title: reference.title.clone(),
                        alt: Some(alt),
                        ..Default::default()
                    },
                    raw.identifier.clone().into(),
                );
            } else {
                text = parse_cx
                    .node_source(node)
                    .map(str::to_string)
                    .unwrap_or_else(|| raw.alt.clone());
                paragraph.push_str(&text);
            }
        }
        Node::InlineMath(raw) => {
            text = parse_cx
                .node_source(node)
                .map(str::to_string)
                .unwrap_or_else(|| raw.value.clone());
            paragraph.push(
                InlineNode::new(&text).marks(vec![(0..text.len(), TextMark::default().code())]),
            );
        }
        Node::Break(_) => {
            text.push('\n');
            paragraph.push(InlineNode::new("\n"));
        }
        Node::MdxTextExpression(raw) => {
            text = raw.value.clone();
            paragraph
                .push(InlineNode::new(&text).marks(vec![(0..text.len(), TextMark::default())]));
        }
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) => {
                if let Some(inline_text) =
                    append_inline_html_blocks(paragraph, Arc::unwrap_or_clone(el.blocks))
                {
                    text = inline_text;
                } else {
                    if cfg!(debug_assertions) {
                        tracing::warn!("unsupported inline html tag: {:#?}", val.value);
                    }
                }
            }
            Err(err) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("failed parsing html: {:#?}", err);
                }

                text.push_str(&val.value);
            }
        },
        Node::FootnoteReference(foot) => {
            let prefix = format!("[{}]", foot.identifier);
            paragraph.push(InlineNode::new(&prefix).marks(vec![(
                0..prefix.len(),
                TextMark {
                    italic: true,
                    ..Default::default()
                },
            )]));
        }
        Node::LinkReference(link) => {
            let link_mark = LinkMark {
                url: "".into(),
                title: link.label.clone().map(Into::into),
                identifier: Some(link.identifier.clone().into()),
            };

            text = merge_children_with_mark(
                paragraph,
                &link.children,
                TextMark {
                    link: Some(link_mark),
                    ..Default::default()
                },
                parse_cx,
                cx,
            );
        }
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported inline node: {:#?}", node);
            }
        }
    }

    text
}

/// Image alt text as written in the original source, keyed by the image
/// node's byte range, for every image whose bytes a source preparer rewrote.
///
/// Source preparation only produces a parse view; user-visible alt text must
/// come from the authoritative source. When no image was touched the map is
/// empty and no second parse happens.
fn authoritative_image_alts(
    source: &str,
    prepared_source: &str,
    prepared_root: &mdast::Node,
) -> HashMap<Range<usize>, SharedString> {
    if source == prepared_source
        || !prepared_image_source_changed(prepared_root, source, prepared_source)
    {
        return HashMap::new();
    }

    let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) else {
        return HashMap::new();
    };
    let mut alts = HashMap::new();
    collect_authoritative_image_alts(&root, &mut alts);
    alts
}

fn prepared_image_source_changed(node: &mdast::Node, source: &str, prepared_source: &str) -> bool {
    if matches!(node, Node::Image(_) | Node::ImageReference(_))
        && let Some(position) = node.position()
    {
        let range = position.start.offset..position.end.offset;
        if source.get(range.clone()) != prepared_source.get(range) {
            return true;
        }
    }
    node.children().is_some_and(|children| {
        children
            .iter()
            .any(|child| prepared_image_source_changed(child, source, prepared_source))
    })
}

fn collect_authoritative_image_alts(
    node: &mdast::Node,
    alts: &mut HashMap<Range<usize>, SharedString>,
) {
    let alt = match node {
        Node::Image(image) => Some(image.alt.as_str()),
        Node::ImageReference(image) => Some(image.alt.as_str()),
        _ => None,
    };
    if let Some(alt) = alt
        && let Some(position) = node.position()
    {
        alts.insert(
            position.start.offset..position.end.offset,
            alt.to_string().into(),
        );
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_authoritative_image_alts(child, alts);
        }
    }
}

fn ast_to_document(
    source: &str,
    prepared_source: &str,
    root: mdast::Node,
    cx: &mut NodeContext,
) -> ParsedDocument {
    let authoritative_image_alts = authoritative_image_alts(source, prepared_source, &root);
    let parse_cx = MarkdownParseContext::new(
        source,
        prepared_source,
        cx.offset,
        &authoritative_image_alts,
    );
    let root = match root {
        Node::Root(r) => r,
        _ => panic!("expected root node"),
    };

    let blocks = root
        .children
        .into_iter()
        .map(|c| ast_to_node(c, &parse_cx, cx))
        .collect();
    ParsedDocument {
        source: source.to_string().into(),
        blocks: Arc::new(blocks),
    }
}

fn new_span(pos: Option<markdown::unist::Position>, cx: &NodeContext) -> Option<Span> {
    let pos = pos?;

    Some(Span {
        start: cx.offset + pos.start.offset,
        end: cx.offset + pos.end.offset,
    })
}

fn ast_to_node(
    value: mdast::Node,
    parse_cx: &MarkdownParseContext<'_>,
    cx: &mut NodeContext,
) -> BlockNode {
    let span = new_span(value.position().cloned(), cx);
    if let Some(mut node) = cx.markdown_extensions.parse_block(&value, parse_cx) {
        node.set_span(span);
        return BlockNode::Custom(node);
    }

    match value {
        Node::Root(_) => unreachable!("node::Root should be handled separately"),
        Node::Paragraph(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, parse_cx, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::Blockquote(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(c, parse_cx, cx))
                .collect();
            BlockNode::Blockquote {
                children,
                span: new_span(val.position, cx),
            }
        }
        Node::List(list) => {
            let children = list
                .children
                .into_iter()
                .map(|c| ast_to_node(c, parse_cx, cx))
                .collect();
            BlockNode::List {
                ordered: list.ordered,
                children,
                span: new_span(list.position, cx),
            }
        }
        Node::ListItem(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(c, parse_cx, cx))
                .collect();
            BlockNode::ListItem {
                children,
                spread: val.spread,
                checked: val.checked,
                span: new_span(val.position, cx),
            }
        }
        Node::Break(val) => BlockNode::Break {
            html: false,
            span: new_span(val.position, cx),
        },
        Node::Code(raw) => BlockNode::CodeBlock(CodeBlock::new(
            raw.value.into(),
            raw.lang.map(|s| s.into()),
            new_span(raw.position, cx),
        )),
        Node::Heading(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, parse_cx, cx);
            });

            BlockNode::Heading {
                level: val.depth,
                children: paragraph,
                span: new_span(val.position, cx),
            }
        }
        Node::Math(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            None,
            new_span(val.position, cx),
        )),
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) => BlockNode::Root {
                children: Arc::unwrap_or_clone(el.blocks),
                span: new_span(val.position, cx),
            },
            Err(err) => {
                if cfg!(debug_assertions) {
                    tracing::warn!("error parsing html: {:#?}", err);
                }

                BlockNode::Paragraph(Paragraph::new(val.value))
            }
        },
        Node::MdxFlowExpression(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("mdx".into()),
            new_span(val.position, cx),
        )),
        Node::Yaml(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("yml".into()),
            new_span(val.position, cx),
        )),
        Node::Toml(val) => BlockNode::CodeBlock(CodeBlock::new(
            val.value.into(),
            Some("toml".into()),
            new_span(val.position, cx),
        )),
        Node::MdxJsxTextElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, parse_cx, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::MdxJsxFlowElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, parse_cx, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::ThematicBreak(val) => BlockNode::HorizontalRule {
            span: new_span(val.position, cx),
        },
        Node::Table(val) => {
            let mut table = Table::default();
            table.column_aligns = val
                .align
                .clone()
                .into_iter()
                .map(|align| align.into())
                .collect();
            val.children.iter().for_each(|c| {
                if let Node::TableRow(row) = c {
                    parse_table_row(&mut table, row, parse_cx, cx);
                }
            });
            table.span = new_span(val.position, cx);

            BlockNode::Table(table)
        }
        Node::FootnoteDefinition(def) => {
            let mut paragraph = Paragraph::default();
            let prefix = format!("[{}]: ", def.identifier);
            paragraph.push(InlineNode::new(&prefix).marks(vec![(
                0..prefix.len(),
                TextMark {
                    italic: true,
                    ..Default::default()
                },
            )]));

            def.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, parse_cx, cx);
            });
            paragraph.span = new_span(def.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::Definition(def) => {
            cx.add_ref(
                def.identifier.clone().into(),
                LinkMark {
                    url: def.url.clone().into(),
                    identifier: Some(def.identifier.clone().into()),
                    title: def.title.clone().map(Into::into),
                },
            );

            BlockNode::Definition {
                identifier: def.identifier.clone().into(),
                url: def.url.clone().into(),
                title: def.title.clone().map(|s| s.into()),
                span: new_span(def.position, cx),
            }
        }
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported node: {:#?}", value);
            }
            BlockNode::Unknown
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CjkAttentionKind {
    Emphasis,
    Strong,
}

impl CjkAttentionKind {
    fn marker_len(self) -> usize {
        match self {
            Self::Emphasis => 1,
            Self::Strong => 2,
        }
    }
}

struct CjkAttentionMatch {
    range: Range<usize>,
    kind: CjkAttentionKind,
    opening_punctuation: Option<Range<usize>>,
    closing_punctuation: Option<Range<usize>>,
}

struct CjkAttentionPreparation {
    source: String,
    placeholder: char,
    matches: Vec<(Range<usize>, CjkAttentionKind)>,
}

const CJK_ATTENTION_PLACEHOLDERS: &[char] = &[
    '\u{e000}', '\u{e001}', '\u{e002}', '\u{e003}', '\u{e004}', '\u{e005}', '\u{e006}', '\u{e007}',
];

/// Prepare a private, byte-aligned parse view for the narrow CJK punctuation
/// exception. Marker eligibility comes from the strict mdast's Text ranges, so
/// code, math, HTML syntax, images, and link destinations remain opaque. The
/// marker pair may still span native inline children such as a link label.
fn prepare_cjk_attention_source(
    source: &str,
    prepared_source: &str,
    root: &Node,
) -> Option<CjkAttentionPreparation> {
    let mut attention_matches = Vec::new();
    collect_cjk_attention_matches(root, source, prepared_source, &mut attention_matches);
    if attention_matches.is_empty() {
        return None;
    }

    let placeholder = CJK_ATTENTION_PLACEHOLDERS
        .iter()
        .copied()
        .find(|placeholder| {
            !source.contains(*placeholder) && !prepared_source.contains(*placeholder)
        })?;
    let mut punctuation_ranges = attention_matches
        .iter()
        .flat_map(|attention| {
            [
                attention.opening_punctuation.clone(),
                attention.closing_punctuation.clone(),
            ]
            .into_iter()
            .flatten()
        })
        .collect::<Vec<_>>();
    punctuation_ranges.sort_by_key(|range| range.start);
    punctuation_ranges.dedup();
    if punctuation_ranges
        .windows(2)
        .any(|ranges| ranges[0].end > ranges[1].start)
        || punctuation_ranges
            .iter()
            .any(|range| range.len() != placeholder.len_utf8())
    {
        return None;
    }

    let mut compatible_source = prepared_source.to_string();
    let placeholder_text = placeholder.to_string();
    for range in punctuation_ranges {
        compatible_source.replace_range(range, &placeholder_text);
    }

    Some(CjkAttentionPreparation {
        source: compatible_source,
        placeholder,
        matches: attention_matches
            .into_iter()
            .map(|attention| (attention.range, attention.kind))
            .collect(),
    })
}

fn collect_cjk_attention_matches(
    node: &Node,
    source: &str,
    prepared_source: &str,
    matches: &mut Vec<CjkAttentionMatch>,
) {
    if matches!(
        node,
        Node::Paragraph(_) | Node::Heading(_) | Node::TableCell(_) | Node::MdxJsxTextElement(_)
    ) {
        let mut text_ranges = Vec::new();
        collect_text_ranges(node, source, prepared_source, &mut text_ranges);
        text_ranges.sort_by_key(|range| range.start);
        let Some(scan_start) = text_ranges.first().map(|range| range.start) else {
            return;
        };
        let scan_end = text_ranges
            .last()
            .map(|range| range.end)
            .unwrap_or(scan_start);
        let mut cursor = scan_start;
        while let Some(attention) =
            find_cjk_attention(source, prepared_source, &text_ranges, cursor, scan_end)
        {
            cursor = attention.range.end;
            matches.push(attention);
        }
        return;
    }

    if let Some(children) = node.children() {
        for child in children {
            collect_cjk_attention_matches(child, source, prepared_source, matches);
        }
    }
}

fn collect_text_ranges(
    node: &Node,
    source: &str,
    prepared_source: &str,
    ranges: &mut Vec<Range<usize>>,
) {
    if matches!(node, Node::Text(_))
        && let Some(position) = node.position()
    {
        let range = position.start.offset..position.end.offset;
        if range.start < range.end
            && source.get(range.clone()).is_some()
            && prepared_source.get(range.clone()).is_some()
        {
            ranges.push(range);
        }
        return;
    }

    if let Some(children) = node.children() {
        for child in children {
            collect_text_ranges(child, source, prepared_source, ranges);
        }
    }
}

/// Find the narrow punctuation-adjacent emphasis form commonly emitted in CJK
/// prose but rejected by CommonMark's deliberately conservative flanking rule.
/// At least one side must actually require the CJK exception; ordinary native
/// Markdown remains the strict parser's responsibility.
fn find_cjk_attention(
    source: &str,
    prepared_source: &str,
    text_ranges: &[Range<usize>],
    from: usize,
    scan_end: usize,
) -> Option<CjkAttentionMatch> {
    let mut search = from;
    while search < scan_end {
        let relative = prepared_source.get(search..scan_end)?.find('*')?;
        let open = search + relative;
        let run_length = attention_run_length(prepared_source, open);
        let kind = match run_length {
            1 => CjkAttentionKind::Emphasis,
            2 => CjkAttentionKind::Strong,
            _ => {
                search = open + run_length;
                continue;
            }
        };
        let marker_len = kind.marker_len();
        if !attention_marker_is_eligible(source, prepared_source, text_ranges, open, marker_len) {
            search = open + run_length;
            continue;
        }

        let content_start = open + marker_len;
        let ordinary_open = is_attention_open(
            prepared_source[..open].chars().next_back(),
            prepared_source.get(content_start..)?.chars().next(),
        );
        let cjk_open = is_cjk_attention_open(
            source[..open].chars().next_back(),
            source.get(content_start..)?.chars().next(),
        );
        let opening_punctuation = if ordinary_open {
            None
        } else if cjk_open {
            let range = next_character_range(source, content_start)?;
            compatible_punctuation_range(source, prepared_source, text_ranges, range)
        } else {
            search = open + run_length;
            continue;
        };
        if !ordinary_open && opening_punctuation.is_none() {
            search = open + run_length;
            continue;
        }

        if let Some((close, closing_punctuation)) = find_cjk_attention_close(
            source,
            prepared_source,
            text_ranges,
            content_start,
            scan_end,
            kind,
        ) && (opening_punctuation.is_some() || closing_punctuation.is_some())
        {
            return Some(CjkAttentionMatch {
                range: open..close + marker_len,
                kind,
                opening_punctuation,
                closing_punctuation,
            });
        }

        search = open + run_length;
    }
    None
}

fn find_cjk_attention_close(
    source: &str,
    prepared_source: &str,
    text_ranges: &[Range<usize>],
    content_start: usize,
    scan_end: usize,
    kind: CjkAttentionKind,
) -> Option<(usize, Option<Range<usize>>)> {
    let marker_len = kind.marker_len();
    let mut search = content_start;
    while search < scan_end {
        let relative = prepared_source.get(search..scan_end)?.find('*')?;
        let close = search + relative;
        let run_length = attention_run_length(prepared_source, close);
        if run_length != marker_len
            || !attention_marker_is_eligible(
                source,
                prepared_source,
                text_ranges,
                close,
                marker_len,
            )
        {
            search = close + run_length;
            continue;
        }

        let ordinary_close = is_attention_close(
            prepared_source[..close].chars().next_back(),
            prepared_source.get(close + marker_len..)?.chars().next(),
        );
        if ordinary_close {
            return Some((close, None));
        }

        let cjk_close = is_cjk_attention_close(
            source[..close].chars().next_back(),
            source.get(close + marker_len..)?.chars().next(),
        );
        if cjk_close
            && let Some(range) = previous_character_range(source, close)
            && let Some(range) =
                compatible_punctuation_range(source, prepared_source, text_ranges, range)
        {
            return Some((close, Some(range)));
        }

        search = close + run_length;
    }
    None
}

fn attention_marker_is_eligible(
    source: &str,
    prepared_source: &str,
    text_ranges: &[Range<usize>],
    marker_start: usize,
    marker_len: usize,
) -> bool {
    let marker_range = marker_start..marker_start + marker_len;
    range_is_text(marker_range.clone(), text_ranges)
        && attention_run_length(source, marker_start) == marker_len
        && source.get(marker_range.clone()) == prepared_source.get(marker_range)
        && !marker_is_escaped(source, marker_start)
        && !marker_is_escaped(prepared_source, marker_start)
        && (marker_start == 0 || source.as_bytes()[marker_start - 1] != b'*')
        && (marker_start == 0 || prepared_source.as_bytes()[marker_start - 1] != b'*')
}

fn compatible_punctuation_range(
    source: &str,
    prepared_source: &str,
    text_ranges: &[Range<usize>],
    range: Range<usize>,
) -> Option<Range<usize>> {
    (range.len() == 3
        && range_is_text(range.clone(), text_ranges)
        && source.get(range.clone()) == prepared_source.get(range.clone()))
    .then_some(range)
}

fn range_is_text(range: Range<usize>, text_ranges: &[Range<usize>]) -> bool {
    text_ranges
        .iter()
        .any(|text_range| text_range.start <= range.start && text_range.end >= range.end)
}

fn marker_is_escaped(text: &str, marker_start: usize) -> bool {
    text.as_bytes()[..marker_start]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

fn next_character_range(text: &str, start: usize) -> Option<Range<usize>> {
    let character = text.get(start..)?.chars().next()?;
    Some(start..start + character.len_utf8())
}

fn previous_character_range(text: &str, end: usize) -> Option<Range<usize>> {
    let (start, _) = text.get(..end)?.char_indices().next_back()?;
    Some(start..end)
}

fn cjk_attention_nodes_match(root: &Node, expected: &[(Range<usize>, CjkAttentionKind)]) -> bool {
    expected
        .iter()
        .all(|(range, kind)| cjk_attention_node_matches(root, range, *kind))
}

fn cjk_attention_node_matches(
    node: &Node,
    expected_range: &Range<usize>,
    expected_kind: CjkAttentionKind,
) -> bool {
    let kind_matches = matches!(
        (node, expected_kind),
        (Node::Emphasis(_), CjkAttentionKind::Emphasis)
            | (Node::Strong(_), CjkAttentionKind::Strong)
    );
    if kind_matches
        && node.position().is_some_and(|position| {
            position.start.offset == expected_range.start
                && position.end.offset == expected_range.end
        })
    {
        return true;
    }

    node.children().is_some_and(|children| {
        children
            .iter()
            .any(|child| cjk_attention_node_matches(child, expected_range, expected_kind))
    })
}

fn restore_cjk_attention_placeholders(
    root: &mut Node,
    source: &str,
    compatible_source: &str,
    placeholder: char,
) -> bool {
    let placeholder_text = placeholder.to_string();
    let replacements = compatible_source
        .match_indices(placeholder)
        .filter_map(|(offset, _)| {
            source
                .get(offset..)
                .and_then(|source| source.chars().next())
                .map(|character| (offset, character))
        })
        .collect::<Vec<_>>();
    if replacements.is_empty()
        || replacements.iter().any(|(offset, character)| {
            character.len_utf8() != placeholder.len_utf8()
                || compatible_source.get(*offset..*offset + placeholder.len_utf8())
                    != Some(placeholder_text.as_str())
        })
    {
        return false;
    }

    let mut restored = 0;
    restore_cjk_attention_node(root, &replacements, placeholder, &mut restored)
        && restored == replacements.len()
}

fn restore_cjk_attention_node(
    node: &mut Node,
    replacements: &[(usize, char)],
    placeholder: char,
    restored: &mut usize,
) -> bool {
    if let Node::Text(text) = node {
        let Some(position) = text.position.as_ref() else {
            return false;
        };
        let replacement_characters = replacements
            .iter()
            .filter(|(offset, _)| *offset >= position.start.offset && *offset < position.end.offset)
            .map(|(_, character)| *character)
            .collect::<Vec<_>>();
        if text
            .value
            .chars()
            .filter(|character| *character == placeholder)
            .count()
            != replacement_characters.len()
        {
            return false;
        }
        if !replacement_characters.is_empty() {
            let replacement_count = replacement_characters.len();
            let mut characters = replacement_characters.into_iter();
            text.value = text
                .value
                .chars()
                .map(|character| {
                    if character == placeholder {
                        characters.next().expect("placeholder count was validated")
                    } else {
                        character
                    }
                })
                .collect();
            *restored += replacement_count;
        }
        return true;
    }

    if let Some(children) = node.children_mut() {
        for child in children {
            if !restore_cjk_attention_node(child, replacements, placeholder, restored) {
                return false;
            }
        }
    }
    true
}

fn attention_run_length(text: &str, marker_start: usize) -> usize {
    text.as_bytes()[marker_start..]
        .iter()
        .take_while(|byte| **byte == b'*')
        .count()
}

fn is_attention_open(previous: Option<char>, next: Option<char>) -> bool {
    next.is_some_and(|character| !character.is_whitespace())
        && (!next.is_some_and(is_markdown_punctuation)
            || previous.is_none_or(|character| {
                character.is_whitespace() || is_markdown_punctuation(character)
            }))
}

fn is_attention_close(previous: Option<char>, next: Option<char>) -> bool {
    previous.is_some_and(|character| !character.is_whitespace())
        && (!previous.is_some_and(is_markdown_punctuation)
            || next.is_none_or(|character| {
                character.is_whitespace() || is_markdown_punctuation(character)
            }))
}

fn is_cjk_attention_open(previous: Option<char>, next: Option<char>) -> bool {
    previous.is_some_and(|character| character.script() == Script::Han)
        && next.is_some_and(is_cjk_opening_punctuation)
}

fn is_cjk_attention_close(previous: Option<char>, next: Option<char>) -> bool {
    previous.is_some_and(is_cjk_closing_punctuation)
        && next.is_some_and(is_unicode_letter_or_number)
}

fn is_markdown_punctuation(character: char) -> bool {
    character.is_ascii_punctuation()
        || character.general_category_group() == GeneralCategoryGroup::Punctuation
}

fn is_unicode_letter_or_number(character: char) -> bool {
    matches!(
        character.general_category_group(),
        GeneralCategoryGroup::Letter | GeneralCategoryGroup::Number
    )
}

fn is_cjk_opening_punctuation(character: char) -> bool {
    matches!(
        character,
        '《' | '「'
            | '『'
            | '【'
            | '〔'
            | '〖'
            | '〘'
            | '〚'
            | '〈'
            | '（'
            | '［'
            | '｛'
            | '“'
            | '‘'
            | '﹁'
            | '﹃'
            | '﹙'
            | '﹛'
            | '﹝'
    )
}

fn is_cjk_closing_punctuation(character: char) -> bool {
    matches!(
        character,
        '》' | '」'
            | '』'
            | '】'
            | '〕'
            | '〗'
            | '〙'
            | '〛'
            | '〉'
            | '）'
            | '］'
            | '｝'
            | '”'
            | '’'
            | '﹂'
            | '﹄'
            | '﹚'
            | '﹜'
            | '﹞'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ParentElement;

    use crate::text::{MarkdownExtensions, MarkdownNode, MarkdownPlugin};

    #[test]
    fn test_nested_emphasis_merges_text_marks() {
        let mut cx = NodeContext::default();
        let document = parse("This has **_bold and italic_** text.", &mut cx).unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        let bold_italic = paragraph
            .children
            .iter()
            .find(|child| child.text.as_ref() == "bold and italic")
            .expect("expected emphasized text");

        assert!(
            bold_italic
                .marks
                .iter()
                .any(|(_, mark)| mark.bold && mark.italic),
            "nested emphasis should produce a bold and italic mark"
        );
    }

    #[test]
    fn cjk_emphasis_compatibility_is_narrow_and_opt_in() {
        fn parse_with_compatibility(source: &str) -> ParsedDocument {
            let mut cx = NodeContext {
                markdown_extensions: MarkdownExtensions::default()
                    .cjk_emphasis_compatibility()
                    .into(),
                ..NodeContext::default()
            };
            parse(source, &mut cx).unwrap()
        }

        fn paragraph(document: &ParsedDocument) -> &Paragraph {
            let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
                panic!("expected paragraph");
            };
            paragraph
        }

        fn has_mark(paragraph: &Paragraph, predicate: impl Fn(&TextMark) -> bool) -> bool {
            paragraph
                .children
                .iter()
                .flat_map(|child| child.marks.iter().map(|(_, mark)| mark))
                .any(predicate)
        }

        let strong_source = "一次**“超现实主义”的渲染挑战**后";
        let mut strict_cx = NodeContext::default();
        let strict = parse(strong_source, &mut strict_cx).unwrap();
        assert_eq!(strict.source.as_ref(), strong_source);
        assert_eq!(paragraph(&strict).text(), strong_source);
        assert!(!has_mark(paragraph(&strict), |mark| mark.bold));

        let strong = parse_with_compatibility(strong_source);
        assert_eq!(strong.source.as_ref(), strong_source);
        assert_eq!(paragraph(&strong).text(), "一次“超现实主义”的渲染挑战后");
        assert!(has_mark(paragraph(&strong), |mark| mark.bold));

        let emphasis = parse_with_compatibility("了*《法》*后");
        assert_eq!(paragraph(&emphasis).text(), "了《法》后");
        assert!(has_mark(paragraph(&emphasis), |mark| mark.italic));

        let linked = parse_with_compatibility("一次**“点击[链接](https://example.com)”**后");
        let linked_paragraph = paragraph(&linked);
        assert_eq!(linked_paragraph.text(), "一次“点击链接”后");
        let linked_text = linked_paragraph
            .children
            .iter()
            .find(|child| child.text.contains("链接"))
            .expect("expected linked strong text");
        let link_start = linked_text.text.find("链接").expect("linked text offset");
        let link_range = link_start..link_start + "链接".len();
        assert!(linked_text.marks.iter().any(|(range, mark)| {
            mark.bold && range.start <= link_range.start && range.end >= link_range.end
        }));
        assert!(linked_text.marks.iter().any(|(range, mark)| {
            range.start <= link_range.start
                && range.end >= link_range.end
                && mark
                    .link
                    .as_ref()
                    .is_some_and(|link| link.url.as_ref() == "https://example.com")
        }));

        let decoded = parse_with_compatibility("一次**“内容 &amp; 内容”**后");
        assert_eq!(paragraph(&decoded).text(), "一次“内容 & 内容”后");
        assert!(has_mark(paragraph(&decoded), |mark| mark.bold));

        let list = parse_with_compatibility("- **H01M（电池）**1,560件");
        let BlockNode::List { children, .. } = &list.blocks[0] else {
            panic!("expected list");
        };
        let BlockNode::ListItem { children, .. } = &children[0] else {
            panic!("expected list item");
        };
        let BlockNode::Paragraph(list_paragraph) = &children[0] else {
            panic!("expected list paragraph");
        };
        assert_eq!(list_paragraph.text(), "H01M（电池）1,560件");
        assert!(has_mark(list_paragraph, |mark| mark.bold));

        for source in [
            "foo**a,b**bar",
            "a**+x）**1",
            "3 * 4 * 5",
            r"\*\*",
            "`一次**“内容”**后`",
        ] {
            let mut strict_cx = NodeContext::default();
            let strict = parse(source, &mut strict_cx).unwrap();
            let compatible = parse_with_compatibility(source);
            assert_eq!(
                compatible, strict,
                "compatibility changed native parsing for {source:?}"
            );
        }
    }

    #[test]
    fn prepared_image_keeps_authoritative_alt_text() {
        let extensions =
            MarkdownExtensions::default().prepare_source(|source| source.replace("$5", "^5"));
        let mut cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let source = "![Cost $5](https://example.com/image.svg \"Preview\")";

        let document = parse(source, &mut cx).unwrap();
        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        let image = paragraph.children[0]
            .image
            .as_ref()
            .expect("prepared image");
        assert_eq!(image.alt.as_deref(), Some("Cost $5"));
        assert_eq!(image.url.to_string(), "https://example.com/image.svg");
        assert_eq!(image.title.as_deref(), Some("Preview"));
        assert_eq!(document.source.as_ref(), source);
    }

    #[test]
    fn prepared_reference_images_keep_authoritative_alt_text_and_destinations() {
        let extensions =
            MarkdownExtensions::default().prepare_source(|source| source.replace("$$", "^^"));
        let mut cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        // Definitions precede their references here; resolving definitions
        // that appear later in the document is the retained-definition
        // pre-pass, restored separately.
        let source = "[r]: https://a.test/i.svg \"A\"\n[s]: https://b.test/i.svg \"B\"\n[c $$z]: https://c.test/i.svg \"C\"\n[d w$$]: https://d.test/i.svg \"D\"\n\n![a $$x][r] ![b y$$][s] ![c $$z][] ![d w$$]";

        let document = parse(source, &mut cx).unwrap();
        let BlockNode::Paragraph(paragraph) = document
            .blocks
            .iter()
            .find(|block| matches!(block, BlockNode::Paragraph(_)))
            .expect("expected paragraph")
        else {
            unreachable!();
        };
        let images = paragraph
            .children
            .iter()
            .filter_map(|child| child.image.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(images.len(), 4);
        assert_eq!(images[0].alt.as_deref(), Some("a $$x"));
        assert_eq!(images[0].url.to_string(), "https://a.test/i.svg");
        assert_eq!(images[0].title.as_deref(), Some("A"));
        assert_eq!(images[1].alt.as_deref(), Some("b y$$"));
        assert_eq!(images[1].url.to_string(), "https://b.test/i.svg");
        assert_eq!(images[1].title.as_deref(), Some("B"));
        assert_eq!(images[2].alt.as_deref(), Some("c $$z"));
        assert_eq!(images[2].url.to_string(), "https://c.test/i.svg");
        assert_eq!(images[2].title.as_deref(), Some("C"));
        assert_eq!(images[3].alt.as_deref(), Some("d w$$"));
        assert_eq!(images[3].url.to_string(), "https://d.test/i.svg");
        assert_eq!(images[3].title.as_deref(), Some("D"));
        assert_eq!(document.source.as_ref(), source);
    }

    #[test]
    fn test_inline_html_image_stays_in_markdown_paragraph() {
        let mut cx = NodeContext::default();
        let document = parse(
            r#"Before <img src="https://example.com/avatar.png" alt="Avatar" width="32" height="32" /> after."#,
            &mut cx,
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        assert_eq!(paragraph.children.len(), 3);
        assert_eq!(paragraph.children[0].text.as_ref(), "Before ");
        assert_eq!(paragraph.children[2].text.as_ref(), " after.");

        let image = paragraph.children[1]
            .image
            .as_ref()
            .expect("expected inline html image");
        assert_eq!(image.url.as_ref(), "https://example.com/avatar.png");
        assert_eq!(image.width, Some(gpui::px(32.).into()));
        assert_eq!(image.height, Some(gpui::px(32.).into()));
    }

    #[test]
    fn test_inline_html_image_without_size_stays_in_markdown_paragraph() {
        let mut cx = NodeContext::default();
        let document = parse(
            r#"Before <img src="https://avatars.githubusercontent.com/u/5518"> after."#,
            &mut cx,
        )
        .unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };

        assert_eq!(paragraph.children.len(), 3);
        assert_eq!(paragraph.children[0].text.as_ref(), "Before ");
        assert_eq!(paragraph.children[2].text.as_ref(), " after.");

        let image = paragraph.children[1]
            .image
            .as_ref()
            .expect("expected inline html image");
        assert_eq!(
            image.url.as_ref(),
            "https://avatars.githubusercontent.com/u/5518"
        );
        assert_eq!(image.width, None);
        assert_eq!(image.height, None);
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Ticker {
        symbol: String,
    }

    fn parse_ticker_block(node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Paragraph(paragraph) = node else {
            return None;
        };
        let [Node::Text(text)] = paragraph.children.as_slice() else {
            return None;
        };
        let symbol = text.value.strip_prefix('$')?.to_string();
        let node_text = format!("${symbol}");

        Some(
            MarkdownNode::new("ticker", Ticker { symbol })
                .text(node_text)
                .markdown(cx.node_source(node).unwrap_or_default()),
        )
    }

    #[test]
    fn custom_block_parser_converts_ticker_syntax_to_custom_node() {
        let extensions = MarkdownExtensions::default().block_parser(parse_ticker_block);

        let mut cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let document = parse("$TSLA.US", &mut cx).unwrap();

        let BlockNode::Custom(node) = &document.blocks[0] else {
            panic!("expected custom markdown node");
        };
        assert_eq!(node.name(), "ticker");
        assert_eq!(node.as_text(), "$TSLA.US");
        assert_eq!(node.as_markdown(), "$TSLA.US");
        assert_eq!(
            node.data::<Ticker>(),
            Some(&Ticker {
                symbol: "TSLA.US".to_string()
            })
        );
        assert_eq!(document.text(), "$TSLA.US\n");
        assert_eq!(document.to_markdown(), "$TSLA.US");
    }

    struct TickerPlugin {
        name: &'static str,
    }

    impl TickerPlugin {
        fn new(name: &'static str) -> Self {
            Self { name }
        }
    }

    impl MarkdownPlugin for TickerPlugin {
        fn is_block(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            self.name
        }

        fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
            parse_ticker_block(node, cx)
        }

        fn render(
            &self,
            node: &MarkdownNode,
            _window: &mut gpui::Window,
            _cx: &mut gpui::App,
        ) -> impl gpui::IntoElement {
            gpui::div().child(node.as_text().to_string())
        }
    }

    #[test]
    fn custom_block_plugin_registers_parser_and_renderer() {
        let extensions = MarkdownExtensions::default().plugin(TickerPlugin::new("ticker"));

        let mut cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let document = parse("$TSLA.US", &mut cx).unwrap();

        let BlockNode::Custom(node) = &document.blocks[0] else {
            panic!("expected custom markdown node");
        };
        assert_eq!(node.name(), "ticker");
        assert_eq!(
            node.data::<Ticker>(),
            Some(&Ticker {
                symbol: "TSLA.US".to_string()
            })
        );
    }
}
