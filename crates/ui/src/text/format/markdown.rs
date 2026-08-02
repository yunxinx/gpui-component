use std::ops::Range;

use gpui::SharedString;
use markdown::mdast::{self, Node};

use crate::text::{
    document::ParsedDocument,
    markdown_ext::MarkdownParseContext,
    node::{
        self, BlockNode, CodeBlock, ImageNode, InlineNode, LinkMark, NodeContext, Paragraph, Span,
        Table, TableRow, TextMark,
    },
};

const DEFINITION_BOUNDARY: &str = "gpui-component-retained-definition-boundary";

/// Parse Markdown into a tree of nodes.
pub(crate) fn parse(source: &str, cx: &mut NodeContext) -> Result<ParsedDocument, SharedString> {
    parse_with_retained_definitions(source, &[], &[], cx)
}

/// Parse an incremental fragment with reference identifiers retained from
/// earlier blocks in the same document.
#[cfg(test)]
pub(crate) fn parse_with_reference_identifiers(
    source: &str,
    reference_source_identifiers: &[SharedString],
    cx: &mut NodeContext,
) -> Result<ParsedDocument, SharedString> {
    parse_with_retained_definitions(source, reference_source_identifiers, &[], cx)
}

/// Parse an incremental fragment with document-wide definitions retained from
/// earlier blocks in the same document.
pub(crate) fn parse_with_retained_definitions(
    source: &str,
    reference_source_identifiers: &[SharedString],
    footnote_source_identifiers: &[SharedString],
    cx: &mut NodeContext,
) -> Result<ParsedDocument, SharedString> {
    let prepared_source = cx.markdown_extensions.prepared_source(source)?;
    let options = cx.markdown_extensions.configured_parse_options();
    let Some((parse_source, prefix_len, prefix_line_count)) = definition_prefix(
        &prepared_source,
        reference_source_identifiers,
        footnote_source_identifiers,
    ) else {
        return markdown::to_mdast(&prepared_source, &options)
            .map(|root| ast_to_document(source, &prepared_source, root, &options, cx))
            .map_err(|error| error.to_string().into());
    };

    let mut root = match markdown::to_mdast(&parse_source, &options) {
        Ok(root) => root,
        Err(prefix_error) => {
            return match markdown::to_mdast(&prepared_source, &options) {
                Err(source_error) => Err(source_error.to_string().into()),
                Ok(_) => Err(format!(
                    "failed to parse Markdown with retained definitions: {}",
                    prefix_error.reason
                )
                .into()),
            };
        }
    };
    remove_definition_prefix(
        &mut root,
        reference_source_identifiers.len(),
        footnote_source_identifiers.len(),
        prefix_len,
        prefix_line_count,
    )?;
    Ok(ast_to_document(
        source,
        &prepared_source,
        root,
        &options,
        cx,
    ))
}

/// Prefix an incremental fragment with inert definitions so markdown-rs knows
/// which retained identifiers are valid while parsing the fragment.
///
/// Definitions are placed before the fragment because an unterminated fenced
/// code, HTML, or math block at EOF could consume a suffix. These are the
/// original, reparseable source identifiers captured from definition nodes,
/// before markdown-rs performs potentially length-expanding Unicode case
/// normalization.
fn definition_prefix(
    prepared_source: &str,
    reference_source_identifiers: &[SharedString],
    footnote_source_identifiers: &[SharedString],
) -> Option<(String, usize, usize)> {
    if reference_source_identifiers.is_empty() && footnote_source_identifiers.is_empty() {
        return None;
    }

    let mut parse_source = String::new();
    for identifier in reference_source_identifiers {
        parse_source.push('[');
        parse_source.push_str(identifier);
        parse_source.push_str("]: /\n");
    }
    for identifier in footnote_source_identifiers {
        parse_source.push_str("[^");
        parse_source.push_str(identifier);
        parse_source.push_str("]: /\n");
    }
    parse_source.push('\n');
    parse_source.push_str(DEFINITION_BOUNDARY);
    parse_source.push_str("\n\n");

    let prefix_len = parse_source.len();
    let prefix_line_count = markdown_line_ending_count(&parse_source);
    parse_source.push_str(prepared_source);
    Some((parse_source, prefix_len, prefix_line_count))
}

