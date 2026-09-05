use std::{cell::Cell, ops::Range, rc::Rc};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Length, Pixels, Style, WeakEntity, Window, point,
    px, relative, size,
};

use super::{
    TextViewState,
    block_heights::{BlockHeightCache, BlockTypography},
    document::{NodeRenderOptions, ParsedDocument, estimated_block_height},
    node::NodeContext,
};

/// A natural-height document whose children are materialized after its parent
/// has applied the current scroll offset and content mask.
pub(super) struct WindowedDocument {
    document: ParsedDocument,
    node_cx: NodeContext,
    heights: BlockHeightCache,
    state: WeakEntity<TextViewState>,
    renderer_revision: u64,
}

impl WindowedDocument {
    pub(super) fn new(
        document: ParsedDocument,
        node_cx: NodeContext,
        heights: BlockHeightCache,
        state: WeakEntity<TextViewState>,
        renderer_revision: u64,
    ) -> Self {
        Self {
            document,
            node_cx,
            heights,
            state,
            renderer_revision,
        }
    }
}

/// The clipped viewport, expanded in document coordinates for bounded overdraw.
fn visible_range(bounds: Bounds<Pixels>, viewport: Bounds<Pixels>) -> Option<Range<Pixels>> {
    if bounds.size.height == px(0.)
        && bounds.left() < viewport.right()
        && bounds.right() > viewport.left()
        && bounds.top() >= viewport.top()
        && bounds.top() < viewport.bottom()
    {
        // A pending image or custom block may acquire intrinsic height later.
        return Some(px(0.)..viewport.size.height * 3.);
    }
    let visible = bounds.intersect(&viewport);
    if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
        return None;
    }
    let overdraw = viewport.size.height * 2.;
    Some(
        (visible.top() - bounds.top() - overdraw).max(px(0.))
            ..visible.bottom() - bounds.top() + overdraw,
    )
}

impl IntoElement for WindowedDocument {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WindowedDocument {
    type RequestLayoutState = (bool, Rc<Cell<bool>>);
    type PrepaintState = Vec<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        Some("document".into())
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
        // Measurement callbacks run after ancestor text-style scopes unwind.
        let typography = BlockTypography::capture(window, cx);
        let estimated = estimated_block_height(&self.node_cx.style, window);
        let style = self.node_cx.style.clone();
        let renderer_revision = self.renderer_revision;
        let count = self.document.blocks.len();
        let heights = self.heights.clone();
        let was_complete = heights.is_complete();
        heights.align(count, estimated);
        let invalidated = Rc::new(Cell::new(false));
        let invalidated_in_layout = invalidated.clone();

        let layout_style = Style {
            size: size(relative(1.).into(), Length::Auto),
            flex_shrink: 0.,
            ..Default::default()
        };
        let layout_id =
            window.request_measured_layout(layout_style, move |known, available, _, _| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                if let Some(width) = width {
                    let changed =
                        heights.prepare(count, width, &typography, &style, renderer_revision);
                    invalidated_in_layout.set(invalidated_in_layout.get() || changed);
                }
                size(width.unwrap_or_default(), heights.total_height())
            });
        (layout_id, (was_complete, invalidated))
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout_invalidated: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let invalidated = self.heights.prepare(
            self.document.blocks.len(),
            bounds.size.width,
            &BlockTypography::capture(window, cx),
            &self.node_cx.style,
            self.renderer_revision,
        ) || layout_invalidated.1.get();
        let mut elements = Vec::new();
        let mut measurements = Vec::new();
        if let Some(range) = visible_range(bounds, window.content_mask().bounds)
            && let Some(first) = self.heights.first_block_for_y(range.start)
        {
            let mut y = self.heights.sum_range(0..first);
            for ix in first..self.document.blocks.len() {
                if y >= range.end {
                    break;
                }
                let mut element = self.document.blocks[ix].render_block(
                    NodeRenderOptions {
                        ix,
                        is_last: ix + 1 == self.document.blocks.len(),
                        ..Default::default()
                    },
                    &self.node_cx,
                    window,
                    cx,
                );
                let measured = element.layout_as_root(
                    size(
                        AvailableSpace::Definite(bounds.size.width),
                        AvailableSpace::MaxContent,
                    ),
                    window,
                    cx,
                );
                element.prepaint_at(bounds.origin + point(px(0.), y), window, cx);
                y += measured.height;
                measurements.push((ix, measured.height));
                elements.push(element);
            }
        }

        self.heights.measure_many(measurements);
        let changed = (self.heights.total_height() - bounds.size.height).abs() > px(0.5)
            || self.heights.is_complete() != layout_invalidated.0;
        // Rebuilt callback closures can invalidate unseen measurements without
        // changing effective geometry. Only a changed result needs another frame.
        if changed || invalidated {
            let _ = self.state.update(cx, |state, cx| {
                if invalidated {
                    state.selection_revision = state.selection_revision.wrapping_add(1);
                }
                if changed {
                    // GPUI suppresses observer delivery during drawing.
                    cx.defer_in(window, |_, _, cx| cx.notify());
                }
            });
        }
        elements
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        elements: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for element in elements {
            element.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_uses_the_clipped_current_frame_bounds() {
        let viewport = Bounds::new(point(px(20.), px(100.)), size(px(300.), px(200.)));
        let document = Bounds::new(point(px(20.), px(-900.)), size(px(300.), px(4000.)));
        assert_eq!(visible_range(document, viewport), Some(px(600.)..px(1600.)));
        let outside = Bounds::new(point(px(20.), px(400.)), document.size);
        assert_eq!(visible_range(outside, viewport), None);
    }
}
