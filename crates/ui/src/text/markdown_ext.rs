use std::{
    any::Any,
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::{AnyElement, App, IntoElement, SharedString, Window};
use markdown::{ParseOptions, mdast};

use crate::text::node::Span;

use super::inline_flow::InlineFlowState;

static MARKDOWN_EXTENSIONS_REVISION: AtomicU64 = AtomicU64::new(1);

/// Re-export of the Markdown AST types used by custom parsers.
pub use markdown::mdast as markdown_ast;

/// Type for a custom Markdown block parser.
///
/// Parsers run during Markdown AST conversion, often on a background task. They
/// must not depend on [`Window`] or [`App`]; return parsed, reusable data in a
/// [`MarkdownNode`] and render it later with a block renderer.
pub type MarkdownBlockParserFn =
    dyn for<'a> Fn(&mdast::Node, &MarkdownParseContext<'a>) -> Option<MarkdownNode> + Send + Sync;

/// Type for a custom Markdown block renderer.
pub type MarkdownBlockRenderFn =
    dyn Fn(&MarkdownNode, &mut Window, &mut App) -> AnyElement + Send + Sync;

/// A reusable Markdown extension that parses and renders one custom node.
pub trait MarkdownPlugin: Send + Sync + 'static {
    /// Whether this plugin produces block-level nodes.
    ///
    /// Plugins are inline by default. TextView does not support inline custom
    /// Markdown rendering yet, so block plugins should return `true`.
    fn is_block(&self) -> bool {
        false
    }

    /// Stable name for nodes produced by this plugin.
    fn name(&self) -> &str;

    /// Convert an mdast node into a custom Markdown node.
    fn parse(&self, node: &mdast::Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode>;

    /// Render a custom Markdown node produced by this plugin.
    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement;
}

/// Context passed to custom Markdown parsers.
pub struct MarkdownParseContext<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> MarkdownParseContext<'a> {
    pub(crate) fn new(source: &'a str, offset: usize) -> Self {
        Self { source, offset }
    }

    /// Source text for the Markdown fragment currently being parsed.
    pub fn source(&self) -> &'a str {
        self.source
    }

    /// Byte offset of `source` in the full document when parsing an appended
    /// fragment.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Source slice for a specific mdast node.
    pub fn node_source(&self, node: &mdast::Node) -> Option<&'a str> {
        let position = node.position()?;
        self.source.get(position.start.offset..position.end.offset)
    }
}

/// A custom Markdown node produced by [`MarkdownExtensions`].
#[derive(Clone)]
pub struct MarkdownNode {
    name: SharedString,
    text: SharedString,
    markdown: SharedString,
    data: Arc<dyn Any + Send + Sync>,
    inline_flow_states: Vec<InlineFlowState>,
    inline_flow_breaks_before: Option<Vec<usize>>,
    heading_level: Option<u8>,
    pub(crate) span: Option<Span>,
}