fn markdown_line_ending_count(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut count = 0;
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\r' => {
                count += 1;
                offset += usize::from(bytes.get(offset + 1) == Some(&b'\n')) + 1;
            }
            b'\n' => {
                count += 1;
                offset += 1;
            }
            _ => offset += 1,
        }
    }
    count
}

/// Remove the synthetic definitions and translate all remaining mdast source
/// positions back into the fragment's coordinate space.
fn remove_definition_prefix(
    root: &mut Node,
    reference_definition_count: usize,
    footnote_definition_count: usize,
    prefix_len: usize,
    prefix_line_count: usize,
) -> Result<(), SharedString> {
    let Node::Root(root) = root else {
        return Err("markdown parser returned a non-root node".into());
    };
    let definition_count = reference_definition_count + footnote_definition_count;
    if root.children.len() <= definition_count
        || !root.children[..reference_definition_count]
            .iter()
            .all(|node| matches!(node, Node::Definition(_)))
        || !root.children[reference_definition_count..definition_count]
            .iter()
            .all(|node| matches!(node, Node::FootnoteDefinition(_)))
        || !matches!(
            &root.children[definition_count],
            Node::Paragraph(paragraph)
                if matches!(
                    paragraph.children.as_slice(),
                    [Node::Text(text)] if text.value == DEFINITION_BOUNDARY
                )
        )
    {
        return Err("failed to reconstruct retained Markdown definitions".into());
    }

    root.children.drain(..=definition_count);
    for node in &mut root.children {
        rebase_position(node, prefix_len, prefix_line_count)?;
    }
    Ok(())
}

fn rebase_position(
    node: &mut Node,
    prefix_len: usize,
    prefix_line_count: usize,
) -> Result<(), SharedString> {
    match node {
        Node::MdxjsEsm(node) => rebase_stops(&mut node.stops, prefix_len)?,
        Node::MdxFlowExpression(node) => rebase_stops(&mut node.stops, prefix_len)?,
        Node::MdxTextExpression(node) => rebase_stops(&mut node.stops, prefix_len)?,
        Node::MdxJsxFlowElement(node) => {
            rebase_attribute_stops(&mut node.attributes, prefix_len)?;
        }
        Node::MdxJsxTextElement(node) => {
            rebase_attribute_stops(&mut node.attributes, prefix_len)?;
        }
        _ => {}
    }
    if let Some(position) = node.position_mut() {
        if position.start.offset < prefix_len
            || position.end.offset < prefix_len
            || position.start.line <= prefix_line_count
            || position.end.line <= prefix_line_count
        {
            return Err("Markdown node crossed the retained-definition prefix boundary".into());
        }

        position.start.offset -= prefix_len;
        position.end.offset -= prefix_len;
        position.start.line -= prefix_line_count;
        position.end.line -= prefix_line_count;
    }
    if let Some(children) = node.children_mut() {
        for child in children {
            rebase_position(child, prefix_len, prefix_line_count)?;
        }
    }
    Ok(())
}

fn rebase_attribute_stops(
    attributes: &mut [mdast::AttributeContent],
    prefix_len: usize,
) -> Result<(), SharedString> {
    for attribute in attributes {
        match attribute {
            mdast::AttributeContent::Expression(expression) => {
                rebase_stops(&mut expression.stops, prefix_len)?;
            }
            mdast::AttributeContent::Property(property) => {
                if let Some(mdast::AttributeValue::Expression(expression)) = &mut property.value {
                    rebase_stops(&mut expression.stops, prefix_len)?;
                }
            }
        }
    }
    Ok(())
}

