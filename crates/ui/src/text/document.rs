use gpui::{
    App, Hsla, InteractiveElement as _, IntoElement, ListState, ParentElement as _, SharedString,
    Styled as _, Window, div, px,
};

use crate::text::node::{BlockNode, NodeContext};

/// The parsed document AST.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ParsedDocument {
    pub(crate) source: SharedString,
    pub(crate) blocks: Vec<BlockNode>,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct NodeRenderOptions {
    pub(crate) ix: usize,
    pub(crate) in_list: bool,
    pub(crate) todo: bool,
    pub(crate) ordered: bool,
    pub(crate) depth: usize,
    pub(crate) is_last: bool,
    pub(crate) text_color: Option<Hsla>,
}

impl NodeRenderOptions {
    pub(crate) fn is_last(mut self, is_last: bool) -> Self {
        self.is_last = is_last;
        self
    }
}

impl ParsedDocument {
    pub(super) fn text(&self) -> String {
        let mut text = String::new();
        for block in self.blocks.iter() {
            text.push_str(&block.text());
        }
        text
    }

    pub(super) fn selected_text(&self) -> String {
        let mut text = String::new();
        for block in self.blocks.iter() {
            text.push_str(&block.selected_text());
        }
        text
    }

    /// Synchronously clear the selection stored in every inline state.
    ///
    /// This mirrors the [`selected_text`](Self::selected_text) traversal so the
    /// stored selection can be cleared without relying on a repaint. Offscreen
    /// (virtualized) views do not repaint, so their `InlineState.selection`
    /// would otherwise retain stale values from the last painted frame.
    pub(super) fn clear_selection(&self) {
        for block in self.blocks.iter() {
            block.clear_selection();
        }
    }

    /// Converts the node to markdown format.
    ///
    /// This is used to generate markdown for test.
    #[allow(dead_code)]
    pub(crate) fn to_markdown(&self) -> String {
        self.blocks
            .iter()
            .map(|child| child.to_markdown())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub(super) fn render_root(
        &self,
        list_state: Option<ListState>,
        node_cx: &NodeContext,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let Some(list_state) = list_state else {
            let blocks_len = self.blocks.len();
            return div()
                .id("document")
                .children(self.blocks.iter().enumerate().map(move |(ix, node)| {
                    let is_last = ix + 1 == blocks_len;
                    node.render_block(
                        NodeRenderOptions {
                            ix,
                            is_last,
                            ..Default::default()
                        },
                        node_cx,
                        window,
                        cx,
                    )
                }));
        };

        let options = NodeRenderOptions {
            is_last: true,
            ..Default::default()
        };

        let blocks = &self.blocks;

        if list_state.item_count() != blocks.len() {
            // Keep a bounded scrollbar estimate for blocks outside the
            // viewport. Unknown items otherwise contribute zero height until
            // they are visited, which makes a growing stream's thumb jump
            // taller and then stop reflecting the document length.
            let previous_offset = (list_state.item_count() > 0
                && list_state.viewport_bounds().size.height > px(0.))
            .then(|| list_state.scroll_px_offset_for_scrollbar());
            list_state.reset_with_uniform_height(blocks.len(), window.line_height().max(px(1.)));
            if let Some(previous_offset) = previous_offset {
                list_state.set_offset_from_scrollbar(previous_offset);
            }
        }

        div().id("document").size_full().child(
            gpui::list(list_state, {
                let node_cx = node_cx.clone();
                let blocks = blocks.clone();
                move |ix, window, cx| {
                    let is_last = ix + 1 == blocks.len();
                    blocks[ix]
                        .render_block(
                            NodeRenderOptions {
                                ix,
                                is_last,
                                ..options
                            },
                            &node_cx,
                            window,
                            cx,
                        )
                        .into_any_element()
                }
            })
            .size_full(),
        )
    }
}