impl MarkdownNode {
    /// Create a custom Markdown node with a stable name and typed data.
    pub fn new<T>(name: impl Into<SharedString>, data: T) -> Self
    where
        T: Any + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            text: SharedString::default(),
            markdown: SharedString::default(),
            data: Arc::new(data),
            inline_flow_states: Vec::new(),
            inline_flow_breaks_before: None,
            heading_level: None,
            span: None,
        }
    }

    /// Stable name for this custom node.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Text representation of this custom node.
    pub fn as_text(&self) -> &str {
        &self.text
    }

    /// Markdown representation of this custom node.
    pub fn as_markdown(&self) -> &str {
        &self.markdown
    }

    /// Set the text representation of this custom node.
    pub fn text(mut self, text: impl Into<SharedString>) -> Self {
        self.text = text.into();
        self
    }

    /// Set the Markdown representation of this custom node.
    pub fn markdown(mut self, markdown: impl Into<SharedString>) -> Self {
        self.markdown = markdown.into();
        self
    }

    /// Attach persistent selection state used by a custom mixed inline flow.
    ///
    /// This lets a block plugin reuse [`InlineFlow`](super::InlineFlow) without
    /// teaching the Markdown parser about the plugin's renderer-specific data.
    pub fn inline_flow_state(mut self, state: InlineFlowState) -> Self {
        self.inline_flow_states = vec![state];
        self.inline_flow_breaks_before = None;
        self
    }

    /// Attach persistent selection states for a custom block that renders more
    /// than one independent mixed inline flow.
    pub fn inline_flow_states(mut self, states: impl IntoIterator<Item = InlineFlowState>) -> Self {
        self.inline_flow_states = states.into_iter().collect();
        self.inline_flow_breaks_before = None;
        self
    }

    /// Attach ordered selection states and the exact number of logical line
    /// breaks before each flow.
    ///
    /// Most custom blocks should use [`Self::inline_flow_states`], whose
    /// selected fragments retain the historical single-newline separator.
    /// This variant is for renderers that split one logical Markdown block
    /// into several flows and must preserve consecutive hard breaks across
    /// those renderer boundaries.
    pub fn inline_flow_states_with_breaks(
        mut self,
        states: impl IntoIterator<Item = (InlineFlowState, usize)>,
    ) -> Self {
        let (states, breaks_before): (Vec<_>, Vec<_>) = states.into_iter().unzip();
        self.inline_flow_states = states;
        self.inline_flow_breaks_before = Some(breaks_before);
        self
    }

    /// Mark this custom block as a replacement for a native Markdown heading.
    ///
    /// The TextView block renderer will apply its native heading typography and
    /// spacing around the plugin element.
    pub fn heading(mut self, level: u8) -> Self {
        self.heading_level = Some(level);
        self
    }

    /// Selection state attached by [`Self::inline_flow_state`].
    pub fn attached_inline_flow_state(&self) -> Option<&InlineFlowState> {
        self.inline_flow_states.first()
    }

    /// Selection states attached by [`Self::inline_flow_states`].
    pub fn attached_inline_flow_states(&self) -> &[InlineFlowState] {
        &self.inline_flow_states
    }

    pub(crate) fn attached_inline_flow_breaks_before(&self) -> Option<&[usize]> {
        self.inline_flow_breaks_before.as_deref()
    }

    pub(crate) fn heading_level(&self) -> Option<u8> {
        self.heading_level
    }

    /// Read typed data.
    pub fn data<T>(&self) -> Option<&T>
    where
        T: Any + Send + Sync + 'static,
    {
        self.data.downcast_ref()
    }

    pub(crate) fn set_span(&mut self, span: Option<Span>) {
        self.span = span;
    }

    pub(crate) fn to_markdown(&self) -> String {
        if self.markdown.is_empty() {
            self.text.to_string()
        } else {
            self.markdown.to_string()
        }
    }
}

impl fmt::Debug for MarkdownNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarkdownNode")
            .field("name", &self.name)
            .field("text", &self.text)
            .field("markdown", &self.markdown)
            .field("inline_flow_state_count", &self.inline_flow_states.len())
            .field("inline_flow_breaks_before", &self.inline_flow_breaks_before)
            .field("heading_level", &self.heading_level)
            .field("span", &self.span)
            .finish_non_exhaustive()
    }
}

impl PartialEq for MarkdownNode {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.text == other.text
            && self.markdown == other.markdown
            && self.inline_flow_breaks_before == other.inline_flow_breaks_before
            && self.heading_level == other.heading_level
            && self.span == other.span
    }
}

/// Registry for custom Markdown parsing and rendering.
#[derive(Clone, Default)]
pub struct MarkdownExtensions {
    enable_mdx: bool,
    block_parsers: Vec<Arc<MarkdownBlockParserFn>>,
    block_renderers: HashMap<SharedString, Arc<MarkdownBlockRenderFn>>,
    revision: u64,
}

impl MarkdownExtensions {
    /// Enable MDX JSX/expression constructs.
    ///
    /// This disables raw HTML constructs because `markdown-rs` gives HTML
    /// priority over MDX when both are enabled.
    pub fn mdx(mut self) -> Self {
        self.enable_mdx = true;
        self.bump_revision();
        self
    }

