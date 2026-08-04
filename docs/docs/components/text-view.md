---
title: TextView
description: Renders selectable plain text, Markdown, and HTML with optional custom Markdown plugins.
---

# TextView

`TextView` renders text in GPUI. It supports literal plain text, Markdown, simple HTML, text selection, code block actions, and custom Markdown plugins for project-specific syntax.

## Import

```rust
use gpui_component::text::{markdown, plain, TextView, TextViewState};
```

## Usage

### Plain text

Use the plain-text format when Markdown, HTML, and math-looking syntax must stay literal. Selection and copying return the authoritative source unchanged, and incremental `TextViewState::push_str` updates remain plain text.

```rust
plain("**not bold** <b>not HTML</b> $not_math$")
    .selectable(true)
```

Use `TextView::plain` when the element needs an explicit stable id:

```rust
TextView::plain("message", literal_source).selectable(true)
```

For a stateful view, create and retain a plain state just like a Markdown state:

```rust
let state = cx.new(|cx| TextViewState::plain(source, cx));
TextView::new(&state).selectable(true)
```

### Markdown

Use the `markdown` helper when you only need to render Markdown text:

```rust
use gpui_component::text::markdown;

markdown("# Hello\n\nThis is **Markdown**.")
    .selectable(true)
    .scrollable(true)
```

You can also construct a `TextView` directly when you need a stable id:

```rust
use gpui_component::text::TextView;

TextView::markdown("preview", markdown_source)
    .selectable(true)
```

### HTML

```rust
TextView::html("html-preview", "<strong>Hello</strong>")
```

## Markdown Plugins

Use `.plugin(...)` to support custom Markdown formats. A plugin owns both parsing and rendering, so callers only need to attach it to the `TextView`:

```rust
markdown(source)
    .plugin(TickerPlugin::new())
```

A Markdown plugin implements `MarkdownPlugin`:

```rust
use gpui::{App, IntoElement, ParentElement as _, Window};
use gpui_component::text::{
    markdown_ast, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
};

struct TickerNode {
    symbol: String,
}

struct TickerPlugin;

impl TickerPlugin {
    fn new() -> Self {
        Self
    }
}

impl MarkdownPlugin for TickerPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "ticker"
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Paragraph(paragraph) = node else {
            return None;
        };
        let [markdown_ast::Node::Text(text)] = paragraph.children.as_slice() else {
            return None;
        };
        let symbol = text.value.strip_prefix('$')?;

        Some(
            MarkdownNode::new(
                "ticker",
                TickerNode {
                    symbol: symbol.to_string(),
                },
            )
            .text(format!("${symbol}"))
            .markdown(cx.node_source(node).unwrap_or(text.value.as_str())),
        )
    }

    fn render(
        &self,
        node: &MarkdownNode,
        _window: &mut Window,
        _cx: &mut App,
    ) -> impl IntoElement {
        let ticker = node.data::<TickerNode>().expect("ticker node data");

        gpui::div().child(format!("${}", ticker.symbol))
    }
}
```

Then attach it to a Markdown `TextView`:

```rust
markdown("$AAPL.US")
    .plugin(TickerPlugin::new())
```

## MarkdownNode

`MarkdownNode` is the neutral data passed between `parse` and `render`.

```rust
MarkdownNode::new("ticker", TickerNode { symbol })
    .text("$AAPL.US")
    .markdown("$AAPL.US")
```

- `name` is the stable node name used to match the renderer.
- `data` is typed parser output read with `node.data::<T>()`.
- `text` is the plain text representation used by selection and fallback rendering.
- `markdown` is the Markdown representation used when the document is serialized back to Markdown.

## Block and Inline Extensions

The `MarkdownPlugin` API above remains supported for block-level replacements. Return `true` from `is_block()` so its legacy renderer is used:

```rust
fn is_block(&self) -> bool {
    true
}
```

For an atomic inline node that must coexist with native prose, marks, links, images, headings, selection, and copying, use `MarkdownInlinePlugin` or the typed `MarkdownExtensions::inline_parser` / `inline_renderer` pair. An inline parser claims only the mdast node it understands. Returning `None` leaves the node on TextView's native path; returning `None` from the renderer keeps the node's styled, selectable `MarkdownNode::text` fallback.

Attach a reusable extension registry when an integration needs more than one stage:

```rust
use gpui::SharedString;
use gpui_component::text::MarkdownExtensions;

let extensions = MarkdownExtensions::default()
    .cjk_emphasis_compatibility()
    .parse_options(|options| options.constructs.math_text = true)
    .try_prepare_source(|source| Ok::<_, SharedString>(source.to_string()))
    .inline_parser(parse_formula)
    .inline_renderer("formula", render_formula)
    .block_parser(parse_formula_block)
    .block_renderer("formula", render_formula_block);

TextView::markdown("preview", source).markdown_extensions(extensions)
```

`cjk_emphasis_compatibility` is opt-in. It recognizes the narrow `*` / `**`
flanking patterns used when CJK opening or closing punctuation touches nearby
Han prose, such as `一次**“重点”**说明`. The default remains strict GFM, and
underscore emphasis, escaped markers, code, HTML, and link destinations keep
their native semantics.

## Source Preparation

`prepare_source` and `try_prepare_source` create a parse-only view before mdast conversion. The returned string must preserve both the exact UTF-8 byte length and every character boundary. TextView continues to use the original Markdown for node ranges, selection, copying, serialization, and incremental updates.

Native direct and reference-style images continue to resolve their URL and title through the prepared AST and definitions, while their user-facing alt text is recovered from the authoritative Markdown. A preparer may therefore mask a hazardous delimiter inside an image label without exposing the masked text or losing reference-image resolution.

Use `try_prepare_source` when an integration may be unable to prove that a rewrite is semantically safe. Returning an error aborts parsing instead of publishing a parse view with invalid offsets or changed native Markdown meaning. During incremental parsing, a preparer may receive synthetic retained definitions before the authoritative fragment so reference resolution remains stable; parser callbacks should use `MarkdownParseContext::source`, `node_source`, and `node_range` for the original document view.

Source preparation can run on a background task. Return stable, locale-independent diagnostics there, then use `parse_error_formatter` to turn them into a user-facing message during TextView's UI render pass. Because the formatter runs on every error render, applications can resolve the current locale without rebuilding or reparsing the document.

## Code Block Actions

You can render controls for Markdown code blocks:

```rust
markdown(source)
    .code_block_actions(|code_block, _window, _cx| {
        gpui::div().child(format!("Run {}", code_block.lang().unwrap_or_default()))
    })
```
