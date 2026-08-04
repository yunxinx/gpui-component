use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, Entity, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, ParentElement, Pixels,
    SharedString, StyleRefinement, Styled, Window, div,
};

use crate::StyledExt;
use crate::scroll::ScrollableElement;
use crate::text::TextViewFormat;
use crate::text::markdown_ext::{
    MarkdownExtensions, MarkdownInline, MarkdownInlineRenderContext, MarkdownNode, MarkdownPlugin,
};
use crate::text::node::CodeBlock;
use crate::text::state::TextViewState;
use crate::{global_state::GlobalState, text::TextViewStyle};

/// Type for code block actions generator function.
pub(crate) type CodeBlockActionsFn =
    dyn Fn(&CodeBlock, &mut Window, &mut App) -> AnyElement + Send + Sync;

/// A text view that can render plain text, Markdown, or HTML.
///
/// ## Goals
///
/// - Provide a rich text rendering component for such as Markdown or HTML,
/// used to display rich text in GPUI application (e.g., Help messages, Release notes)
/// - Support Markdown GFM and HTML (Simple HTML like Safari Reader Mode) for showing most common used markups.
/// - Support Heading, Paragraph, Bold, Italic, StrikeThrough, Code, Link, Image, Blockquote, List, Table, HorizontalRule, CodeBlock ...
///
/// ## Not Goals
///
/// - Customization of the complex style (some simple styles will be supported)
/// - As a Markdown editor or viewer (If you want to like this, you must fork your version).
/// - As a HTML viewer, we not support CSS, we only support basic HTML tags for used to as a content reader.
///
/// See also [`MarkdownElement`], [`HtmlElement`]
#[derive(Clone)]
pub struct TextView {
    id: ElementId,
    format: Option<TextViewFormat>,
    text: Option<SharedString>,
    pub(crate) state: Option<Entity<TextViewState>>,
    text_view_style: TextViewStyle,
    style: StyleRefinement,
    selectable: bool,
    scrollable: bool,
    code_block_actions: Option<Arc<CodeBlockActionsFn>>,
    markdown_extensions: Arc<MarkdownExtensions>,
}

/// A plugin that can configure a [`TextView`].
pub trait TextViewPlugin {
    fn setup(self, text_view: TextView) -> TextView;
}

impl<P> TextViewPlugin for P
where
    P: MarkdownPlugin,
{
    fn setup(self, mut text_view: TextView) -> TextView {
        let extensions = Arc::make_mut(&mut text_view.markdown_extensions);
        let current = std::mem::take(extensions);
        *extensions = current.plugin(self);
        text_view
    }
}

impl Styled for TextView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl TextView {
    /// Create new TextView with managed state.
    pub fn new(state: &Entity<TextViewState>) -> Self {
        Self {
            id: ElementId::Name(state.entity_id().to_string().into()),
            state: Some(state.clone()),
            format: None,
            text: None,
            text_view_style: TextViewStyle::default(),
            style: StyleRefinement::default(),
            selectable: false,
            scrollable: false,
            code_block_actions: None,
            markdown_extensions: Arc::default(),
        }
    }