    /// Register a parser for block-level Markdown AST nodes.
    pub fn block_parser<F>(mut self, parser: F) -> Self
    where
        F: for<'a> Fn(&mdast::Node, &MarkdownParseContext<'a>) -> Option<MarkdownNode>
            + Send
            + Sync
            + 'static,
    {
        self.push_block_parser(parser);
        self
    }

    /// Register a renderer for a custom block node name.
    pub fn block_renderer<F, E>(mut self, name: impl Into<SharedString>, renderer: F) -> Self
    where
        F: Fn(&MarkdownNode, &mut Window, &mut App) -> E + Send + Sync + 'static,
        E: IntoElement,
    {
        self.push_block_renderer(name, renderer);
        self
    }

    /// Apply a reusable Markdown plugin.
    pub fn plugin<P>(self, plugin: P) -> Self
    where
        P: MarkdownPlugin,
    {
        let plugin = Arc::new(plugin);
        let name = SharedString::from(plugin.name().to_string());
        let parser = plugin.clone();
        let renderer = plugin;

        if parser.is_block() {
            let mut extensions = self.block_parser(move |node, cx| parser.parse(node, cx));
            extensions.push_block_renderer(name, move |node, window, cx| {
                renderer.render(node, window, cx).into_any_element()
            });
            extensions
        } else {
            panic!("inline Markdown plugins are not supported by TextView yet")
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn push_block_parser<F>(&mut self, parser: F)
    where
        F: for<'a> Fn(&mdast::Node, &MarkdownParseContext<'a>) -> Option<MarkdownNode>
            + Send
            + Sync
            + 'static,
    {
        self.block_parsers.push(Arc::new(parser));
        self.bump_revision();
    }

    pub(crate) fn push_block_renderer<F, E>(&mut self, name: impl Into<SharedString>, renderer: F)
    where
        F: Fn(&MarkdownNode, &mut Window, &mut App) -> E + Send + Sync + 'static,
        E: IntoElement,
    {
        self.block_renderers.insert(
            name.into(),
            Arc::new(move |node, window, cx| renderer(node, window, cx).into_any_element()),
        );
        self.bump_revision();
    }

    pub(crate) fn parse_options(&self) -> ParseOptions {
        let mut options = ParseOptions::gfm();
        if self.enable_mdx {
            options.constructs.html_flow = false;
            options.constructs.html_text = false;
            options.constructs.mdx_expression_flow = true;
            options.constructs.mdx_expression_text = true;
            options.constructs.mdx_jsx_flow = true;
            options.constructs.mdx_jsx_text = true;
        }
        options
    }

    pub(crate) fn parse_block(
        &self,
        node: &mdast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        for parser in &self.block_parsers {
            if let Some(node) = parser(node, cx) {
                return Some(node);
            }
        }
        None
    }

    pub(crate) fn render_block(
        &self,
        node: &MarkdownNode,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.block_renderers
            .get(node.name())
            .map(|render| render(node, window, cx))
    }

    fn bump_revision(&mut self) {
        self.revision = MARKDOWN_EXTENSIONS_REVISION.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_node_mixed_flow_builders_preserve_default_and_exact_break_modes() {
        let default = MarkdownNode::new("default-flow", ())
            .inline_flow_states([InlineFlowState::default(), InlineFlowState::default()]);
        assert_eq!(default.attached_inline_flow_states().len(), 2);
        assert_eq!(default.attached_inline_flow_breaks_before(), None);

        let exact = MarkdownNode::new("exact-flow", ()).inline_flow_states_with_breaks([
            (InlineFlowState::default(), 0),
            (InlineFlowState::default(), 2),
        ]);
        assert_eq!(exact.attached_inline_flow_states().len(), 2);
        assert_eq!(
            exact.attached_inline_flow_breaks_before(),
            Some(&[0, 2][..])
        );
    }
}
