use std::{
    any::Any,
    collections::HashMap,
    fmt,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gpui::{AnyElement, App, IntoElement, SharedString, TextStyle, Window};
use markdown::{ParseOptions, mdast};

use crate::text::{
    TextViewStyle,
    inline_flow::InlineMetrics,
    node::{LinkMark, Span, TextMark},
};

use super::inline::SelectableTextState;
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

/// Type for configuring the Markdown parser options used by TextView.
pub type MarkdownParseOptionsFn = dyn Fn(&mut ParseOptions) + Send + Sync;

/// Type for preparing a length-preserving Markdown parse view.
///
/// Incremental parsing may include synthetic retained definitions in this
/// input so context-sensitive preparation preserves reference resolution. The
/// authoritative source exposed by parse contexts and TextView remains the
/// real document fragment. The callback may rewrite any bytes permitted by the
/// length/character-boundary contract; TextView restores its private boundary
/// and falls back to a full authoritative parse if the synthetic definitions
/// can no longer be reconstructed unambiguously.
pub type MarkdownSourcePreparerFn = dyn Fn(&str) -> String + Send + Sync;

/// Type for a fallible length-preserving Markdown source preparer.
///
/// Returning an error aborts parsing before an unsafe parse view can be
/// published. Infallible integrations can continue to use
/// [`MarkdownSourcePreparerFn`].
pub type MarkdownTrySourcePreparerFn = dyn Fn(&str) -> Result<String, SharedString> + Send + Sync;

/// Type for formatting a Markdown parse diagnostic for display.
///
/// Unlike source preparers, this callback is invoked only from TextView's UI
/// render pass. Applications can therefore keep parser errors as stable
/// diagnostic codes and resolve the current locale here.
pub type MarkdownParseErrorFormatterFn = dyn Fn(&str) -> SharedString + Send + Sync;

/// Type for a custom Markdown inline parser.
///
/// Parsers run during Markdown AST conversion, often on a background task. As
/// with block parsers, they must not depend on [`Window`] or [`App`].
pub type MarkdownInlineParserFn =
    dyn for<'a> Fn(&mdast::Node, &MarkdownParseContext<'a>) -> Option<MarkdownNode> + Send + Sync;

/// Type for a custom Markdown inline renderer.
pub type MarkdownInlineRenderFn = dyn Fn(&MarkdownNode, &MarkdownInlineRenderContext, &mut Window, &mut App) -> Option<MarkdownInline>
    + Send
    + Sync;

/// A reusable Markdown extension retained for block-plugin compatibility.
///
/// New inline extensions should implement [`MarkdownInlinePlugin`], whose
/// renderer supplies the metrics required by the native paragraph flow.
pub trait MarkdownPlugin: Send + Sync + 'static {
    /// Whether this plugin produces block-level nodes.
    ///
    /// Existing block plugins should return `true`. A `false` plugin is parsed
    /// as inline fallback text, but its legacy renderer is not invoked because
    /// this trait cannot supply inline metrics.
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

/// A reusable Markdown extension that parses and renders an inline atomic
/// node in TextView's native paragraph flow.
pub trait MarkdownInlinePlugin: Send + Sync + 'static {
    /// Stable name for nodes produced by this plugin.
    fn name(&self) -> &str;

    /// Convert an mdast inline node into a custom Markdown node.
    fn parse(&self, node: &mdast::Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode>;

    /// Render a parsed node as a baseline-aligned inline item.
    ///
    /// Returning `None` keeps the node's delimiter-preserving selectable text
    /// fallback in the paragraph flow.
    fn render(
        &self,
        node: &MarkdownNode,
        context: &MarkdownInlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<MarkdownInline>;
}

/// Context passed to custom Markdown parsers.
pub struct MarkdownParseContext<'a> {
    source: &'a str,
    prepared_source: &'a str,
    offset: usize,
    authoritative_image_alts: &'a HashMap<Range<usize>, SharedString>,
}