    /// Create a new plain-text view.
    pub fn plain(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            format: Some(TextViewFormat::Plain),
            text: Some(text.into()),
            text_view_style: TextViewStyle::default(),
            style: StyleRefinement::default(),
            state: None,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
            markdown_extensions: Arc::default(),
        }
    }

    /// Create a new markdown text view.
    pub fn markdown(id: impl Into<ElementId>, markdown: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            format: Some(TextViewFormat::Markdown),
            text: Some(markdown.into()),
            text_view_style: TextViewStyle::default(),
            style: StyleRefinement::default(),
            state: None,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
            markdown_extensions: Arc::default(),
        }
    }

    /// Create a new html text view.
    pub fn html(id: impl Into<ElementId>, html: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            format: Some(TextViewFormat::Html),
            text: Some(html.into()),
            text_view_style: TextViewStyle::default(),
            style: StyleRefinement::default(),
            state: None,
            selectable: false,
            scrollable: false,
            code_block_actions: None,
            markdown_extensions: Arc::default(),
        }
    }

    /// Set [`TextViewStyle`].
    pub fn style(mut self, style: TextViewStyle) -> Self {
        self.text_view_style = style;
        self
    }

    /// Set the text view to be selectable, default is false.
    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    /// Set the text view to be scrollable, default is false.
    ///
    /// ## If true for `scrollable`
    ///
    /// The `scrollable` mode used for large content,
    /// will show scrollbar, but requires the parent to have a fixed height,
    /// and use [`gpui::list`] to render the content in a virtualized way.
    ///
    /// ## If false to fit content
    ///
    /// The TextView will expand to fit all content, no scrollbar.
    /// This mode is suitable for small content, such as a few lines of text, a label, etc.
    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    /// Set custom block actions for code blocks.
    ///
    /// The closure receives the [`CodeBlock`],
    /// and returns an element to display.
    pub fn code_block_actions<F, E>(mut self, f: F) -> Self
    where
        F: Fn(&CodeBlock, &mut Window, &mut App) -> E + Send + Sync + 'static,
        E: IntoElement,
    {
        self.code_block_actions = Some(Arc::new(move |code_block, window, cx| {
            f(&code_block, window, cx).into_any_element()
        }));
        self
    }

    /// Replace the Markdown extension registry.
    pub fn markdown_extensions(mut self, extensions: MarkdownExtensions) -> Self {
        self.markdown_extensions = Arc::new(extensions);
        self
    }

    /// Enable MDX JSX/expression parsing.
    ///
    /// This disables raw HTML parsing because `markdown-rs` gives HTML
    /// priority over MDX when both are enabled.
    pub fn markdown_mdx(mut self) -> Self {
        let extensions = Arc::make_mut(&mut self.markdown_extensions);
        *extensions = extensions.clone().mdx();
        self
    }

    /// Register a custom block-level Markdown parser.
    ///
    /// The parser runs during Markdown AST conversion and must be independent
    /// of [`Window`] / [`App`]. Store any parsed data in [`MarkdownNode`] and
    /// render it later with [`Self::markdown_block_renderer`].
    pub fn markdown_block_parser<F>(mut self, parser: F) -> Self
    where
        F: for<'a> Fn(
                &markdown::mdast::Node,
                &crate::text::MarkdownParseContext<'a>,
            ) -> Option<MarkdownNode>
            + Send
            + Sync
            + 'static,
    {
        Arc::make_mut(&mut self.markdown_extensions).push_block_parser(parser);
        self
    }

    /// Register a renderer for a custom block-level Markdown node name.
    pub fn markdown_block_renderer<F, E>(
        mut self,
        name: impl Into<SharedString>,
        renderer: F,
    ) -> Self
    where
        F: Fn(&MarkdownNode, &mut Window, &mut App) -> E + Send + Sync + 'static,
        E: IntoElement,
    {
        Arc::make_mut(&mut self.markdown_extensions).push_block_renderer(name, renderer);
        self
    }

    /// Configure the `markdown-rs` options used by this TextView.
    pub fn markdown_parse_options<F>(mut self, configure: F) -> Self
    where
        F: Fn(&mut markdown::ParseOptions) + Send + Sync + 'static,
    {
        Arc::make_mut(&mut self.markdown_extensions).push_parse_options(configure);
        self
    }

    /// Register a length-preserving source preparation step.
    pub fn markdown_prepare_source<F>(mut self, prepare: F) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        Arc::make_mut(&mut self.markdown_extensions).push_source_preparer(prepare);
        self
    }

    /// Register a fallible length-preserving source preparation step.
    pub fn markdown_try_prepare_source<F, E>(mut self, prepare: F) -> Self
    where
        F: Fn(&str) -> Result<String, E> + Send + Sync + 'static,
        E: Into<SharedString>,
    {
        Arc::make_mut(&mut self.markdown_extensions)
            .push_try_source_preparer(move |source| prepare(source).map_err(Into::into));
        self
    }

    /// Register a custom inline-level Markdown parser.
    pub fn markdown_inline_parser<F>(mut self, parser: F) -> Self
    where
        F: for<'a> Fn(
                &markdown::mdast::Node,
                &crate::text::MarkdownParseContext<'a>,
            ) -> Option<MarkdownNode>
            + Send
            + Sync
            + 'static,
    {
        Arc::make_mut(&mut self.markdown_extensions).push_inline_parser(parser);
        self
    }

    /// Register a renderer for a custom inline Markdown node name.
    pub fn markdown_inline_renderer<F>(mut self, name: impl Into<SharedString>, renderer: F) -> Self
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
        Arc::make_mut(&mut self.markdown_extensions).push_inline_renderer(name, renderer);
        self
    }

    /// Apply a reusable text view plugin.
    pub fn plugin<P>(self, plugin: P) -> Self
    where
        P: TextViewPlugin,
    {
        plugin.setup(self)
    }
}

