use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex},
};

use gpui::{
    AbsoluteLength, AnyElement, App, AvailableSpace, Bounds, ClickEvent, DefiniteLength, Element,
    ElementId, GlobalElementId, HighlightStyle, Hitbox, HitboxBehavior, InspectorElementId,
    InteractiveElement as _, IntoElement, LayoutId, LineFragment as WrapLineFragment, MouseButton,
    MouseClickEvent, MouseDownEvent, MouseUpEvent, ObjectFit, Pixels, ShapedLine, SharedString,
    SharedUri, Size, StatefulInteractiveElement as _, Styled, StyledImage as _, TextRun, TextStyle,
    WhiteSpace, Window, img, point, prelude::FluentBuilder as _, px, relative, size,
};

use crate::{
    GlobalState,
    input::Selection,
    text::{
        TextViewMultiClickKind,
        text_view::{LinkClickHandlerFn, handle_link_click},
    },
    theme::ActiveTheme as _,
};

use super::{
    inline::{Inline, InlineState, point_in_text_selection},
    node::LinkMark,
    utils::image_source,
};

const IMAGE_LEN: usize = 1;

/// Persistent selection state for a mixed inline flow.
#[derive(Clone, Default, Debug)]
pub struct InlineFlowState {
    inner: Arc<Mutex<InlineFlowStateInner>>,
}

#[derive(Default, Debug)]
struct InlineFlowStateInner {
    fragments: Vec<InlineFlowSelectionFragment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct InlineFlowSelectionKey {
    item_ix: usize,
    source_start: usize,
    source_end: usize,
}

#[derive(Debug)]
struct InlineFlowSelectionFragment {
    key: InlineFlowSelectionKey,
    state: Arc<Mutex<InlineState>>,
    hard_breaks_before: usize,
}

impl InlineFlowState {
    pub fn selected_text(&self) -> String {
        let Ok(inner) = self.inner.lock() else {
            return String::new();
        };
        let mut selected = String::new();
        let mut has_selected = false;
        for fragment in &inner.fragments {
            let Ok(state) = fragment.state.lock() else {
                continue;
            };
            let Some(selection) = &state.selection else {
                continue;
            };
            let start = selection.start.min(selection.end).min(state.text.len());
            let end = selection.start.max(selection.end).min(state.text.len());
            if let Some(text) = state.text.get(start..end) {
                if !text.is_empty() && has_selected {
                    selected.extend(std::iter::repeat_n('\n', fragment.hard_breaks_before));
                }
                selected.push_str(text);
                has_selected |= !text.is_empty();
            }
        }
        selected
    }

    pub fn clear_selection(&self) {
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        for fragment in &inner.fragments {
            if let Ok(mut state) = fragment.state.lock() {
                state.selection = None;
            }
        }
    }

    fn synchronize(
        &self,
        fragments: &[PositionedFragment],
        items: &[InlineFlowItem],
    ) -> Vec<Option<Arc<Mutex<InlineState>>>> {
        let Ok(mut inner) = self.inner.lock() else {
            return vec![None; fragments.len()];
        };
        let mut previous = std::mem::take(&mut inner.fragments)
            .into_iter()
            .map(|fragment| (fragment.key, fragment.state))
            .collect::<HashMap<_, _>>();
        let mut current = Vec::new();
        let mut states = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            let Some((key, text, hard_breaks_before)) = fragment.selection_content() else {
                states.push(None);
                continue;
            };
            let state = match fragment {
                PositionedFragment::Text {
                    item_ix,
                    source_range,
                    ..
                } => match items.get(*item_ix) {
                    Some(InlineFlowItem::Text { state, text, .. })
                        if source_range.start == 0 && source_range.end == text.len() =>
                    {
                        state.clone()
                    }
                    _ => previous
                        .remove(&key)
                        .unwrap_or_else(|| Arc::new(Mutex::new(InlineState::default()))),
                },
                _ => previous
                    .remove(&key)
                    .unwrap_or_else(|| Arc::new(Mutex::new(InlineState::default()))),
            };
            if let Ok(mut state) = state.lock() {
                state.set_text(text);
            }
            current.push(InlineFlowSelectionFragment {
                key,
                state: state.clone(),
                hard_breaks_before,
            });
            states.push(Some(state));
        }
        inner.fragments = current;
        states
    }
}

/// Baseline metrics for an atomic inline element.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InlineMetrics {
    width: Pixels,
    ascent: Pixels,
    descent: Pixels,
}

impl InlineMetrics {
    pub fn new(width: Pixels, ascent: Pixels, descent: Pixels) -> Self {
        Self {
            width: width.max(Pixels::ZERO),
            ascent: ascent.max(Pixels::ZERO),
            descent: descent.max(Pixels::ZERO),
        }
    }
    pub fn width(self) -> Pixels {
        self.width
    }
    pub fn ascent(self) -> Pixels {
        self.ascent
    }
    pub fn descent(self) -> Pixels {
        self.descent
    }
    fn size(self) -> Size<Pixels> {
        size(self.width, self.ascent + self.descent)
    }
}