fn rebase_stops(stops: &mut [mdast::Stop], prefix_len: usize) -> Result<(), SharedString> {
    for (_, source_offset) in stops {
        if *source_offset < prefix_len {
            return Err("MDX stop crossed the retained-definition prefix boundary".into());
        }
        *source_offset -= prefix_len;
    }
    Ok(())
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
                paragraph.push(InlineNode::image(image));
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
            paragraph.push_image(ImageNode {
                url: raw.url.clone().into(),
                title: raw.title.clone().map(|t| t.into()),
                alt: Some(raw.alt.clone().into()),
                ..Default::default()
            });
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
                if let Some(inline_text) = append_inline_html_blocks(paragraph, el.blocks) {
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

fn ast_to_document(
    source: &str,
    prepared_source: &str,
    root: mdast::Node,
    parse_options: &markdown::ParseOptions,
    cx: &mut NodeContext,
) -> ParsedDocument {
    collect_definitions(prepared_source, &root, parse_options, cx);
    let root = match root {
        Node::Root(r) => r,
        _ => panic!("expected root node"),
    };

    let blocks = root
        .children
        .into_iter()
        .map(|c| ast_to_node(source, prepared_source, c, cx))
        .collect();
    ParsedDocument {
        source: source.to_string().into(),
        blocks,
    }
}

/// Collect document-wide parse metadata before presentation plugins can
/// replace a definition or one of its ancestor blocks.
fn collect_definitions(
    prepared_source: &str,
    node: &Node,
    options: &markdown::ParseOptions,
    cx: &mut NodeContext,
) {
    match node {
        Node::Definition(definition) => {
            let identifier: SharedString = definition.identifier.clone().into();
            cx.add_link_definition(
                identifier.clone(),
                reparseable_definition_source_identifier(prepared_source, definition, options),
                LinkMark {
                    url: definition.url.clone().into(),
                    identifier: Some(identifier),
                    title: definition.title.clone().map(Into::into),
                },
                definition
                    .position
                    .as_ref()
                    .map(|position| cx.offset + position.start.offset),
            );
        }
        Node::FootnoteDefinition(definition) => {
            cx.add_footnote_definition(
                definition.identifier.clone().into(),
                reparseable_footnote_definition_source_identifier(
                    prepared_source,
                    definition,
                    options,
                ),
                definition
                    .position
                    .as_ref()
                    .map(|position| cx.offset + position.start.offset),
            );
        }
        _ => {}
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_definitions(prepared_source, child, options, cx);
        }
    }
}

/// Find a label spelling that can be replayed outside its original container
/// and still produces the same normalized mdast identifier.
///
/// The exact source spelling is preferred because Unicode case folding can
/// expand a valid label beyond markdown-rs's label-size limit. Container
/// continuation markers may occur inside the definition position, however, so
/// every candidate is parsed and checked before it is retained.
fn reparseable_definition_source_identifier(
    prepared_source: &str,
    definition: &mdast::Definition,
    options: &markdown::ParseOptions,
) -> Option<SharedString> {
    reparseable_source_identifier(
        extract_positioned_definition_label(prepared_source, definition.position.as_ref(), "["),
        &definition.identifier,
        DefinitionKind::Link,
        options,
    )
}

fn reparseable_footnote_definition_source_identifier(
    prepared_source: &str,
    definition: &mdast::FootnoteDefinition,
    options: &markdown::ParseOptions,
) -> Option<SharedString> {
    reparseable_source_identifier(
        extract_positioned_definition_label(prepared_source, definition.position.as_ref(), "[^"),
        &definition.identifier,
        DefinitionKind::Footnote,
        options,
    )
}

fn reparseable_source_identifier(
    source_identifier: Option<&str>,
    normalized_identifier: &str,
    kind: DefinitionKind,
    options: &markdown::ParseOptions,
) -> Option<SharedString> {
    for candidate in [source_identifier, Some(normalized_identifier)]
        .into_iter()
        .flatten()
    {
        if standalone_definition_identifier_matches(candidate, normalized_identifier, kind, options)
        {
            return Some(candidate.to_string().into());
        }
    }
    None
}

fn extract_positioned_definition_label<'a>(
    prepared_source: &'a str,
    position: Option<&markdown::unist::Position>,
    prefix: &str,
) -> Option<&'a str> {
    let position = position?;
    let source = prepared_source.get(position.start.offset..position.end.offset)?;
    extract_definition_label(source, prefix)
}