impl IntoElement for TextView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub struct TextViewLayoutState {
    state: Entity<TextViewState>,
    element: AnyElement,
}

impl Element for TextView {
    type RequestLayoutState = TextViewLayoutState;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let state = if let Some(state) = self.state.clone() {
            state
        } else {
            let default_format = self.format.unwrap_or(TextViewFormat::Markdown);
            let default_text = self.text.clone().unwrap_or_default();

            let state = window.use_keyed_state(
                SharedString::from(format!("{}/state", self.id)),
                cx,
                move |_, cx| match default_format {
                    TextViewFormat::Plain => TextViewState::plain(default_text.as_str(), cx),
                    TextViewFormat::Markdown => TextViewState::markdown(default_text.as_str(), cx),
                    TextViewFormat::Html => TextViewState::html(default_text.as_str(), cx),
                },
            );
            self.state = Some(state.clone());
            state
        };

        state.update(cx, |state, cx| {
            state.code_block_actions = self.code_block_actions.clone();
            state.set_markdown_extensions(self.markdown_extensions.clone(), cx);
            state.selectable = self.selectable;
            state.scrollable = self.scrollable;
            state.text_view_style = self.text_view_style.clone();

            if let Some(text) = self.text.clone() {
                state.set_text(text.as_str(), cx);
            }
        });

        let focus_handle = state.read(cx).focus_handle.clone();
        let list_state = state.read(cx).list_state.clone();