pub struct InlineFlow {
    id: ElementId,
    state: InlineFlowState,
    items: Vec<InlineFlowItem>,
    link_click_handler: Option<Arc<LinkClickHandlerFn>>,
}

// The text variant keeps its selection state private; callers should use the
// constructors below rather than constructing the enum fields directly.
#[allow(private_interfaces)]
pub enum InlineFlowItem {
    Text {
        state: Arc<Mutex<InlineState>>,
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    },
    Image {
        url: SharedUri,
        link: Option<LinkMark>,
        title: String,
        width: Option<DefiniteLength>,
        height: Option<DefiniteLength>,
    },
    Custom {
        text: SharedString,
        metrics: InlineMetrics,
        element: Option<AnyElement>,
        link: Option<LinkMark>,
    },
}

impl InlineFlowItem {
    pub fn text(text: impl Into<SharedString>) -> Self {
        Self::text_with_state(text, Arc::new(Mutex::new(InlineState::default())))
    }

    pub(crate) fn text_with_state(
        text: impl Into<SharedString>,
        state: Arc<Mutex<InlineState>>,
    ) -> Self {
        Self::Text {
            state,
            text: text.into(),
            links: Vec::new(),
            highlights: Vec::new(),
        }
    }

    pub fn highlights(mut self, highlights: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        if let Self::Text {
            highlights: current,
            ..
        } = &mut self
        {
            *current = highlights;
        }
        self
    }

    pub fn link(mut self, range: Range<usize>, url: impl Into<SharedString>) -> Self {
        if let Self::Text { links, .. } = &mut self {
            links.push((
                range,
                LinkMark {
                    url: url.into(),
                    identifier: None,
                    title: None,
                },
            ));
        }
        self
    }

    pub fn custom(
        text: impl Into<SharedString>,
        metrics: InlineMetrics,
        element: impl IntoElement,
    ) -> Self {
        Self::Custom {
            text: text.into(),
            metrics,
            element: Some(element.into_any_element()),
            link: None,
        }
    }

    pub fn custom_link(mut self, url: impl Into<SharedString>) -> Self {
        if let Self::Custom { link, .. } = &mut self {
            *link = Some(LinkMark {
                url: url.into(),
                identifier: None,
                title: None,
            });
        }
        self
    }

    pub(crate) fn with_custom_link_mark(mut self, link: Option<LinkMark>) -> Self {
        if let Self::Custom { link: current, .. } = &mut self {
            *current = link;
        }
        self
    }

    pub(crate) fn with_links(mut self, links: Vec<(Range<usize>, LinkMark)>) -> Self {
        if let Self::Text { links: current, .. } = &mut self {
            *current = links;
        }
        self
    }

    pub fn image(
        url: impl Into<SharedUri>,
        title: impl Into<String>,
        width: Option<DefiniteLength>,
        height: Option<DefiniteLength>,
    ) -> Self {
        Self::Image {
            url: url.into(),
            link: None,
            title: title.into(),
            width,
            height,
        }
    }

    pub fn image_link(mut self, url: impl Into<SharedString>) -> Self {
        if let Self::Image { link, .. } = &mut self {
            *link = Some(LinkMark {
                url: url.into(),
                identifier: None,
                title: None,
            });
        }
        self
    }

    pub(crate) fn with_image_link_mark(mut self, link: Option<LinkMark>) -> Self {
        if let Self::Image { link: current, .. } = &mut self {
            *current = link;
        }
        self
    }
}

#[derive(Default)]
pub struct InlineFlowLayoutState {
    layout: Arc<Mutex<Option<InlineFlowLayout>>>,
}

#[derive(Default)]
struct InlineFlowLayout {
    fragments: Vec<PositionedFragment>,
    size: Size<Pixels>,
}

#[derive(Clone)]
enum PositionedFragment {
    Text {
        item_ix: usize,
        origin: gpui::Point<Pixels>,
        size: Size<Pixels>,
        source_range: Range<usize>,
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        hard_breaks_before: usize,
    },
    Image {
        item_ix: usize,
        origin: gpui::Point<Pixels>,
        size: Size<Pixels>,
    },
    Custom {
        item_ix: usize,
        origin: gpui::Point<Pixels>,
        size: Size<Pixels>,
        text: SharedString,
        hard_breaks_before: usize,
    },
}

impl PositionedFragment {
    fn selection_content(&self) -> Option<(InlineFlowSelectionKey, SharedString, usize)> {
        match self {
            Self::Text {
                item_ix,
                source_range,
                text,
                hard_breaks_before,
                ..
            } => (!text.is_empty()).then(|| {
                (
                    InlineFlowSelectionKey {
                        item_ix: *item_ix,
                        source_start: source_range.start,
                        source_end: source_range.end,
                    },
                    text.clone(),
                    *hard_breaks_before,
                )
            }),
            Self::Custom {
                item_ix,
                text,
                hard_breaks_before,
                ..
            } => (!text.is_empty()).then(|| {
                (
                    InlineFlowSelectionKey {
                        item_ix: *item_ix,
                        source_start: 0,
                        source_end: text.len(),
                    },
                    text.clone(),
                    *hard_breaks_before,
                )
            }),
            Self::Image { .. } => None,
        }
    }
}