impl<'a> MarkdownParseContext<'a> {
    /// A context for one parse of `source` through its equal-length
    /// `prepared_source` view. `authoritative_image_alts` holds the original
    /// alt text of every image whose bytes a source preparer rewrote.
    pub(crate) fn new(
        source: &'a str,
        prepared_source: &'a str,
        offset: usize,
        authoritative_image_alts: &'a HashMap<Range<usize>, SharedString>,
    ) -> Self {
        Self {
            source,
            prepared_source,
            offset,
            authoritative_image_alts,
        }
    }

    /// Source text for the Markdown fragment currently being parsed.
    pub fn source(&self) -> &'a str {
        self.source
    }

    /// Length-preserving source view used to produce the mdast nodes.
    pub fn prepared_source(&self) -> &'a str {
        self.prepared_source
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

    /// Prepared source slice for a specific mdast node.
    pub fn prepared_node_source(&self, node: &mdast::Node) -> Option<&'a str> {
        let position = node.position()?;
        self.prepared_source
            .get(position.start.offset..position.end.offset)
    }

    /// Absolute source range for a specific mdast node.
    pub fn node_range(&self, node: &mdast::Node) -> Option<std::ops::Range<usize>> {
        let position = node.position()?;
        Some(self.offset + position.start.offset..self.offset + position.end.offset)
    }

    pub(crate) fn authoritative_image_alt(&self, node: &mdast::Node) -> Option<&SharedString> {
        let position = node.position()?;
        self.authoritative_image_alts
            .get(&(position.start.offset..position.end.offset))
    }
}

/// Rendered payload for one custom Markdown inline node.
pub struct MarkdownInline {
    metrics: InlineMetrics,
    element: AnyElement,
}

impl MarkdownInline {
    /// Create a baseline-aligned inline payload.
    pub fn new(metrics: InlineMetrics, element: impl IntoElement) -> Self {
        Self {
            metrics,
            element: element.into_any_element(),
        }
    }

    pub(crate) fn into_parts(self) -> (InlineMetrics, AnyElement) {
        (self.metrics, self.element)
    }
}

/// Effective native context for rendering a custom Markdown inline node.
#[derive(Clone)]
pub struct MarkdownInlineRenderContext {
    text_style: TextStyle,
    text_view_style: TextViewStyle,
    heading_level: Option<u8>,
    heading_style: Option<HeadingStyle>,
    mark: TextMark,
    source_range: std::ops::Range<usize>,
}

impl MarkdownInlineRenderContext {
    pub(crate) fn new(
        text_style: TextStyle,
        text_view_style: TextViewStyle,
        heading_level: Option<u8>,
        heading_style: Option<HeadingStyle>,
        mark: TextMark,
        source_range: std::ops::Range<usize>,
    ) -> Self {
        Self {
            text_style,
            text_view_style,
            heading_level,
            heading_style,
            mark,
            source_range,
        }
    }

    /// Fully resolved text style, including heading typography and inherited
    /// Markdown marks.
    pub fn text_style(&self) -> &TextStyle {
        &self.text_style
    }

    /// TextView styling configuration active for this document.
    pub fn text_view_style(&self) -> &TextViewStyle {
        &self.text_view_style
    }

    /// Native heading level containing this node, if any.
    pub fn heading_level(&self) -> Option<u8> {
        self.heading_level
    }

    /// Resolved native heading typography containing this node, if any.
    pub fn heading_style(&self) -> Option<HeadingStyle> {
        self.heading_style
    }

    /// Merged Markdown mark inherited by this atomic node.
    pub fn mark(&self) -> &TextMark {
        &self.mark
    }

    /// Resolved link inherited by this node, including reference links.
    pub fn link(&self) -> Option<&LinkMark> {
        self.mark.link.as_ref()
    }

    /// Absolute byte range in the original Markdown document.
    pub fn source_range(&self) -> std::ops::Range<usize> {
        self.source_range.clone()
    }
}

