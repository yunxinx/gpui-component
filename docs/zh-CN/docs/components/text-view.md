---
title: TextView
description: 渲染可选择的纯文本、Markdown 与 HTML，并支持自定义 Markdown 插件。
---

# TextView

`TextView` 用于在 GPUI 中渲染文本。它支持字面纯文本、Markdown、简单 HTML、文本选择、代码块操作，以及通过 Markdown 插件解析和渲染项目自定义语法。

## 导入

```rust
use gpui_component::text::{markdown, plain, TextView, TextViewState};
```

## 用法

### 纯文本

当 Markdown、HTML 与类似数学语法的内容必须按字面显示时，请使用纯文本格式。选择和复制会原样返回权威 source，增量 `TextViewState::push_str` 更新也始终保持纯文本语义。

```rust
plain("**不会加粗** <b>不会解析 HTML</b> $不会解析数学$")
    .selectable(true)
```

元素需要显式的稳定 id 时，使用 `TextView::plain`：

```rust
TextView::plain("message", literal_source).selectable(true)
```

需要有状态的视图时，像 Markdown state 一样创建并持有 plain state：

```rust
let state = cx.new(|cx| TextViewState::plain(source, cx));
TextView::new(&state).selectable(true)
```

### Markdown

只需要渲染 Markdown 时，可以使用 `markdown` helper：

```rust
use gpui_component::text::markdown;

markdown("# Hello\n\nThis is **Markdown**.")
    .selectable(true)
    .scrollable(true)
```

如果需要稳定 id，也可以直接构造 `TextView`：

```rust
use gpui_component::text::TextView;

TextView::markdown("preview", markdown_source)
    .selectable(true)
```

### HTML

```rust
TextView::html("html-preview", "<strong>Hello</strong>")
```

## Markdown 插件

使用 `.plugin(...)` 支持自定义 Markdown 格式。插件同时拥有解析和渲染逻辑，调用方只需要把它挂到 `TextView` 上：

```rust
markdown(source)
    .plugin(TickerPlugin::new())
```

Markdown 插件实现 `MarkdownPlugin`：

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

然后挂到 Markdown `TextView`：

```rust
markdown("$AAPL.US")
    .plugin(TickerPlugin::new())
```

## MarkdownNode

`MarkdownNode` 是 `parse` 和 `render` 之间传递的中性数据结构。

```rust
MarkdownNode::new("ticker", TickerNode { symbol })
    .text("$AAPL.US")
    .markdown("$AAPL.US")
```

- `name` 是稳定的节点名称，用于匹配 renderer。
- `data` 是 parser 产生的类型化数据，通过 `node.data::<T>()` 读取。
- `text` 是纯文本表示，用于选择和未注册 renderer 时的回退渲染。
- `markdown` 是 Markdown 表示，用于将文档重新序列化为 Markdown。

## Block 与 Inline 扩展

上面的 `MarkdownPlugin` API 继续兼容 block 级替换。让 `is_block()` 返回 `true`，即可继续使用它的旧版 renderer：

```rust
fn is_block(&self) -> bool {
    true
}
```

需要让原子 inline 节点与原生正文、marks、链接、图片、标题、选择和复制共同工作时，请使用 `MarkdownInlinePlugin`，或类型明确的 `MarkdownExtensions::inline_parser` / `inline_renderer` 组合。Inline parser 只接管自己认识的 mdast 节点；parser 返回 `None` 时保留 TextView 原生路径，renderer 返回 `None` 时则保留带样式、可选择的 `MarkdownNode::text` 文本回退。

如果集成需要组合多个阶段，可以注册一个可复用的扩展表：

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

`cjk_emphasis_compatibility` 是显式启用的兼容选项。它只识别 CJK 开闭标点
与相邻汉字触发的窄范围 `*` / `**` flanking 形式，例如
`一次**“重点”**说明`。默认行为仍是严格 GFM；下划线强调、转义标记、代码、
HTML 和链接目标仍保留原生语义。

## 源码预处理

`prepare_source` 与 `try_prepare_source` 会在 mdast 转换前创建只用于解析的 source view。返回字符串必须同时保持完全相同的 UTF-8 字节长度与全部字符边界。节点范围、选择、复制、序列化和增量更新仍始终使用 TextView 保存的原始 Markdown。

原生直接图片和引用式图片仍通过准备后的 AST 与 definitions 解析 URL 和 title，但面向用户的 alt 文本会从权威 Markdown 恢复。因此，preparer 可以屏蔽图片标签中的危险分隔符，而不会暴露被屏蔽的文本或破坏引用式图片的解析。

如果集成无法始终证明一次改写在语义上安全，请使用 `try_prepare_source`。返回错误会中止解析，不会发布 offset 已失效或原生 Markdown 含义已经改变的 parse view。增量解析时，为了保持引用解析稳定，preparer 可能先收到合成的已保留 definitions，再收到权威 fragment；parser callback 应通过 `MarkdownParseContext::source`、`node_source` 和 `node_range` 读取原始文档视图。

Source preparation 可能在后台任务中运行，因此应在该阶段返回稳定、与语言无关的诊断值，再用 `parse_error_formatter` 在 TextView 的 UI 渲染阶段将其转换为用户可见消息。Formatter 会在每次错误渲染时执行，应用无需重建或重新解析文档即可按当前语言解析消息。

## 代码块操作

可以为 Markdown 代码块渲染操作控件：

```rust
markdown(source)
    .code_block_actions(|code_block, _window, _cx| {
        gpui::div().child(format!("Run {}", code_block.lang().unwrap_or_default()))
    })
```