enum MeasureItem {
    Text {
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    },
    Custom {
        text: SharedString,
        metrics: InlineMetrics,
    },
    Image {
        url: SharedUri,
        width: Option<DefiniteLength>,
        height: Option<DefiniteLength>,
    },
}

struct LineFragmentLayout {
    item_ix: usize,
    kind: LineFragmentKind,
    size: Size<Pixels>,
    source_range: Range<usize>,
    alignment: FragmentAlignment,
}

enum LineFragmentKind {
    Text {
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    },
    Image,
    Custom {
        text: SharedString,
    },
}

#[derive(Clone, Copy)]
enum FragmentAlignment {
    Baseline { ascent: Pixels, descent: Pixels },
    Center,
}

#[derive(Default)]
pub struct InlineFlowPrepaintState {
    fragments: Vec<PrepaintFragment>,
}

enum PrepaintFragment {
    Element(AnyElement),
    Atomic {
        element: AnyElement,
        state: Arc<Mutex<InlineState>>,
        text: SharedString,
        link: Option<LinkMark>,
        bounds: Bounds<Pixels>,
        hitbox: Hitbox,
    },
}

impl InlineFlow {
    pub fn new(
        id: impl Into<ElementId>,
        state: InlineFlowState,
        items: Vec<InlineFlowItem>,
    ) -> Self {
        Self {
            id: id.into(),
            state,
            items,
            link_click_handler: None,
        }
    }

    pub(super) fn with_link_click_handler(
        mut self,
        link_click_handler: Option<Arc<LinkClickHandlerFn>>,
    ) -> Self {
        self.link_click_handler = link_click_handler;
        self
    }

    fn image_element(
        ix: usize,
        url: &SharedUri,
        link: &Option<LinkMark>,
        _title: &str,
        size: Size<Pixels>,
        link_click_handler: Option<Arc<LinkClickHandlerFn>>,
    ) -> AnyElement {
        img(image_source(url))
            .id(ix)
            .object_fit(ObjectFit::Contain)
            .max_w(relative(1.))
            .w(size.width)
            .h(size.height)
            .when_some(link.clone(), |this, link| {
                let aux_link = link.clone();
                let aux_link_click_handler = link_click_handler.clone();
                this.cursor_pointer()
                    .on_click(move |event, window, cx| {
                        crate::TextSelection::end(window, cx);
                        cx.stop_propagation();
                        handle_link_click(
                            &link_click_handler,
                            link.url.clone(),
                            event.clone(),
                            window,
                            cx,
                        );
                    })
                    .on_aux_click(move |event, window, cx| {
                        crate::TextSelection::end(window, cx);
                        cx.stop_propagation();
                        handle_link_click(
                            &aux_link_click_handler,
                            aux_link.url.clone(),
                            event.clone(),
                            window,
                            cx,
                        );
                    })
            })
            .into_any_element()
    }
}

impl IntoElement for InlineFlow {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InlineFlow {
    type RequestLayoutState = InlineFlowLayoutState;
    type PrepaintState = InlineFlowPrepaintState;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let measure_items = self.items.iter().map(MeasureItem::from).collect::<Vec<_>>();
        let line_height = window.line_height();
        let rem_size = window.rem_size();
        let image_sizes = measure_items
            .iter()
            .enumerate()
            .map(|(ix, item)| match item {
                MeasureItem::Image { url, width, height } => Some(measure_image_size(
                    ix,
                    url,
                    *width,
                    *height,
                    line_height,
                    rem_size,
                    window,
                    cx,
                )),
                MeasureItem::Text { .. } | MeasureItem::Custom { .. } => None,
            })
            .collect::<Vec<_>>();
        let layout_state = InlineFlowLayoutState::default();
        let layout_ref = layout_state.layout.clone();

        let layout_id = window.request_measured_layout(Default::default(), {
            move |known_dimensions, available_space, window, _cx| {
                let text_style = window.text_style();
                let wrap_width = if text_style.white_space == WhiteSpace::Normal {
                    known_dimensions.width.or(match available_space.width {
                        AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    })
                } else {
                    None
                };
                let layout = layout_flow(
                    &measure_items,
                    &image_sizes,
                    &text_style,
                    wrap_width,
                    window,
                );
                let size = layout.size;
                if let Ok(mut state) = layout_ref.lock() {
                    *state = Some(layout);
                }
                size
            }
        });

        (layout_id, layout_state)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let fragments = request_layout
            .layout
            .lock()
            .ok()
            .and_then(|layout| layout.as_ref().map(|layout| layout.fragments.clone()))
            .unwrap_or_default();
        let selection_states = self.state.synchronize(&fragments, &self.items);
        let mut prepaint = InlineFlowPrepaintState {
            fragments: Vec::with_capacity(fragments.len()),
        };