/// Resolved typography for a custom inline node rendered inside a heading.
///
/// The upstream TextView keeps heading typography as a render-time style
/// refinement rather than exposing a dedicated public type. This small value
/// object lets extensions receive the same information without depending on
/// TextView's internal layout code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadingStyle {
    pub font_size: gpui::Pixels,
    pub font_weight: gpui::FontWeight,
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
    selectable_text_state: Option<SelectableTextState>,
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
            selectable_text_state: None,
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

    /// Attach persistent selection state for one continuous styled text element.
    pub fn selectable_text_state(mut self, state: SelectableTextState) -> Self {
        self.selectable_text_state = Some(state);
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

    /// Selection state attached by [`Self::selectable_text_state`].
    pub fn attached_selectable_text_state(&self) -> Option<&SelectableTextState> {
        self.selectable_text_state.as_ref()
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

    pub(crate) fn ensure_fallback(&mut self, source: &str) {
        if self.text.is_empty() {
            self.text = source.to_string().into();
        }
        if self.markdown.is_empty() {
            self.markdown = source.to_string().into();
        }
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
            .field(
                "has_selectable_text_state",
                &self.selectable_text_state.is_some(),
            )
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
    enable_cjk_emphasis_compatibility: bool,
    parse_options_configurers: Vec<Arc<MarkdownParseOptionsFn>>,
    source_preparers: Vec<Arc<MarkdownTrySourcePreparerFn>>,
    parse_error_formatter: Option<Arc<MarkdownParseErrorFormatterFn>>,
    block_parsers: Vec<Arc<MarkdownBlockParserFn>>,
    block_renderers: HashMap<SharedString, Arc<MarkdownBlockRenderFn>>,
    inline_parsers: Vec<Arc<MarkdownInlineParserFn>>,
    inline_renderers: HashMap<SharedString, Arc<MarkdownInlineRenderFn>>,
    revision: u64,
}

impl MarkdownExtensions {
    /// Accept emphasis next to CJK punctuation where CommonMark's flanking
    /// rules would otherwise leave the markers literal.
    ///
    /// CommonMark intentionally treats punctuation-delimited intraword
    /// emphasis conservatively, so model output such as
    /// `一次**“强调内容”**说明` and `**H01M（电池）**1,560件` remain literal by
    /// default. This opt-in keeps the strict default while recognizing only
    /// those narrow opening- and closing-punctuation patterns for `*` and
    /// `**`. Underscore emphasis, other punctuation, escaped markers, code,
    /// HTML, and link destinations retain the parser's native semantics.
    pub fn cjk_emphasis_compatibility(mut self) -> Self {
        self.enable_cjk_emphasis_compatibility = true;
        self.bump_revision();
        self
    }

    /// Enable MDX JSX/expression constructs.
    ///
    /// This disables raw HTML constructs because `markdown-rs` gives HTML
    /// priority over MDX when both are enabled.
    pub fn mdx(mut self) -> Self {
        self.enable_mdx = true;
        self.bump_revision();
        self
    }

    /// Configure the `markdown-rs` parse options, starting from GFM defaults.
    pub fn parse_options<F>(mut self, configure: F) -> Self
    where
        F: Fn(&mut ParseOptions) + Send + Sync + 'static,
    {
        self.push_parse_options(configure);
        self
    }

    /// Register a source preparation step that runs before mdast parsing.
    ///
    /// The returned parse view must have the same UTF-8 byte length and
    /// character-boundary offsets as its input. TextView retains the original
    /// source for selection, copying, node ranges, and incremental updates.
    /// During an incremental parse, the input may include synthetic definitions
    /// retained from earlier blocks so reference-aware preparation sees the
    /// same document context as markdown-rs. Rewriting their structure is
    /// supported: an incremental reconstruction that becomes ambiguous is
    /// discarded and retried against the full authoritative source.
    pub fn prepare_source<F>(mut self, prepare: F) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        self.push_source_preparer(prepare);
        self
    }

    /// Register a fallible source preparation step that runs before mdast parsing.
    ///
    /// This has the same length and character-boundary contract as
    /// [`Self::prepare_source`]. Returning an error aborts parsing rather than
    /// publishing a parse view whose semantic invariants could not be proven.
    pub fn try_prepare_source<F, E>(mut self, prepare: F) -> Self
    where
        F: Fn(&str) -> Result<String, E> + Send + Sync + 'static,
        E: Into<SharedString>,
    {
        self.push_try_source_preparer(move |source| prepare(source).map_err(Into::into));
        self
    }

    /// Format parse diagnostics at the UI boundary.
    ///
    /// Parsing and source preparation may run on a background task, so those
    /// callbacks should return stable, locale-independent diagnostics. This
    /// formatter runs during rendering and may resolve the application's
    /// current locale. Unconfigured TextViews display the original diagnostic.
    pub fn parse_error_formatter<F, E>(mut self, formatter: F) -> Self
    where
        F: Fn(&str) -> E + Send + Sync + 'static,
        E: Into<SharedString>,
    {
        self.parse_error_formatter = Some(Arc::new(move |error| formatter(error).into()));
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

    /// Register a parser for inline Markdown AST nodes.
    pub fn inline_parser<F>(mut self, parser: F) -> Self
    where
        F: for<'a> Fn(&mdast::Node, &MarkdownParseContext<'a>) -> Option<MarkdownNode>
            + Send
            + Sync
            + 'static,
    {
        self.push_inline_parser(parser);
        self
    }

    /// Register a renderer for a custom inline node name.
    pub fn inline_renderer<F>(mut self, name: impl Into<SharedString>, renderer: F) -> Self
    where
        F: Fn(
                &MarkdownNode,
                &MarkdownInlineRenderContext,
                &mut Window,
                &mut App,
            ) -> Option<MarkdownInline>
            + Send
            + Sync
            + 'static,
    {
        self.push_inline_renderer(name, renderer);
        self
    }

    /// Apply a reusable typed inline Markdown plugin.
    pub fn inline_plugin<P>(self, plugin: P) -> Self
    where
        P: MarkdownInlinePlugin,
    {
        let plugin = Arc::new(plugin);
        let name = SharedString::from(plugin.name().to_string());
        let parser = plugin.clone();
        let renderer = plugin;

        self.inline_parser(move |node, cx| parser.parse(node, cx))
            .inline_renderer(name, move |node, context, window, cx| {
                renderer.render(node, context, window, cx)
            })
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
            // The historical plugin trait cannot supply inline metrics. Keep
            // its parsed text as a safe fallback; new inline plugins should
            // use `inline_plugin` or the typed inline parser/renderer pair.
            self.inline_parser(move |node, cx| parser.parse(node, cx))
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether replacing these extension handles can change the parsed tree.
    ///
    /// Render methods commonly rebuild equivalent plugin closures every frame.
    /// Their globally unique revisions differ, but the parser shape remains
    /// stable; render handles may be refreshed without reparsing the document.
    ///
    /// Every field that participates in producing the document is compared:
    /// parser flags, parse-option configurers, source preparers, block and
    /// inline parsers, and the renderer name sets (a node claimed by a parser
    /// without a renderer falls back differently). `parse_error_formatter`
    /// is deliberately excluded because it only runs while rendering.
    pub(crate) fn has_same_parser_configuration(&self, other: &Self) -> bool {
        fn same_keys<V>(a: &HashMap<SharedString, V>, b: &HashMap<SharedString, V>) -> bool {
            a.len() == b.len() && a.keys().all(|name| b.contains_key(name))
        }

        self.enable_mdx == other.enable_mdx
            && self.enable_cjk_emphasis_compatibility == other.enable_cjk_emphasis_compatibility
            && self.parse_options_configurers.len() == other.parse_options_configurers.len()
            && self.source_preparers.len() == other.source_preparers.len()
            && self.block_parsers.len() == other.block_parsers.len()
            && self.inline_parsers.len() == other.inline_parsers.len()
            && same_keys(&self.block_renderers, &other.block_renderers)
            && same_keys(&self.inline_renderers, &other.inline_renderers)
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

    pub(crate) fn push_parse_options<F>(&mut self, configure: F)
    where
        F: Fn(&mut ParseOptions) + Send + Sync + 'static,
    {
        self.parse_options_configurers.push(Arc::new(configure));
        self.bump_revision();
    }

    pub(crate) fn push_source_preparer<F>(&mut self, prepare: F)
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        self.push_try_source_preparer(move |source| Ok(prepare(source)));
    }

    pub(crate) fn push_try_source_preparer<F>(&mut self, prepare: F)
    where
        F: Fn(&str) -> Result<String, SharedString> + Send + Sync + 'static,
    {
        self.source_preparers.push(Arc::new(prepare));
        self.bump_revision();
    }

    pub(crate) fn push_inline_parser<F>(&mut self, parser: F)
    where
        F: for<'a> Fn(&mdast::Node, &MarkdownParseContext<'a>) -> Option<MarkdownNode>
            + Send
            + Sync
            + 'static,
    {
        self.inline_parsers.push(Arc::new(parser));
        self.bump_revision();
    }

    pub(crate) fn push_inline_renderer<F>(&mut self, name: impl Into<SharedString>, renderer: F)
    where
        F: Fn(
                &MarkdownNode,
                &MarkdownInlineRenderContext,
                &mut Window,
                &mut App,
            ) -> Option<MarkdownInline>
            + Send
            + Sync
            + 'static,
    {
        self.inline_renderers
            .insert(name.into(), Arc::new(renderer));
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

    pub(crate) fn configured_parse_options(&self) -> ParseOptions {
        let mut options = ParseOptions::gfm();
        if self.enable_mdx {
            options.constructs.html_flow = false;
            options.constructs.html_text = false;
            options.constructs.mdx_expression_flow = true;
            options.constructs.mdx_expression_text = true;
            options.constructs.mdx_jsx_flow = true;
            options.constructs.mdx_jsx_text = true;
        }
        for configure in &self.parse_options_configurers {
            configure(&mut options);
        }
        options
    }

    pub(crate) fn cjk_emphasis_compatibility_enabled(&self) -> bool {
        self.enable_cjk_emphasis_compatibility
    }

    pub(crate) fn prepared_source(&self, source: &str) -> Result<String, SharedString> {
        let mut prepared = source.to_string();
        for prepare in &self.source_preparers {
            let next = prepare(&prepared)?;
            if next.len() != prepared.len() {
                return Err(format!(
                    "Markdown source preparation must preserve the same UTF-8 byte length (expected {}, got {})",
                    prepared.len(),
                    next.len()
                )
                .into());
            }

            let boundaries_match = prepared
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(prepared.len()))
                .eq(next
                    .char_indices()
                    .map(|(offset, _)| offset)
                    .chain(std::iter::once(next.len())));
            if !boundaries_match {
                return Err(
                    "Markdown source preparation must preserve the same UTF-8 character boundaries"
                        .into(),
                );
            }
            prepared = next;
        }
        Ok(prepared)
    }

    pub(crate) fn format_parse_error(&self, error: &str) -> SharedString {
        self.parse_error_formatter
            .as_ref()
            .map_or_else(|| error.to_string().into(), |formatter| formatter(error))
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

    pub(crate) fn parse_inline(
        &self,
        node: &mdast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        for parser in &self.inline_parsers {
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

    pub(crate) fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &MarkdownInlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<MarkdownInline> {
        self.inline_renderers
            .get(node.name())
            .and_then(|render| render(node, context, window, cx))
    }

    fn bump_revision(&mut self) {
        self.revision = MARKDOWN_EXTENSIONS_REVISION.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyInlinePlugin;

    impl MarkdownInlinePlugin for DummyInlinePlugin {
        fn name(&self) -> &str {
            "dummy-inline"
        }

        fn parse(
            &self,
            _node: &mdast::Node,
            _cx: &MarkdownParseContext<'_>,
        ) -> Option<MarkdownNode> {
            None
        }

        fn render(
            &self,
            _node: &MarkdownNode,
            _context: &MarkdownInlineRenderContext,
            _window: &mut Window,
            _cx: &mut App,
        ) -> Option<MarkdownInline> {
            None
        }
    }

    struct DummyLegacyInlinePlugin;

    impl MarkdownPlugin for DummyLegacyInlinePlugin {
        fn name(&self) -> &str {
            "legacy-inline"
        }

        fn parse(
            &self,
            node: &mdast::Node,
            _cx: &MarkdownParseContext<'_>,
        ) -> Option<MarkdownNode> {
            let mdast::Node::Text(text) = node else {
                return None;
            };
            Some(MarkdownNode::new(self.name(), ()).text(text.value.clone()))
        }

        fn render(
            &self,
            _node: &MarkdownNode,
            _window: &mut Window,
            _cx: &mut App,
        ) -> impl IntoElement {
            gpui::div()
        }
    }

    #[test]
    fn markdown_extensions_builder_configures_all_extension_stages() {
        let extensions = MarkdownExtensions::default()
            .cjk_emphasis_compatibility()
            .mdx()
            .parse_options(|options| options.constructs.math_text = true)
            .prepare_source(|source| source.replace("ab", "cd"))
            .try_prepare_source(|source| Ok::<_, SharedString>(source.replace("cd", "ef")))
            .parse_error_formatter(|error| format!("localized: {error}"))
            .block_parser(|_, _| None)
            .block_renderer("block", |_, _, _| gpui::div())
            .inline_parser(|_, _| None)
            .inline_renderer("inline", |_, _, _, _| None);

        let options = extensions.configured_parse_options();
        assert!(options.constructs.math_text);
        assert!(extensions.cjk_emphasis_compatibility_enabled());
        assert!(options.constructs.mdx_expression_text);
        assert!(!options.constructs.html_text);
        assert_eq!(extensions.prepared_source("ab").unwrap(), "ef");
        assert_eq!(
            extensions.format_parse_error("preparation-code"),
            "localized: preparation-code"
        );
        assert_eq!(extensions.block_parsers.len(), 1);
        assert_eq!(extensions.block_renderers.len(), 1);
        assert_eq!(extensions.inline_parsers.len(), 1);
        assert_eq!(extensions.inline_renderers.len(), 1);
        assert_ne!(extensions.revision(), 0);

        let fallible = MarkdownExtensions::default()
            .try_prepare_source(|_| Err::<String, _>("preparation rejected"));
        assert_eq!(
            fallible.prepared_source("source").unwrap_err().as_ref(),
            "preparation rejected"
        );

        let plugin_extensions = MarkdownExtensions::default().inline_plugin(DummyInlinePlugin);
        assert_eq!(plugin_extensions.inline_parsers.len(), 1);
        assert!(
            plugin_extensions
                .inline_renderers
                .contains_key("dummy-inline")
        );
    }

    #[test]
    fn legacy_inline_plugin_uses_selectable_fallback_without_block_renderer() {
        let source = "legacy";
        let extensions = MarkdownExtensions::default().plugin(DummyLegacyInlinePlugin);
        let ast = markdown::to_mdast(source, &ParseOptions::gfm()).unwrap();
        let mdast::Node::Root(root) = ast else {
            panic!("expected root");
        };
        let mdast::Node::Paragraph(paragraph) = &root.children[0] else {
            panic!("expected paragraph");
        };
        let alts = HashMap::new();
        let context = MarkdownParseContext::new(source, source, 0, &alts);
        let node = extensions
            .parse_inline(&paragraph.children[0], &context)
            .expect("legacy inline parser should remain usable");

        assert_eq!(node.name(), "legacy-inline");
        assert_eq!(node.as_text(), "legacy");
        assert_eq!(extensions.inline_parsers.len(), 1);
        assert!(extensions.inline_renderers.is_empty());
        assert!(extensions.block_parsers.is_empty());
        assert!(extensions.block_renderers.is_empty());
    }

    #[test]
    fn markdown_node_mixed_flow_builders_preserve_default_and_exact_break_modes() {
        let default = MarkdownNode::new("default-flow", 42_u8)
            .text("visible")
            .markdown("**visible**")
            .heading(2)
            .inline_flow_states([InlineFlowState::default(), InlineFlowState::default()]);
        assert_eq!(default.name(), "default-flow");
        assert_eq!(default.as_text(), "visible");
        assert_eq!(default.as_markdown(), "**visible**");
        assert_eq!(default.data::<u8>(), Some(&42));
        assert_eq!(default.heading_level(), Some(2));
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
