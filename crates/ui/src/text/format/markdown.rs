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

/// Parse Markdown into a tree of nodes.
pub(crate) fn parse(source: &str, cx: &mut NodeContext) -> Result<ParsedDocument, SharedString> {
    let prepared_source = cx.markdown_extensions.prepared_source(source)?;
    let options = cx.markdown_extensions.configured_parse_options();
    markdown::to_mdast(&prepared_source, &options)
        .map(|n| ast_to_document(source, &prepared_source, n, cx))
        .map_err(|e| e.to_string().into())
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
    cx: &mut NodeContext,
) -> ParsedDocument {
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