        for (fragment_ix, fragment) in fragments.into_iter().enumerate() {
            match fragment {
                PositionedFragment::Text {
                    item_ix,
                    origin,
                    size: fragment_size,
                    source_range,
                    text,
                    links,
                    highlights,
                    ..
                } => {
                    let state = match &self.items[item_ix] {
                        InlineFlowItem::Text {
                            state,
                            text: source,
                            ..
                        } if source_range == (0..source.len()) => state.clone(),
                        _ => Arc::new(Mutex::new(InlineState::default())),
                    };
                    let state = selection_states
                        .get(fragment_ix)
                        .and_then(Clone::clone)
                        .unwrap_or(state);
                    if let Ok(mut state) = state.lock() {
                        state.set_text(text);
                    }

                    let mut element = Inline::new(
                        fragment_ix,
                        state,
                        links,
                        highlights,
                        self.link_click_handler.clone(),
                    )
                    .into_any_element();
                    element.prepaint_as_root(
                        bounds.origin + origin,
                        size(
                            AvailableSpace::Definite(fragment_size.width),
                            AvailableSpace::Definite(fragment_size.height),
                        ),
                        window,
                        cx,
                    );
                    prepaint.fragments.push(PrepaintFragment::Element(element));
                }
                PositionedFragment::Image {
                    item_ix,
                    origin,
                    size: fragment_size,
                } => {
                    let InlineFlowItem::Image {
                        url, link, title, ..
                    } = &self.items[item_ix]
                    else {
                        continue;
                    };
                    let mut element = Self::image_element(
                        fragment_ix,
                        url,
                        link,
                        title.as_str(),
                        fragment_size,
                        self.link_click_handler.clone(),
                    );
                    element.prepaint_as_root(
                        bounds.origin + origin,
                        size(
                            AvailableSpace::Definite(fragment_size.width),
                            AvailableSpace::Definite(fragment_size.height),
                        ),
                        window,
                        cx,
                    );
                    prepaint.fragments.push(PrepaintFragment::Element(element));
                }
                PositionedFragment::Custom {
                    item_ix,
                    origin,
                    size: fragment_size,
                    text,
                    ..
                } => {
                    let InlineFlowItem::Custom { element, link, .. } = &mut self.items[item_ix]
                    else {
                        continue;
                    };
                    let Some(mut element) = element.take() else {
                        continue;
                    };
                    let fragment_bounds = Bounds::new(bounds.origin + origin, fragment_size);
                    element.prepaint_as_root(
                        fragment_bounds.origin,
                        size(
                            AvailableSpace::Definite(fragment_size.width),
                            AvailableSpace::Definite(fragment_size.height),
                        ),
                        window,
                        cx,
                    );
                    let hitbox = window.insert_hitbox(fragment_bounds, HitboxBehavior::Normal);
                    let state = selection_states
                        .get(fragment_ix)
                        .and_then(Clone::clone)
                        .unwrap_or_else(|| Arc::new(Mutex::new(InlineState::default())));
                    prepaint.fragments.push(PrepaintFragment::Atomic {
                        element,
                        state,
                        text,
                        link: link.clone(),
                        bounds: fragment_bounds,
                        hitbox,
                    });
                }
            }
        }

        prepaint
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for fragment in &mut prepaint.fragments {
            match fragment {
                PrepaintFragment::Element(element) => {
                    element.paint(window, cx);
                }
                PrepaintFragment::Atomic {
                    element,
                    state,
                    text,
                    link,
                    bounds,
                    hitbox,
                } => {
                    element.paint(window, cx);
                    paint_atomic_selection(
                        state,
                        text,
                        link,
                        self.link_click_handler.clone(),
                        *bounds,
                        hitbox,
                        window,
                        cx,
                    );
                }
            }
        }
    }
}

