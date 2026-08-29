use gpui::SharedString;

use crate::text::{
    document::ParsedDocument,
    node::{BlockNode, NodeContext, Paragraph, Span},
};

/// Parse authoritative plain text as one selectable paragraph.
pub(crate) fn parse(source: &str, cx: &mut NodeContext) -> Result<ParsedDocument, SharedString> {
    let blocks = if source.is_empty() {
        Vec::new()
    } else {
        let mut paragraph = Paragraph::new(source.to_string());
        paragraph.set_span(Span {
            start: cx.offset,
            end: cx.offset + source.len(),
        });
        vec![BlockNode::Paragraph(paragraph)]
    };

    Ok(ParsedDocument {
        source: source.to_string().into(),
        blocks: blocks.into(),
    })
}