fn extract_definition_label<'a>(source: &'a str, prefix: &str) -> Option<&'a str> {
    let label_start = source.find(prefix)? + prefix.len();
    let mut bracket_depth = 0usize;
    let mut escaped = false;

    for (relative_offset, character) in source[label_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' => bracket_depth += 1,
            ']' if bracket_depth > 0 => bracket_depth -= 1,
            ']' => {
                let label_end = label_start + relative_offset;
                if source[label_end + 1..].starts_with(':') {
                    return source.get(label_start..label_end);
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy)]
enum DefinitionKind {
    Link,
    Footnote,
}

fn standalone_definition_identifier_matches(
    source_identifier: &str,
    expected_identifier: &str,
    kind: DefinitionKind,
    options: &markdown::ParseOptions,
) -> bool {
    let source = match kind {
        DefinitionKind::Link => format!("[{source_identifier}]: /"),
        DefinitionKind::Footnote => format!("[^{source_identifier}]: /"),
    };
    let Ok(Node::Root(root)) = markdown::to_mdast(&source, options) else {
        return false;
    };
    match (kind, root.children.as_slice()) {
        (DefinitionKind::Link, [Node::Definition(definition)]) => {
            definition.identifier == expected_identifier
        }
        (DefinitionKind::Footnote, [Node::FootnoteDefinition(definition)]) => {
            definition.identifier == expected_identifier
        }
        _ => false,
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
    source: &str,
    prepared_source: &str,
    value: mdast::Node,
    cx: &mut NodeContext,
) -> BlockNode {
    let span = new_span(value.position().cloned(), cx);
    let parse_cx = MarkdownParseContext::new(source, prepared_source, cx.offset);
    if let Some(mut node) = cx.markdown_extensions.parse_block(&value, &parse_cx) {
        node.set_span(span);
        return BlockNode::Custom(node);
    }

    match value {
        Node::Root(_) => unreachable!("node::Root should be handled separately"),
        Node::Paragraph(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, &parse_cx, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::Blockquote(val) => {
            let children = val
                .children
                .into_iter()
                .map(|c| ast_to_node(source, prepared_source, c, cx))
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
                .map(|c| ast_to_node(source, prepared_source, c, cx))
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
                .map(|c| ast_to_node(source, prepared_source, c, cx))
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
                parse_paragraph(&mut paragraph, c, &parse_cx, cx);
            });

            BlockNode::Heading {
                level: val.depth,
                children: paragraph,
                span: new_span(val.position, cx),
            }
        }
        Node::Math(val) => {
            let text = val
                .position
                .as_ref()
                .and_then(|position| source.get(position.start.offset..position.end.offset))
                .map(str::to_string)
                .unwrap_or(val.value);
            let mut paragraph = Paragraph::new(text);
            if let Some(span) = new_span(val.position, cx) {
                paragraph.set_span(span);
            }
            BlockNode::Paragraph(paragraph)
        }
        Node::Html(val) => match super::html::parse(&val.value, cx) {
            Ok(el) => BlockNode::Root {
                children: el.blocks,
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
                parse_paragraph(&mut paragraph, c, &parse_cx, cx);
            });
            paragraph.span = new_span(val.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::MdxJsxFlowElement(val) => {
            let mut paragraph = Paragraph::default();
            val.children.iter().for_each(|c| {
                parse_paragraph(&mut paragraph, c, &parse_cx, cx);
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
                    parse_table_row(&mut table, row, &parse_cx, cx);
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
                parse_paragraph(&mut paragraph, c, &parse_cx, cx);
            });
            paragraph.span = new_span(def.position, cx);
            BlockNode::Paragraph(paragraph)
        }
        Node::Definition(def) => BlockNode::Definition {
            identifier: def.identifier.clone().into(),
            url: def.url.clone().into(),
            title: def.title.clone().map(|s| s.into()),
            span: new_span(def.position, cx),
        },
        _ => {
            if cfg!(debug_assertions) {
                tracing::warn!("unsupported node: {:#?}", value);
            }
            BlockNode::Unknown
        }
    }
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

    fn math_inline_extensions() -> MarkdownExtensions {
        MarkdownExtensions::default()
            .parse_options(|options| {
                options.constructs.math_text = true;
                options.constructs.math_flow = true;
            })
            .prepare_source(|source| {
                source
                    .replace(r"\(", "$$")
                    .replace(r"\)", "$$")
                    .replace(r"\[", "$$")
                    .replace(r"\]", "$$")
            })
            .inline_parser(|node, cx| {
                let Node::InlineMath(math) = node else {
                    return None;
                };
                let source = cx.node_source(node)?.to_string();
                Some(
                    MarkdownNode::new("math-inline", math.value.clone())
                        .text(source.clone())
                        .markdown(source),
                )
            })
    }

    #[test]
    fn source_preparation_must_preserve_utf8_byte_offsets() {
        let mut cx = NodeContext {
            markdown_extensions: MarkdownExtensions::default()
                .prepare_source(|source| format!("{source}!"))
                .into(),
            ..NodeContext::default()
        };

        let error = parse("original", &mut cx).expect_err("non-length-preserving source");
        assert!(
            error.contains("same UTF-8 byte length"),
            "unexpected preparation error: {error}"
        );
    }

    #[test]
    fn source_preparation_must_preserve_utf8_character_boundaries() {
        let mut cx = NodeContext {
            markdown_extensions: MarkdownExtensions::default()
                .prepare_source(|_| "aa".to_string())
                .into(),
            ..NodeContext::default()
        };

        let error = parse("é", &mut cx).expect_err("shifted UTF-8 boundaries");
        assert!(
            error.contains("same UTF-8 character boundaries"),
            "unexpected preparation error: {error}"
        );
    }

    #[test]
    fn reference_definitions_retain_reparseable_source_identifiers() {
        for source_identifier in [r"a\]b", r"a\[b\]c", "a&#93;b", "a\nb"] {
            let mut cx = NodeContext::default();
            parse(&format!("[{source_identifier}]: /"), &mut cx)
                .expect("reference definition should parse");

            assert_eq!(
                cx.link_ref_source_identifiers.len(),
                1,
                "definition did not parse for {source_identifier:?}"
            );
            assert_eq!(
                cx.link_ref_source_identifiers
                    .values()
                    .next()
                    .map(|identifier| identifier.as_ref()),
                Some(source_identifier)
            );
        }
    }

    #[test]
    fn inline_parser_preserves_original_source_and_native_ancestor_marks() {
        let source = "before **[$x$](https://example.com)** after";
        let mut cx = NodeContext {
            markdown_extensions: math_inline_extensions().into(),
            ..NodeContext::default()
        };
        let document = parse(source, &mut cx).unwrap();

        assert_eq!(document.source.as_ref(), source);
        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text(), "before $x$ after");

        let custom = paragraph
            .children
            .iter()
            .find(|child| child.custom.is_some())
            .expect("expected custom inline node");
        assert_eq!(custom.text.as_ref(), "$x$");
        assert!(custom.marks.iter().any(|(_, mark)| {
            mark.bold
                && mark
                    .link
                    .as_ref()
                    .is_some_and(|link| link.url.as_ref() == "https://example.com")
        }));
    }

    #[test]
    fn prepared_delimiters_map_back_to_original_inline_source() {
        let source = r"before \(x + y\) after";
        let mut cx = NodeContext {
            markdown_extensions: math_inline_extensions().into(),
            ..NodeContext::default()
        };
        let document = parse(source, &mut cx).unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        let custom = paragraph
            .children
            .iter()
            .find_map(|child| child.custom.as_ref())
            .expect("expected prepared custom inline node");
        assert_eq!(custom.as_text(), r"\(x + y\)");
        assert_eq!(custom.as_markdown(), r"\(x + y\)");
        assert_eq!(document.source.as_ref(), source);
    }

    #[test]
    fn inline_parse_context_exposes_both_source_views_and_absolute_range() {
        let extensions = MarkdownExtensions::default()
            .parse_options(|options| options.constructs.math_text = true)
            .prepare_source(|source| source.replace(r"\(", "$$").replace(r"\)", "$$"))
            .inline_parser(|node, cx| {
                let Node::InlineMath(_) = node else {
                    return None;
                };
                Some(MarkdownNode::new(
                    "source-views",
                    (
                        cx.node_source(node)?.to_string(),
                        cx.prepared_node_source(node)?.to_string(),
                        cx.node_range(node)?,
                    ),
                ))
            });
        let mut cx = NodeContext {
            offset: 7,
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };

        let document = parse(r"\(x\)", &mut cx).unwrap();
        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        let custom = paragraph.children[0].custom.as_ref().unwrap();
        assert_eq!(
            custom.data::<(String, String, Range<usize>)>(),
            Some(&(r"\(x\)".to_string(), "$$x$$".to_string(), 7..12))
        );
        assert_eq!(custom.as_text(), r"\(x\)");
        assert_eq!(custom.as_markdown(), r"\(x\)");
        assert_eq!(custom.span, Some(Span { start: 7, end: 12 }));
    }

    #[test]
    fn retained_references_preserve_inline_source_views_and_ranges() {
        let extensions = MarkdownExtensions::default()
            .parse_options(|options| options.constructs.math_text = true)
            .prepare_source(|source| source.replace(r"\(", "$$").replace(r"\)", "$$"))
            .inline_parser(|node, cx| {
                let Node::InlineMath(_) = node else {
                    return None;
                };
                Some(MarkdownNode::new(
                    "retained-reference-source-views",
                    (
                        cx.source().to_string(),
                        cx.prepared_source().to_string(),
                        cx.node_source(node)?.to_string(),
                        cx.prepared_node_source(node)?.to_string(),
                        cx.node_range(node)?,
                    ),
                ))
            });
        let mut cx = NodeContext {
            offset: 11,
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };

        let source = r"\(x\)";
        let document = parse_with_reference_identifiers(source, &["ref".into()], &mut cx).unwrap();
        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        let custom = paragraph.children[0].custom.as_ref().unwrap();
        assert_eq!(
            custom.data::<(String, String, String, String, Range<usize>)>(),
            Some(&(
                source.to_string(),
                "$$x$$".to_string(),
                source.to_string(),
                "$$x$$".to_string(),
                11..16,
            ))
        );
        assert_eq!(custom.span, Some(Span { start: 11, end: 16 }));
        assert_eq!(document.source.as_ref(), source);
    }

    #[test]
    fn reference_prefix_counts_all_markdown_line_endings() {
        assert_eq!(markdown_line_ending_count("a\nb\rc\r\nd"), 3);
    }

    fn collect_mdx_source_offsets(node: &Node, offsets: &mut Vec<usize>) {
        let mut collect_stops = |stops: &[mdast::Stop]| {
            offsets.extend(stops.iter().map(|(_, source_offset)| *source_offset));
        };
        match node {
            Node::MdxjsEsm(node) => collect_stops(&node.stops),
            Node::MdxFlowExpression(node) => collect_stops(&node.stops),
            Node::MdxTextExpression(node) => collect_stops(&node.stops),
            Node::MdxJsxFlowElement(node) => {
                collect_attribute_source_offsets(&node.attributes, offsets);
            }
            Node::MdxJsxTextElement(node) => {
                collect_attribute_source_offsets(&node.attributes, offsets);
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            for child in children {
                collect_mdx_source_offsets(child, offsets);
            }
        }
    }

    fn collect_attribute_source_offsets(
        attributes: &[mdast::AttributeContent],
        offsets: &mut Vec<usize>,
    ) {
        for attribute in attributes {
            let stops = match attribute {
                mdast::AttributeContent::Expression(expression) => &expression.stops,
                mdast::AttributeContent::Property(property) => {
                    let Some(mdast::AttributeValue::Expression(expression)) = &property.value
                    else {
                        continue;
                    };
                    &expression.stops
                }
            };
            offsets.extend(stops.iter().map(|(_, source_offset)| *source_offset));
        }
    }

    fn mdx_source_offset_extensions() -> MarkdownExtensions {
        MarkdownExtensions::default()
            .mdx()
            .parse_options(|options| {
                options.constructs.mdx_esm = true;
                options.mdx_expression_parse = Some(Box::new(|_, _| markdown::MdxSignal::Ok));
                options.mdx_esm_parse = Some(Box::new(|_| markdown::MdxSignal::Ok));
            })
            .block_parser(|node, cx| {
                let mut offsets = Vec::new();
                collect_mdx_source_offsets(node, &mut offsets);
                if offsets.is_empty() {
                    return None;
                }
                Some(
                    MarkdownNode::new("mdx-source-offsets", offsets)
                        .text(cx.node_source(node)?.to_string()),
                )
            })
    }

    fn parse_mdx_source_offsets(
        source: &str,
        reference_identifiers: &[SharedString],
    ) -> Vec<usize> {
        let mut cx = NodeContext {
            markdown_extensions: mdx_source_offset_extensions().into(),
            ..NodeContext::default()
        };
        let document = parse_with_reference_identifiers(source, reference_identifiers, &mut cx)
            .expect("MDX source should parse");
        let BlockNode::Custom(custom) = &document.blocks[0] else {
            panic!("expected observed MDX block for {source:?}");
        };
        custom
            .data::<Vec<usize>>()
            .expect("expected captured MDX source offsets")
            .clone()
    }

    #[test]
    fn retained_references_preserve_mdx_stop_offsets() {
        for source in [
            "before {alpha} <Widget value={beta} {...gamma}>child {delta}</Widget> after",
            "{flow}",
            "export const value = 1",
        ] {
            let expected = parse_mdx_source_offsets(source, &[]);
            assert!(!expected.is_empty(), "expected MDX stops for {source:?}");

            let actual = parse_mdx_source_offsets(source, &["ref".into()]);
            assert_eq!(actual, expected, "MDX stops changed for {source:?}");
            assert!(
                actual.iter().all(|offset| *offset <= source.len()),
                "MDX stop escaped source for {source:?}: {actual:?}"
            );
        }
    }

    #[test]
    fn retained_references_report_mdx_errors_in_fragment_coordinates() {
        let extensions = MarkdownExtensions::default().mdx();
        let source = "{";
        let mut baseline_cx = NodeContext {
            markdown_extensions: extensions.clone().into(),
            ..NodeContext::default()
        };
        let expected = parse(source, &mut baseline_cx).expect_err("MDX should be incomplete");

        let mut retained_cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let actual = parse_with_reference_identifiers(source, &["ref".into()], &mut retained_cx)
            .expect_err("MDX should remain incomplete");

        assert_eq!(actual, expected);
        assert!(actual.starts_with("1:2:"), "unexpected MDX error: {actual}");
    }

    #[test]
    fn retained_references_do_not_leak_into_unclosed_fenced_code() {
        let source = "```text\n[value][ref]";
        let mut cx = NodeContext::default();

        let document = parse_with_reference_identifiers(source, &["ref".into()], &mut cx).unwrap();
        let BlockNode::CodeBlock(code) = &document.blocks[0] else {
            panic!("expected fenced code block");
        };
        assert_eq!(code.code().as_ref(), "[value][ref]");
        assert_eq!(
            code.span,
            Some(Span {
                start: 0,
                end: source.len()
            })
        );
        assert_eq!(document.source.as_ref(), source);
    }

    #[derive(Debug, PartialEq)]
    struct ObservedBlockSource {
        source: String,
        prepared_source: String,
        node_source: String,
        range: Range<usize>,
    }

    fn observe_unclosed_block_extensions() -> MarkdownExtensions {
        MarkdownExtensions::default()
            .parse_options(|options| options.constructs.math_flow = true)
            .block_parser(|node, cx| {
                let name = match node {
                    Node::Html(_) => "observed-html",
                    Node::Math(_) => "observed-math",
                    _ => return None,
                };
                let node_source = cx.node_source(node)?.to_string();
                Some(
                    MarkdownNode::new(
                        name,
                        ObservedBlockSource {
                            source: cx.source().to_string(),
                            prepared_source: cx.prepared_source().to_string(),
                            node_source: node_source.clone(),
                            range: cx.node_range(node)?,
                        },
                    )
                    .text(node_source.clone())
                    .markdown(node_source),
                )
            })
    }

    #[test]
    fn retained_references_do_not_leak_into_unclosed_html_or_math() {
        for source in ["<div>\n[value][ref]", "$$\n[value][ref]"] {
            let mut cx = NodeContext {
                offset: 5,
                markdown_extensions: observe_unclosed_block_extensions().into(),
                ..NodeContext::default()
            };

            let document =
                parse_with_reference_identifiers(source, &["ref".into()], &mut cx).unwrap();
            let BlockNode::Custom(custom) = &document.blocks[0] else {
                panic!("expected observed custom block for {source:?}");
            };
            assert_eq!(
                custom.data::<ObservedBlockSource>(),
                Some(&ObservedBlockSource {
                    source: source.to_string(),
                    prepared_source: source.to_string(),
                    node_source: source.to_string(),
                    range: 5..5 + source.len(),
                })
            );
            assert_eq!(
                custom.span,
                Some(Span {
                    start: 5,
                    end: 5 + source.len()
                })
            );
            assert_eq!(document.source.as_ref(), source);
        }
    }

    #[test]
    fn retained_references_preserve_eof_hard_break_semantics() {
        for source in ["[value][ref]  ", "[value][ref]\\"] {
            let mut cx = NodeContext::default();
            let document =
                parse_with_reference_identifiers(source, &["ref".into()], &mut cx).unwrap();
            let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
                panic!("expected paragraph for {source:?}");
            };

            assert!(!paragraph.text().contains('\n'));
            assert_eq!(
                paragraph.span,
                Some(Span {
                    start: 0,
                    end: source.len()
                })
            );
            assert!(paragraph.children.iter().any(|child| {
                child.marks.iter().any(|(_, mark)| {
                    mark.link
                        .as_ref()
                        .and_then(|link| link.identifier.as_deref())
                        == Some("ref")
                })
            }));
            assert_eq!(document.source.as_ref(), source);
        }
    }

    #[test]
    fn inline_custom_nodes_keep_commonmark_hard_breaks() {
        let mut cx = NodeContext {
            markdown_extensions: math_inline_extensions().into(),
            ..NodeContext::default()
        };
        let document = parse("before $x$  \nafter", &mut cx).unwrap();

        let BlockNode::Paragraph(paragraph) = &document.blocks[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(paragraph.text(), "before $x$\nafter");
        assert!(
            paragraph
                .children
                .iter()
                .any(|child| child.custom.is_some())
        );
    }

    #[test]
    fn inline_custom_nodes_compose_with_heading_marks_code_and_images() {
        let mut cx = NodeContext {
            markdown_extensions: math_inline_extensions().into(),
            ..NodeContext::default()
        };
        let document = parse(
            "# *$a$* ~~$b$~~ `$not-math$` ![plot](https://example.com/plot.png)",
            &mut cx,
        )
        .unwrap();

        let BlockNode::Heading {
            level, children, ..
        } = &document.blocks[0]
        else {
            panic!("expected heading");
        };
        assert_eq!(*level, 1);

        let custom = children
            .children
            .iter()
            .filter(|child| child.custom.is_some())
            .collect::<Vec<_>>();
        assert_eq!(custom.len(), 2);
        assert!(custom[0].marks.iter().any(|(_, mark)| mark.italic));
        assert!(custom[1].marks.iter().any(|(_, mark)| mark.strikethrough));
        assert!(children.children.iter().any(|child| {
            child.text.as_ref() == "$not-math$"
                && child.marks.iter().any(|(_, mark)| mark.code)
                && child.custom.is_none()
        }));
        assert!(children.children.iter().any(|child| child.image.is_some()));
    }

    #[test]
    fn unclaimed_math_constructs_keep_original_delimiters_as_text() {
        let extensions = MarkdownExtensions::default()
            .parse_options(|options| {
                options.constructs.math_text = true;
                options.constructs.math_flow = true;
            })
            .prepare_source(|source| source.replace(r"\(", "$$").replace(r"\)", "$$"));

        let mut inline_cx = NodeContext {
            markdown_extensions: extensions.clone().into(),
            ..NodeContext::default()
        };
        let inline = parse(r"before \(x\) after", &mut inline_cx).unwrap();
        let BlockNode::Paragraph(paragraph) = &inline.blocks[0] else {
            panic!("expected inline fallback paragraph");
        };
        assert_eq!(paragraph.text(), r"before \(x\) after");

        let mut block_cx = NodeContext {
            markdown_extensions: extensions.into(),
            ..NodeContext::default()
        };
        let block = parse("$$\nx + y\n$$", &mut block_cx).unwrap();
        let BlockNode::Paragraph(paragraph) = &block.blocks[0] else {
            panic!("expected display fallback paragraph");
        };
        assert_eq!(paragraph.text(), "$$\nx + y\n$$");
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