fn paint_atomic_selection(
    inline_state: &Arc<Mutex<InlineState>>,
    text: &SharedString,
    link: &Option<LinkMark>,
    link_click_handler: Option<Arc<LinkClickHandlerFn>>,
    bounds: Bounds<Pixels>,
    hitbox: &Hitbox,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(text_view_state) = GlobalState::global(cx).text_view_state().cloned() else {
        return;
    };
    let current_view = window.current_view();
    let text_view = text_view_state.read(cx);
    let is_selectable = text_view.is_selectable();
    let selection = if text_view.is_all_selected() {
        Some(Selection::from(0..text.len()))
    } else if let Some(selection) = text_view.multi_click_selection() {
        bounds
            .contains(&selection.pos)
            .then(|| Selection::from(0..text.len()))
    } else if let Some((selection_start, selection_end)) = text_view.selection_points(cx) {
        point_in_text_selection(
            bounds.origin,
            bounds.size.width,
            selection_start,
            selection_end,
            bounds.size.height,
        )
        .then(|| Selection::from(0..text.len()))
    } else {
        None
    };

    if let Ok(mut state) = inline_state.lock() {
        state.selection = selection;
    }
    if !is_selectable {
        return;
    }

    window.set_cursor_style(gpui::CursorStyle::IBeam, hitbox);
    if link.is_some() {
        window.set_cursor_style(gpui::CursorStyle::PointingHand, hitbox);
    }
    let text_bounds = bounds.intersect(&window.content_mask().bounds);
    if text_bounds.size.width > Pixels::ZERO && text_bounds.size.height > Pixels::ZERO {
        text_view_state.update(cx, |state, _| {
            state.selection_adapter.register_inline(vec![text_bounds]);
        });
    }

    if selection.is_some() {
        window.paint_quad(gpui::quad(
            bounds,
            px(0.),
            cx.theme().tokens.colors.selection,
            gpui::Edges::default(),
            gpui::transparent_black(),
            gpui::BorderStyle::default(),
        ));
    }

    window.on_mouse_event({
        let hitbox = hitbox.clone();
        let inline_state = inline_state.clone();
        let text = text.clone();
        let text_view_state = text_view_state.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !hitbox.is_hovered(window) || event.button != MouseButton::Left {
                return;
            }
            let kind = match event.click_count {
                2 => TextViewMultiClickKind::Word,
                3 => TextViewMultiClickKind::Paragraph,
                _ => return,
            };
            crate::GlobalState::suppress_text_selection(cx);
            if let Ok(mut state) = inline_state.lock() {
                state.selection = Some((0..text.len()).into());
            }
            text_view_state.update(cx, |state, cx| {
                state.set_multi_click_selection(event.position, kind, text.to_string(), cx);
            });
            cx.notify(current_view);
        }
    });

    // Keep link activation on mouse-up, matching the native `Inline` path.
    // This also lets a drag selection win over a click without opening the
    // destination accidentally.
    if let Some(link) = link.clone() {
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let text_view_state = text_view_state.clone();
            let link_click_handler = link_click_handler.clone();
            move |event: &MouseUpEvent, phase, window, cx| {
                if !phase.bubble() || !hitbox.is_hovered(window) {
                    return;
                }
                if text_view_state.read(cx).has_selection(cx) {
                    return;
                }
                crate::TextSelection::end(window, cx);
                cx.stop_propagation();
                let click = ClickEvent::Mouse(MouseClickEvent {
                    down: MouseDownEvent {
                        button: event.button,
                        position: event.position,
                        modifiers: event.modifiers,
                        click_count: event.click_count,
                        first_mouse: false,
                    },
                    up: event.clone(),
                });
                handle_link_click(&link_click_handler, link.url.clone(), click, window, cx);
            }
        });
    }
}

impl From<&InlineFlowItem> for MeasureItem {
    fn from(item: &InlineFlowItem) -> Self {
        match item {
            InlineFlowItem::Text {
                state: _,
                text,
                links,
                highlights,
                ..
            } => MeasureItem::Text {
                text: text.clone(),
                links: links.clone(),
                highlights: highlights.clone(),
            },
            InlineFlowItem::Image {
                url, width, height, ..
            } => MeasureItem::Image {
                url: url.clone(),
                width: *width,
                height: *height,
            },
            InlineFlowItem::Custom { text, metrics, .. } => MeasureItem::Custom {
                text: text.clone(),
                metrics: *metrics,
            },
        }
    }
}

impl MeasureItem {
    fn len(&self) -> usize {
        match self {
            MeasureItem::Text { text, .. } => text.len(),
            MeasureItem::Image { .. } => IMAGE_LEN,
            MeasureItem::Custom { .. } => IMAGE_LEN,
        }
    }
}