        let mut el = div()
            .key_context("TextView")
            .track_focus(&focus_handle)
            .when(self.scrollable, |this| {
                this.size_full().vertical_scrollbar(&list_state)
            })
            .relative()
            .on_action(move |_: &crate::input::Copy, window, cx| {
                let text = crate::Root::read(window, cx).window_selected_text_for_copy(cx);
                if text.is_empty() {
                    cx.propagate();
                    return;
                }
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            })
            .on_action(window.listener_for(&state, TextViewState::on_action_select_all))
            .child(state.clone())
            .refine_style(&self.style)
            .into_any_element();
        let layout_id = el.request_layout(window, cx);
        (layout_id, TextViewLayoutState { state, element: el })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        request_layout.element.prepaint(window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = &request_layout.state;
        if self.selectable {
            // Register before painting children so this frame's Inline paint can
            // repopulate the text bounds after stale ones are cleared.
            crate::Root::register_selectable_text_view(state, hitbox, window, cx);
        }

        GlobalState::global_mut(cx)
            .text_view_state_stack
            .push(state.clone());
        request_layout.element.paint(window, cx);
        GlobalState::global_mut(cx).text_view_state_stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::{TextView, TextViewPlugin};
    use std::{
        ops::Range,
        sync::{Arc, Mutex},
    };

    use crate::ActiveTheme as _;
    use crate::text::{
        InlineFlow, InlineFlowItem, InlineMetrics, MarkdownExtensions, MarkdownInline,
        MarkdownNode, TextViewFormat, TextViewState, TextViewStyle,
    };
    use gpui::{
        AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement,
        Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _, Render,
        Styled as _, TestAppContext, VisualTestContext, Window, div, point, px,
    };

    struct TextViewTestRoot {
        text_view: Entity<TextViewState>,
    }

    struct TextViewBuilderTestRoot;

    struct DummyTextViewPlugin;

    impl TextViewPlugin for DummyTextViewPlugin {
        fn setup(self, mut text_view: TextView) -> TextView {
            text_view.selectable = true;
            text_view
        }
    }

    fn text_view_builder_fixture() -> TextView {
        TextView::markdown("builder", "---\n\nab $x$")
            .style(TextViewStyle::default())
            .selectable(true)
            .scrollable(true)
            .code_block_actions(|_, _, _| div())
            .markdown_mdx()
            .markdown_parse_options(|options| options.constructs.math_text = true)
            .markdown_prepare_source(|source| source.replace("ab", "cd"))
            .markdown_try_prepare_source(|source| Ok::<_, &'static str>(source.replace("cd", "ef")))
            .markdown_block_parser(|node, cx| {
                let markdown::mdast::Node::ThematicBreak(_) = node else {
                    return None;
                };
                Some(
                    MarkdownNode::new("builder-block", ())
                        .text(cx.node_source(node).unwrap_or_default()),
                )
            })
            .markdown_block_renderer("builder-block", |_, _, _| {
                div()
                    .debug_selector(|| "text-view-builder-block".into())
                    .size(px(8.))
            })
            .markdown_inline_parser(|node, cx| {
                let markdown::mdast::Node::InlineMath(_) = node else {
                    return None;
                };
                Some(
                    MarkdownNode::new("builder-inline", ())
                        .text(cx.node_source(node).unwrap_or_default()),
                )
            })
            .markdown_inline_renderer("builder-inline", |_, _, _, _| {
                Some(MarkdownInline::new(
                    InlineMetrics::new(px(12.), px(8.), px(2.)),
                    div()
                        .debug_selector(|| "text-view-builder-inline".into())
                        .w(px(12.))
                        .h(px(10.)),
                ))
            })
            .plugin(DummyTextViewPlugin)
    }

    impl Render for TextViewBuilderTestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.))
                .h(px(200.))
                .child(text_view_builder_fixture())
        }
    }

    impl TextViewTestRoot {
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            let text_view = cx.new(|cx| TextViewState::markdown(&text, cx));
            Self { text_view }
        }
    }

    impl Render for TextViewTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(160.))
                .child(
                    div()
                        .h(px(24.))
                        .overflow_hidden()
                        .child(TextView::new(&self.text_view).selectable(true)),
                )
                .child(div().h(px(40.)).child("footer"))
        }
    }

    struct InlineImageTextViewTestRoot {
        text_view: Entity<TextViewState>,
    }

    struct InlineFlowStyleTestRoot {
        small_state: crate::text::InlineFlowState,
        large_state: crate::text::InlineFlowState,
    }

    struct AtomicInlineFlowTextViewTestRoot {
        text_view: Entity<TextViewState>,
        extensions: MarkdownExtensions,
    }

    #[derive(Debug)]
    struct InlineRenderSnapshot {
        heading_level: Option<u8>,
        font_size: gpui::AbsoluteLength,
        font_weight: FontWeight,
        bold: bool,
        link: Option<String>,
        source_range: Range<usize>,
        color: Hsla,
    }

    struct MarkdownInlinePluginTestRoot {
        text_view: Entity<TextViewState>,
        extensions: MarkdownExtensions,
    }

    impl Render for MarkdownInlinePluginTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(320.)).child(
                TextView::new(&self.text_view)
                    .markdown_extensions(self.extensions.clone())
                    .selectable(true),
            )
        }
    }

    impl AtomicInlineFlowTextViewTestRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            let extensions = MarkdownExtensions::default()
                .block_parser(|node, _| {
                    let markdown::mdast::Node::Paragraph(_) = node else {
                        return None;
                    };
                    Some(
                        MarkdownNode::new("atomic-inline-flow", ())
                            .text("before $x$\nafter")
                            .inline_flow_state(Default::default()),
                    )
                })
                .block_renderer("atomic-inline-flow", |node, _, _| {
                    InlineFlow::new(
                        "atomic-inline-flow",
                        node.attached_inline_flow_state()
                            .cloned()
                            .unwrap_or_default(),
                        vec![
                            InlineFlowItem::text("before "),
                            InlineFlowItem::custom(
                                "$x$",
                                InlineMetrics::new(px(24.), px(14.), px(4.)),
                                div()
                                    .debug_selector(|| "atomic-inline-flow-item".into())
                                    .w(px(24.))
                                    .h(px(18.)),
                            )
                            .custom_link("https://example.com/formula"),
                            InlineFlowItem::text("\nafter"),
                        ],
                    )
                });
            let text_view = cx.new(|cx| TextViewState::markdown("probe", cx));
            Self {
                text_view,
                extensions,
            }
        }
    }

    impl Render for AtomicInlineFlowTextViewTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(240.)).child(
                TextView::new(&self.text_view)
                    .markdown_extensions(self.extensions.clone())
                    .selectable(true),
            )
        }
    }

    impl InlineFlowStyleTestRoot {
        fn new() -> Self {
            Self {
                small_state: Default::default(),
                large_state: Default::default(),
            }
        }
    }

    impl Render for InlineFlowStyleTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let items = || {
                vec![
                    crate::text::InlineFlowItem::text("inherited typography"),
                    crate::text::InlineFlowItem::custom(
                        "x",
                        crate::text::InlineMetrics::new(px(1.), px(1.), px(0.)),
                        div().w(px(1.)).h(px(1.)),
                    ),
                ]
            };

            crate::v_flex()
                .child(
                    div()
                        .w(px(300.))
                        .text_size(px(10.))
                        .debug_selector(|| "inline-flow-small-style".into())
                        .child(crate::text::InlineFlow::new(
                            "small-flow",
                            self.small_state.clone(),
                            items(),
                        )),
                )
                .child(
                    div()
                        .w(px(300.))
                        .text_size(px(30.))
                        .debug_selector(|| "inline-flow-large-style".into())
                        .child(crate::text::InlineFlow::new(
                            "large-flow",
                            self.large_state.clone(),
                            items(),
                        )),
                )
        }
    }

    impl InlineImageTextViewTestRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            let text_view = cx.new(|cx| {
                TextViewState::markdown(
                    "Build Status ![inline image](https://example.com/image.svg) after",
                    cx,
                )
            });
            Self { text_view }
        }
    }

    impl Render for InlineImageTextViewTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(420.))
                .child(TextView::new(&self.text_view).selectable(true))
        }
    }

    #[gpui::test]
    fn inline_image_keeps_surrounding_text_on_same_line(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|cx| InlineImageTextViewTestRoot::new(cx));
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let inline_bounds = cx.update(|window, cx| {
            crate::Root::read(window, cx)
                .selectable_text_inlines
                .values()
                .next()
                .cloned()
                .unwrap_or_default()
        });

        assert_eq!(inline_bounds.len(), 2);
        assert_eq!(
            inline_bounds[0].top(),
            inline_bounds[1].top(),
            "text before and after an inline image should share a rendered line"
        );
        assert!(
            inline_bounds[1].left() - inline_bounds[0].right() > px(8.),
            "inline image should reserve horizontal space in the text layout"
        );
        assert!(
            inline_bounds[1].left() - inline_bounds[0].right() < px(40.),
            "unloaded inline image fallback should stay generic and compact"
        );
    }

    #[gpui::test]
    fn inline_flow_measurement_uses_inherited_text_style(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|_| InlineFlowStyleTestRoot::new());
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let small = cx.debug_bounds("inline-flow-small-style").unwrap();
        let large = cx.debug_bounds("inline-flow-large-style").unwrap();
        assert!(
            large.size.height > small.size.height * 2.,
            "captured 30px typography should measure substantially taller than 10px: {small:?} vs {large:?}"
        );
    }

    #[gpui::test]
    fn markdown_inline_renderer_receives_native_heading_mark_link_and_range(
        cx: &mut TestAppContext,
    ) {
        const SOURCE: &str = "# before **[$x$](https://example.com)** after\n\n> quoted $y$";

        cx.update(crate::init);
        let snapshots = Arc::new(Mutex::new(Vec::<InlineRenderSnapshot>::new()));
        let renderer_snapshots = snapshots.clone();
        let extensions = MarkdownExtensions::default()
            .parse_options(|options| options.constructs.math_text = true)
            .inline_parser(|node, _| {
                let markdown::mdast::Node::InlineMath(math) = node else {
                    return None;
                };
                Some(MarkdownNode::new("math-inline", math.value.clone()))
            })
            .inline_renderer("math-inline", move |_, context, _, _| {
                if let Ok(mut snapshots) = renderer_snapshots.lock() {
                    snapshots.push(InlineRenderSnapshot {
                        heading_level: context.heading_level(),
                        font_size: context.text_style().font_size,
                        font_weight: context.text_style().font_weight,
                        bold: context.mark().bold,
                        link: context.link().map(|link| link.url.to_string()),
                        source_range: context.source_range(),
                        color: context.text_style().color,
                    });
                }
                Some(MarkdownInline::new(
                    InlineMetrics::new(px(24.), px(14.), px(4.)),
                    div()
                        .debug_selector(|| "markdown-inline-plugin-item".into())
                        .w(px(24.))
                        .h(px(18.)),
                ))
            });
        let text_view = cx.update(|cx| cx.new(|cx| TextViewState::markdown(SOURCE, cx)));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|_| MarkdownInlinePluginTestRoot {
                text_view: text_view.clone(),
                extensions,
            });
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        assert!(cx.debug_bounds("markdown-inline-plugin-item").is_some());
        let muted_foreground = cx.update(|_, cx| cx.theme().muted_foreground);
        let snapshots = snapshots.lock().unwrap();
        let heading_start = SOURCE.find("$x$").unwrap();
        let heading = snapshots
            .iter()
            .find(|snapshot| snapshot.source_range.start == heading_start)
            .expect("heading inline renderer should run");
        assert_eq!(heading.heading_level, Some(1));
        assert_eq!(heading.font_size, px(28.).into());
        assert_eq!(heading.font_weight, FontWeight::BOLD);
        assert!(heading.bold);
        assert_eq!(heading.link.as_deref(), Some("https://example.com"));
        assert_eq!(heading.source_range, heading_start..heading_start + 3);

        let quote_start = SOURCE.find("$y$").unwrap();
        let quote = snapshots
            .iter()
            .find(|snapshot| snapshot.source_range.start == quote_start)
            .expect("blockquote inline renderer should run");
        assert_eq!(quote.heading_level, None);
        assert_eq!(quote.color, muted_foreground);
    }

    #[gpui::test]
    fn pending_markdown_inline_renderer_keeps_selectable_delimiter_text(cx: &mut TestAppContext) {
        const SOURCE: &str = "before **$x$** after";

        cx.update(crate::init);
        let extensions = MarkdownExtensions::default()
            .parse_options(|options| options.constructs.math_text = true)
            .inline_parser(|node, _| {
                let markdown::mdast::Node::InlineMath(math) = node else {
                    return None;
                };
                Some(MarkdownNode::new("pending-math", math.value.clone()))
            })
            .inline_renderer("pending-math", |_, _, _, _| None);
        let text_view = cx.update(|cx| cx.new(|cx| TextViewState::markdown(SOURCE, cx)));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|_| MarkdownInlinePluginTestRoot {
                text_view: text_view.clone(),
                extensions,
            });
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        text_view.update(cx, |state, cx| state.select_all(cx));

        text_view.read_with(cx, |state, _| {
            assert_eq!(state.selected_text().trim(), "before $x$ after");
            assert_eq!(state.source().as_ref(), SOURCE);
        });
    }

    #[gpui::test]
    fn atomic_inline_flow_participates_in_drag_selection(cx: &mut TestAppContext) {
        use crate::WindowExt as _;

        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(AtomicInlineFlowTextViewTestRoot::new);
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let atomic_bounds = cx.debug_bounds("atomic-inline-flow-item").unwrap();
        let registered_bounds = cx.update(|window, cx| {
            crate::Root::read(window, cx)
                .selectable_text_inlines
                .values()
                .flatten()
                .copied()
                .collect::<Vec<_>>()
        });
        assert!(
            registered_bounds
                .iter()
                .any(|bounds| bounds.contains(&atomic_bounds.center())),
            "atomic inline bounds were not registered for selection: {atomic_bounds:?} vs {registered_bounds:?}"
        );

        let right = point(atomic_bounds.right() - px(1.), atomic_bounds.center().y);
        let left = point(atomic_bounds.left() + px(1.), atomic_bounds.center().y);
        cx.simulate_mouse_down(right, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(left, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(|window, cx| window.selected_text(cx));
        assert_eq!(selected.trim(), "$x$");

        cx.simulate_mouse_up(left, MouseButton::Left, Modifiers::default());
        assert_eq!(
            cx.opened_url(),
            None,
            "a reverse drag selection must not activate the custom link"
        );

        cx.simulate_mouse_down(left, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(right, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(|window, cx| window.selected_text(cx));
        assert_eq!(selected.trim(), "$x$");
        cx.simulate_mouse_up(right, MouseButton::Left, Modifiers::default());
        assert_eq!(
            cx.opened_url(),
            None,
            "a forward drag selection must not activate the custom link"
        );

        cx.update(|window, cx| {
            window.end_text_selection(cx);
            let _ = window.draw(cx);
        });
        cx.simulate_click(atomic_bounds.center(), Modifiers::default());
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://example.com/formula")
        );
    }

    #[gpui::test]
    fn atomic_inline_flow_drag_preserves_hard_breaks(cx: &mut TestAppContext) {
        use crate::WindowExt as _;

        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(AtomicInlineFlowTextViewTestRoot::new);
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let mut registered_bounds = cx.update(|window, cx| {
            crate::Root::read(window, cx)
                .selectable_text_inlines
                .values()
                .flatten()
                .copied()
                .collect::<Vec<_>>()
        });
        registered_bounds.sort_by(|left, right| {
            left.top()
                .partial_cmp(&right.top())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    left.left()
                        .partial_cmp(&right.left())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        assert_eq!(registered_bounds.len(), 3);

        let first = registered_bounds[0];
        let last = registered_bounds[2];
        let drag_start = point(last.right() - px(1.), last.center().y);
        let drag_end = point(first.left() + px(1.), first.center().y);
        cx.simulate_mouse_down(drag_start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(drag_end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(|window, cx| window.selected_text(cx));
        assert_eq!(selected.trim(), "before $x$\nafter");
    }

    #[gpui::test]
    fn list_item_renders_fenced_code_block_at_document_width(cx: &mut TestAppContext) {
        struct ListItemBlockRoot;

        impl Render for ListItemBlockRoot {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(840.)).h(px(400.)).child(
                    crate::resizable::h_resizable("markdown-width-test")
                        .child(crate::resizable::resizable_panel().child(div()))
                        .child(crate::resizable::resizable_panel().child(
                            TextView::markdown(
                                "list-with-code",
                                "1. List item\n   ```rust\n   nested code\n   ```\n\n```rust\ntop-level code\n```",
                            )
                            .code_block_actions(|code_block, _, _| {
                                let selector = if code_block.code().contains("nested") {
                                    "nested-code-action"
                                } else {
                                    "top-level-code-action"
                                };
                                div()
                                    .debug_selector(move || selector.into())
                                    .child("Copy")
                            })
                            .scrollable(true)
                            .p_5()
                            .flex_none(),
                        )),
                )
            }
        }

        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|_, _| ListItemBlockRoot);
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let nested_action = cx.debug_bounds("nested-code-action").unwrap();
        let top_level_action = cx.debug_bounds("top-level-code-action").unwrap();
        assert!(
            top_level_action.right() - nested_action.right() < px(32.),
            "nested code block should fill the list item's available width"
        );
    }

    #[test]
    fn plugin_accepts_text_view_plugins_beyond_markdown() {
        let view = TextView::markdown("plugin-test", "").plugin(DummyTextViewPlugin);

        assert!(view.selectable);
    }

    #[gpui::test]
    fn test_text_view_builder(cx: &mut TestAppContext) {
        let view = text_view_builder_fixture();
        let plain = TextView::plain("plain-builder", "**literal**").selectable(true);
        let plain_helper = crate::text::plain("<literal>");

        assert!(view.format == Some(TextViewFormat::Markdown));
        assert_eq!(view.text.as_deref(), Some("---\n\nab $x$"));
        assert!(view.state.is_none());
        assert!(view.selectable);
        assert!(view.scrollable);
        assert!(view.code_block_actions.is_some());
        assert!(plain.format == Some(TextViewFormat::Plain));
        assert_eq!(plain.text.as_deref(), Some("**literal**"));
        assert!(plain.selectable);
        assert!(plain_helper.format == Some(TextViewFormat::Plain));
        assert_eq!(plain_helper.text.as_deref(), Some("<literal>"));

        let options = view.markdown_extensions.configured_parse_options();
        assert!(options.constructs.mdx_expression_text);
        assert!(options.constructs.math_text);
        assert_eq!(
            view.markdown_extensions.prepared_source("ab").unwrap(),
            "ef"
        );
        assert_ne!(view.markdown_extensions.revision(), 0);

        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|_| TextViewBuilderTestRoot);
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        assert!(cx.debug_bounds("text-view-builder-block").is_some());
        assert!(cx.debug_bounds("text-view-builder-inline").is_some());
    }

    #[gpui::test]
    fn plain_text_copy_preserves_source_boundaries(cx: &mut TestAppContext) {
        const SOURCE: &str = "  **literal**\n";

        cx.update(crate::init);
        let text_view = cx.update(|cx| cx.new(|cx| TextViewState::plain(SOURCE, cx)));
        let focus_handle = text_view.read_with(cx, |state, _| state.focus_handle.clone());
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|_| TextViewTestRoot {
                text_view: text_view.clone(),
            });
            crate::Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            focus_handle.focus(window, cx);
        });
        text_view.update(cx, |state, cx| state.select_all(cx));
        cx.dispatch_action(crate::input::Copy);

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(SOURCE.to_string())
        );
    }

    #[gpui::test]
    fn clipped_markdown_link_does_not_open(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|_, cx| {
            TextViewTestRoot::new("visible\n\n[hidden](https://example.com)", cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.simulate_click(point(px(10.), px(34.)), Modifiers::default());

        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    fn visible_markdown_link_still_opens(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, cx) =
            cx.add_window_view(|_, cx| TextViewTestRoot::new("[visible](https://example.com)", cx));
        let cx: &mut VisualTestContext = cx;

        cx.simulate_click(point(px(10.), px(10.)), Modifiers::default());

        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[gpui::test]
    fn clipped_markdown_cannot_start_selection(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (view, cx) = cx
            .add_window_view(|_, cx| TextViewTestRoot::new("visible\n\nhidden selection text", cx));
        let cx: &mut VisualTestContext = cx;

        cx.simulate_mouse_down(
            point(px(10.), px(34.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(90.), px(34.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(90.), px(34.)),
            MouseButton::Left,
            Modifiers::default(),
        );

        let selected_text = view.read_with(cx, |root, cx| root.text_view.read(cx).selected_text());
        assert!(
            selected_text.is_empty(),
            "unexpected selection: {selected_text:?}"
        );
    }

    #[gpui::test]
    fn double_click_selects_word(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (view, cx) =
            cx.add_window_view(|_, cx| TextViewTestRoot::new("quick select value", cx));

        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let position = point(px(10.), px(16.));
        cx.simulate_event(MouseDownEvent {
            position,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 2,
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected_text = view.read_with(cx, |root, cx| root.text_view.read(cx).selected_text());
        assert_eq!(selected_text.trim(), "quick");
    }

    #[gpui::test]
    fn triple_click_selects_paragraph(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (view, cx) =
            cx.add_window_view(|_, cx| TextViewTestRoot::new("quick select value", cx));

        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let position = point(px(10.), px(10.));
        cx.simulate_event(MouseDownEvent {
            position,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 3,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 3,
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected_text = view.read_with(cx, |root, cx| root.text_view.read(cx).selected_text());
        assert_eq!(selected_text.trim(), "quick select value");
    }

    // Regression: markdown `TextView` items inside an outer `gpui::list` with
    // `measure_all` must keep a stable total content height while scrolling.
    // Before synchronous full-replace parsing, off-screen markdown views were
    // first measured with empty content and the scrollbar thumb jittered as the
    // total height grew during scrolling.
    #[gpui::test]
    fn outer_list_content_total_stable_while_scrolling(cx: &mut TestAppContext) {
        use gpui::{ListAlignment, ListState, list};

        const ITEMS: &[&str] = &[
            "# Heading\n\nA paragraph long enough to wrap across several lines and produce a non-trivial height.",
            "Short.",
            "Paragraph A\n\nParagraph B\n\nParagraph C with more words to increase the height.",
            "## Subheading\n\n- One\n- Two\n- Three\n\nClosing paragraph.",
            "Only one line.",
            "**Bold**: medium length text with `code` mixed with regular words.",
            "1. First\n2. Second\n3. Third\n\nA short closing paragraph.",
            "A long message with enough words to wrap across multiple lines, create a taller item, and verify that off-screen measurement matches visible measurement.",
        ];
        let n = 40usize;

        struct ListRoot {
            state: ListState,
        }
        impl Render for ListRoot {
            fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                div().w(px(360.)).h(px(500.)).child(
                    list(self.state.clone(), |ix, _w, _cx| {
                        div()
                            .w_full()
                            .child(TextView::markdown(
                                ("md", ix as u64),
                                ITEMS[ix % ITEMS.len()],
                            ))
                            .into_any_element()
                    })
                    .size_full(),
                )
            }
        }

        cx.update(crate::init);
        let state = ListState::new(n, ListAlignment::Top, px(2048.)).measure_all();
        let probe = state.clone();
        let (_view, cx) = cx.add_window_view(|_w, _cx| ListRoot { state });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|w, cx| {
            let _ = w.draw(cx);
        });
        cx.run_until_parked();
        cx.update(|w, cx| {
            let _ = w.draw(cx);
        });

        let total = |p: &ListState| {
            f32::from(p.max_offset_for_scrollbar().y + p.viewport_bounds().size.height)
        };
        let mut totals = vec![total(&probe)];
        for _ in 0..20 {
            probe.scroll_by(px(150.));
            cx.update(|w, cx| {
                let _ = w.draw(cx);
            });
            cx.run_until_parked();
            totals.push(total(&probe));
        }
        let min = totals.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = totals.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        println!(
            "OUTER_LIST_PROBE min={min:.1} max={max:.1} delta={:.1}",
            max - min
        );
        assert!(
            (max - min) < 2.0,
            "list content total jittered while scrolling: min={min} max={max} totals={totals:?}"
        );
    }
}