fn layout_flow(
    items: &[MeasureItem],
    image_sizes: &[Option<Size<Pixels>>],
    text_style: &TextStyle,
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> InlineFlowLayout {
    let line_height = window.line_height();
    let rem_size = window.rem_size();
    let total_len = items.iter().map(MeasureItem::len).sum::<usize>();
    if total_len == 0 {
        return InlineFlowLayout::default();
    }

    let line_ranges = line_ranges(items, image_sizes, text_style, wrap_width, window);
    let font_size = text_style.font_size.to_pixels(rem_size);
    // Every line starts from the inherited font's baseline so a line made only
    // of atomic items still sits where its surrounding prose would.
    let default_run = text_style.to_run(1);
    let default_shape = shape_line(" ".into(), font_size, &[default_run], window);
    let default_alignment = text_baseline_alignment(&default_shape, line_height);
    let mut fragments = Vec::new();
    let mut max_width = Pixels::ZERO;
    let mut y = Pixels::ZERO;

    for line_range in line_ranges {
        let mut line_fragments = Vec::new();
        let mut line_width = Pixels::ZERO;
        let FragmentAlignment::Baseline {
            ascent: mut line_ascent,
            descent: mut line_descent,
        } = default_alignment
        else {
            unreachable!("text alignment is always baseline-relative");
        };
        let mut max_center_height = Pixels::ZERO;
        let mut item_start = 0;

        for (item_ix, item) in items.iter().enumerate() {
            let item_end = item_start + item.len();
            if item_end <= line_range.start {
                item_start = item_end;
                continue;
            }
            if item_start >= line_range.end {
                break;
            }

            match item {
                MeasureItem::Text {
                    text,
                    links,
                    highlights,
                } => {
                    let local_start = line_range.start.max(item_start) - item_start;
                    let local_end = line_range.end.min(item_end) - item_start;
                    if local_start < local_end {
                        let subtext = SharedString::from(text[local_start..local_end].to_string());
                        let highlights =
                            slice_ranges(highlights, local_start, local_end, |range, style| {
                                (range, *style)
                            });
                        let links = slice_ranges(links, local_start, local_end, |range, link| {
                            (range, link.clone())
                        });
                        let runs = runs_for_highlights(&subtext, text_style, highlights.clone());
                        let shaped_line = shape_line(subtext.clone(), font_size, &runs, window);
                        let width = shaped_line.width();
                        let alignment = text_baseline_alignment(&shaped_line, line_height);
                        let FragmentAlignment::Baseline { ascent, descent } = alignment else {
                            unreachable!("text alignment is always baseline-relative");
                        };
                        line_ascent = line_ascent.max(ascent);
                        line_descent = line_descent.max(descent);
                        line_width += width;
                        line_fragments.push(LineFragmentLayout {
                            item_ix,
                            kind: LineFragmentKind::Text {
                                text: subtext,
                                links,
                                highlights,
                            },
                            size: size(width, ascent + descent),
                            source_range: local_start..local_end,
                            alignment,
                        });
                    }
                }
                MeasureItem::Image { .. } => {
                    if line_range.start <= item_start && item_end <= line_range.end {
                        // Images keep their intrinsic size and stay centered on
                        // the line rather than joining the text baseline.
                        let size = image_sizes
                            .get(item_ix)
                            .copied()
                            .flatten()
                            .unwrap_or_else(|| inline_image_size_for_line(None, line_height));
                        line_width += size.width;
                        max_center_height = max_center_height.max(size.height);
                        line_fragments.push(LineFragmentLayout {
                            item_ix,
                            kind: LineFragmentKind::Image,
                            size,
                            source_range: 0..IMAGE_LEN,
                            alignment: FragmentAlignment::Center,
                        });
                    }
                }
                MeasureItem::Custom { text, metrics } => {
                    if line_range.start <= item_start && item_end <= line_range.end {
                        // Atomic items (formulas) share the text baseline via
                        // the ascent/descent their renderer reported.
                        line_width += metrics.width();
                        line_ascent = line_ascent.max(metrics.ascent());
                        line_descent = line_descent.max(metrics.descent());
                        line_fragments.push(LineFragmentLayout {
                            item_ix,
                            kind: LineFragmentKind::Custom { text: text.clone() },
                            size: metrics.size(),
                            source_range: 0..text.len(),
                            alignment: FragmentAlignment::Baseline {
                                ascent: metrics.ascent(),
                                descent: metrics.descent(),
                            },
                        });
                    }
                }
            }

            item_start = item_end;
        }

        let baseline_height = line_ascent + line_descent;
        let actual_line_height = baseline_height.max(max_center_height);
        let baseline_y = y + (actual_line_height - baseline_height) / 2. + line_ascent;
        let mut x = Pixels::ZERO;
        for fragment in line_fragments {
            let fragment_y = match fragment.alignment {
                FragmentAlignment::Baseline { ascent, .. } => baseline_y - ascent,
                FragmentAlignment::Center => y + (actual_line_height - fragment.size.height) / 2.,
            };
            let origin = point(x, fragment_y);
            let positioned = match fragment.kind {
                LineFragmentKind::Text {
                    text,
                    links,
                    highlights,
                } => PositionedFragment::Text {
                    item_ix: fragment.item_ix,
                    origin,
                    size: fragment.size,
                    source_range: fragment.source_range,
                    text,
                    links,
                    highlights,
                    hard_breaks_before: 0,
                },
                LineFragmentKind::Image => PositionedFragment::Image {
                    item_ix: fragment.item_ix,
                    origin,
                    size: fragment.size,
                },
                LineFragmentKind::Custom { text } => PositionedFragment::Custom {
                    item_ix: fragment.item_ix,
                    origin,
                    size: fragment.size,
                    text,
                    hard_breaks_before: 0,
                },
            };
            x += fragment.size.width;
            fragments.push(positioned);
        }

        max_width = max_width.max(line_width);
        y += actual_line_height;
    }

    InlineFlowLayout {
        fragments,
        size: size(max_width, y),
    }
}

fn line_ranges(
    items: &[MeasureItem],
    image_sizes: &[Option<Size<Pixels>>],
    text_style: &TextStyle,
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> Vec<Range<usize>> {
    let total_len = items.iter().map(MeasureItem::len).sum::<usize>();
    let mut hard_lines = Vec::new();
    let mut line_start = 0;
    let mut item_start = 0;

    for item in items {
        if let MeasureItem::Text { text, .. } = item {
            for (newline, _) in text.match_indices('\n') {
                let newline = item_start + newline;
                hard_lines.push(line_start..newline);
                line_start = newline + 1;
            }
        }
        item_start += item.len();
    }
    hard_lines.push(line_start..total_len);

    let Some(wrap_width) = wrap_width else {
        return hard_lines;
    };
    let rem_size = window.rem_size();
    let font_size = text_style.font_size.to_pixels(rem_size);
    let mut wrapper = window
        .text_system()
        .line_wrapper(text_style.font(), font_size);
    let mut ranges = Vec::new();

    for hard_line in hard_lines {
        let mut item_start = 0;
        let wrap_fragments = items
            .iter()
            .enumerate()
            .filter_map(|(ix, item)| {
                let item_end = item_start + item.len();
                let fragment = if item_end <= hard_line.start || item_start >= hard_line.end {
                    None
                } else {
                    match item {
                        MeasureItem::Text { text, .. } => {
                            let start = hard_line.start.max(item_start) - item_start;
                            let end = hard_line.end.min(item_end) - item_start;
                            (start < end).then(|| WrapLineFragment::text(&text[start..end]))
                        }
                        MeasureItem::Image { .. } => (hard_line.start <= item_start
                            && item_end <= hard_line.end)
                            .then(|| {
                                WrapLineFragment::element(
                                    image_sizes[ix]
                                        .expect("image size should be measured before wrapping")
                                        .width,
                                    IMAGE_LEN,
                                )
                            }),
                        MeasureItem::Custom { metrics, .. } => (hard_line.start <= item_start
                            && item_end <= hard_line.end)
                            .then(|| WrapLineFragment::element(metrics.width(), item.len())),
                    }
                };
                item_start = item_end;
                fragment
            })
            .collect::<Vec<_>>();

        let boundaries = wrapper
            .wrap_line(&wrap_fragments, wrap_width)
            .map(|boundary| hard_line.start + boundary.ix.min(hard_line.len()))
            .collect::<Vec<_>>();
        let mut start = hard_line.start;

        for end in boundaries {
            if start < end {
                ranges.push(start..end);
            }
            start = end;
        }

        if start < hard_line.end || hard_line.is_empty() {
            ranges.push(start..hard_line.end);
        }
    }

    ranges
}

#[allow(clippy::too_many_arguments)]
fn measure_image_size(
    ix: usize,
    url: &SharedUri,
    width: Option<DefiniteLength>,
    height: Option<DefiniteLength>,
    line_height: Pixels,
    rem_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) -> Size<Pixels> {
    let intrinsic_size = if width.is_some() && height.is_some() {
        None
    } else {
        intrinsic_image_size(ix, url, width, height, window, cx)
    };
    image_size(width, height, intrinsic_size, line_height, rem_size)
}

fn intrinsic_image_size(
    ix: usize,
    url: &SharedUri,
    width: Option<DefiniteLength>,
    height: Option<DefiniteLength>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Size<Pixels>> {
    let mut element = img(image_source(url))
        .id(ix)
        .object_fit(ObjectFit::Contain)
        .max_w(relative(1.))
        .when_some(width, |this, width| this.w(width))
        .when_some(height, |this, height| this.h(height))
        .into_any_element();
    let measured_size = element.layout_as_root(AvailableSpace::min_size(), window, cx);

    if measured_size.width <= Pixels::ZERO || measured_size.height <= Pixels::ZERO {
        None
    } else {
        Some(measured_size)
    }
}

fn image_size(
    width: Option<DefiniteLength>,
    height: Option<DefiniteLength>,
    intrinsic_size: Option<Size<Pixels>>,
    line_height: Pixels,
    rem_size: Pixels,
) -> Size<Pixels> {
    let base_size = AbsoluteLength::Pixels(line_height);
    match (width, height) {
        (Some(width), Some(height)) => size(
            width.to_pixels(base_size, rem_size),
            height.to_pixels(base_size, rem_size),
        ),
        (Some(width), None) => {
            let width = width.to_pixels(base_size, rem_size);
            let height = intrinsic_size
                .and_then(|intrinsic_size| {
                    (intrinsic_size.width > Pixels::ZERO && intrinsic_size.height > Pixels::ZERO)
                        .then(|| width * (intrinsic_size.height / intrinsic_size.width))
                })
                .unwrap_or(line_height);
            size(width, height)
        }
        (None, Some(height)) => {
            let height = height.to_pixels(base_size, rem_size);
            let width = intrinsic_size
                .and_then(|intrinsic_size| {
                    (intrinsic_size.width > Pixels::ZERO && intrinsic_size.height > Pixels::ZERO)
                        .then(|| height * (intrinsic_size.width / intrinsic_size.height))
                })
                .unwrap_or(height);
            size(width, height)
        }
        (None, None) => inline_image_size_for_line(intrinsic_size, line_height),
    }
}

fn inline_image_size_for_line(
    intrinsic_size: Option<Size<Pixels>>,
    line_height: Pixels,
) -> Size<Pixels> {
    let height = line_height * 0.75;
    let aspect_ratio = intrinsic_size
        .and_then(|intrinsic_size| {
            (intrinsic_size.width > Pixels::ZERO && intrinsic_size.height > Pixels::ZERO)
                .then(|| intrinsic_size.width / intrinsic_size.height)
        })
        .unwrap_or(1.);

    size((height * aspect_ratio).max(px(1.)), height.max(px(1.)))
}

fn runs_for_highlights(
    text: &str,
    default_style: &TextStyle,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut ix = 0;

    for (range, highlight) in highlights {
        if ix < range.start {
            runs.push(default_style.clone().to_run(range.start - ix));
        }
        runs.push(
            default_style
                .clone()
                .highlight(highlight)
                .to_run(range.len()),
        );
        ix = range.end;
    }

    if ix < text.len() {
        runs.push(default_style.to_run(text.len() - ix));
    }

    runs
}

fn shape_line(
    text: SharedString,
    font_size: Pixels,
    runs: &[TextRun],
    window: &mut Window,
) -> ShapedLine {
    window.text_system().shape_line(text, font_size, runs, None)
}

/// Where a shaped text fragment's baseline sits inside its line box.
///
/// The glyph box is centered in `line_height` (GPUI's usual text placement),
/// so the fragment's ascent includes the leading above the glyphs and its
/// descent the leading below; together they always span the full line box.
fn text_baseline_alignment(shaped_line: &ShapedLine, line_height: Pixels) -> FragmentAlignment {
    let glyph_height = shaped_line.ascent + shaped_line.descent;
    let height = line_height.max(glyph_height);
    let top_padding = (height - glyph_height) / 2.;
    let ascent = top_padding + shaped_line.ascent;
    FragmentAlignment::Baseline {
        ascent,
        descent: height - ascent,
    }
}

fn slice_ranges<T, U>(
    ranges: &[(Range<usize>, T)],
    start: usize,
    end: usize,
    map: impl Fn(Range<usize>, &T) -> U,
) -> Vec<U> {
    ranges
        .iter()
        .filter_map(|(range, value)| {
            let clipped_start = range.start.max(start);
            let clipped_end = range.end.min(end);
            (clipped_start < clipped_end)
                .then(|| map((clipped_start - start)..(clipped_end - start), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, VisualTestContext};

    #[test]
    fn inline_image_without_explicit_size_scales_intrinsic_ratio_to_line_height() {
        let line_height = px(20.);
        let intrinsic_size = size(px(160.), px(40.));

        let measured = inline_image_size_for_line(Some(intrinsic_size), line_height);

        assert_eq!(measured, size(px(60.), px(15.)));
    }

    #[test]
    fn inline_image_without_intrinsic_size_uses_compact_square_fallback() {
        let measured = inline_image_size_for_line(None, px(20.));

        assert_eq!(measured, size(px(15.), px(15.)));
    }

    struct EmptyRoot;

    impl Render for EmptyRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::Empty
        }
    }

    fn text_item(text: &str) -> MeasureItem {
        MeasureItem::Text {
            text: text.into(),
            links: Vec::new(),
            highlights: Vec::new(),
        }
    }

    #[gpui::test]
    fn shaped_mixed_flow_shares_the_text_baseline_and_centers_images(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let (_, cx) = cx.add_window_view(|_, _| EmptyRoot);
        let cx: &mut VisualTestContext = cx;

        cx.update(|window, _| {
            let text_style = window.text_style();
            let line_height = window.line_height();

            // A tall formula raises the line; the text keeps its baseline
            // aligned with the formula's reported baseline.
            let metrics = InlineMetrics::new(px(31.), line_height + px(6.), px(9.));
            let items = vec![
                text_item("before "),
                MeasureItem::Custom {
                    text: "$x^2$".into(),
                    metrics,
                },
                text_item(" after"),
            ];
            let image_sizes = vec![None; items.len()];
            let layout = layout_flow(&items, &image_sizes, &text_style, None, window);

            let mut text_baselines = Vec::new();
            let mut custom_baseline = None;
            for fragment in &layout.fragments {
                match fragment {
                    PositionedFragment::Text { origin, size, .. } => {
                        let default_run = text_style.to_run(1);
                        let shaped = shape_line(
                            " ".into(),
                            text_style.font_size.to_pixels(window.rem_size()),
                            &[default_run],
                            window,
                        );
                        let FragmentAlignment::Baseline { ascent, .. } =
                            text_baseline_alignment(&shaped, line_height)
                        else {
                            unreachable!()
                        };
                        assert_eq!(size.height, line_height);
                        text_baselines.push(origin.y + ascent);
                    }
                    PositionedFragment::Custom { origin, .. } => {
                        custom_baseline = Some(origin.y + metrics.ascent());
                    }
                    PositionedFragment::Image { .. } => unreachable!(),
                }
            }
            let custom_baseline = custom_baseline.expect("custom fragment");
            assert_eq!(text_baselines.len(), 2);
            for baseline in text_baselines {
                assert!(
                    (baseline - custom_baseline).abs() < px(0.01),
                    "text baseline {baseline:?} must match the formula baseline {custom_baseline:?}"
                );
            }
            assert_eq!(layout.size.height, metrics.ascent() + metrics.descent());

            // Images keep the historical centered placement.
            let image_items = vec![
                text_item("before"),
                MeasureItem::Image {
                    url: "https://example.com/image.svg".into(),
                    width: None,
                    height: None,
                },
                text_item("after"),
            ];
            let image_sizes = vec![None, Some(size(px(8.), px(8.))), None];
            let layout = layout_flow(&image_items, &image_sizes, &text_style, None, window);
            let image = layout
                .fragments
                .iter()
                .find_map(|fragment| match fragment {
                    PositionedFragment::Image { origin, size, .. } => Some((*origin, *size)),
                    PositionedFragment::Text { .. } | PositionedFragment::Custom { .. } => None,
                })
                .expect("inline image fragment");
            assert!(
                (image.0.y + image.1.height / 2. - layout.size.height / 2.).abs() < px(0.01),
                "inline images must retain the original centered line alignment"
            );
        });
    }
}
