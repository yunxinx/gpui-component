use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash as _, Hasher as _},
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

#[cfg(test)]
use std::cell::RefCell;

use gpui::{
    App, BorderStyle, Bounds, Corners, CursorStyle, Edges, Element, ElementId, GlobalElementId,
    Font, Half, HighlightStyle, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    SharedString, StyledText, TextAlign, TextLayout, TextRun, WhiteSpace, Window, point, px, quad,
};

use crate::{
    ActiveTheme, WindowExt as _, global_state::GlobalState, input::Selection,
    text::TextViewMultiClickKind, text::node::LinkMark, text::selection::word_range_at,
};

/// A inline element used to render a inline text and support selectable.
///
/// All text in TextView (including the CodeBlock) used this for text rendering.
pub(super) struct Inline {
    id: ElementId,
    text: SharedString,
    links: Rc<Vec<(Range<usize>, LinkMark)>>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    styled_text: StyledText,
    highlight_layout_hash: u64,

    state: Arc<Mutex<InlineState>>,
}

/// Persistent selection state for one continuous styled text element.
///
/// Keep this state with the parsed document and reuse it when rebuilding the
/// element. This preserves selection without splitting a long text into one
/// [`InlineFlow`](super::InlineFlow) per logical line.
#[derive(Clone, Debug, Default)]
pub struct SelectableTextState {
    inner: Arc<Mutex<InlineState>>,
}

#[derive(Clone, Debug, PartialEq)]
struct VisualLineCacheKey {
    width: Pixels,
    line_height: Pixels,
    font: Font,
    font_size: Pixels,
    white_space: WhiteSpace,
    highlight_layout_hash: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct LogicalLineGeometry {
    top: Pixels,
    row_ends: Arc<[Pixels]>,
}

#[derive(Clone, Debug, PartialEq)]
struct VisualLineCache {
    key: VisualLineCacheKey,
    lines: Arc<[LogicalLineGeometry]>,
    line_bottoms: Arc<[Pixels]>,
}

impl SelectableTextState {
    /// Create selection state initialized with the text that will be rendered.
    pub fn new(text: impl Into<SharedString>) -> Self {
        let state = Self::default();
        if let Ok(mut inner) = state.inner.lock() {
            inner.set_text(text.into());
        }
        state
    }

    fn set_rendered_text(&self, text: SharedString) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.text != text {
                inner.text = text;
                inner.selection = None;
            }
        }
    }

    /// Return the selected UTF-8 text, or an empty string when nothing is selected.
    pub fn selected_text(&self) -> String {
        let Ok(inner) = self.inner.lock() else {
            return String::new();
        };
        let Some(selection) = &inner.selection else {
            return String::new();
        };
        let start = selection.start.min(selection.end).min(inner.text.len());
        let end = selection.start.max(selection.end).min(inner.text.len());
        inner.text.get(start..end).unwrap_or_default().to_string()
    }

    /// Clear the current selection synchronously.
    pub fn clear_selection(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.selection = None;
        }
    }
}

/// One continuous selectable text element with optional highlight ranges.
///
/// This uses GPUI's native multiline [`StyledText`] layout. It is intended for
/// long homogeneous text such as source code, logs, and preformatted output;
/// use [`InlineFlow`](super::InlineFlow) when text must be interleaved with
/// atomic images or custom elements.
pub struct SelectableText {
    inline: Inline,
    line_number_gutter: Option<LineNumberGutter>,
}

struct LineNumberGutter {
    right_margin: Pixels,
    color: Hsla,
}

#[cfg(test)]
thread_local! {
    static PAINTED_LINE_NUMBERS: RefCell<Vec<(usize, usize)>> = const {
        RefCell::new(Vec::new())
    };
}

impl SelectableText {
    /// Create a selectable styled text element from persistent state.
    pub fn new(
        id: impl Into<ElementId>,
        state: SelectableTextState,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        Self {
            inline: Inline::new(
                id,
                state.inner,
                Vec::new(),
                highlights.into_iter().collect(),
            ),
            line_number_gutter: None,
        }
    }

    /// Create a selectable styled text element and synchronize its rendered text.
    ///
    /// Use this when the same persistent state can render different text forms,
    /// such as switching between plain source and a decorated projection.
    pub fn with_text(
        id: impl Into<ElementId>,
        state: SelectableTextState,
        text: impl Into<SharedString>,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        state.set_rendered_text(text.into());
        Self::new(id, state, highlights)
    }

    /// Paint logical line numbers in reserved space immediately before the text.
    ///
    /// The caller must reserve `width` in its parent layout. Line numbers are
    /// painted from the source layout, so soft-wrapped continuation rows remain
    /// blank and the selectable text remains the undecorated source.
    pub fn line_number_gutter(mut self, right_margin: Pixels, color: Hsla) -> Self {
        self.line_number_gutter = Some(LineNumberGutter {
            right_margin,
            color,
        });
        self
    }
}

/// Prepaint state for [`SelectableText`].
pub struct SelectableTextPrepaintState {
    inline: InlinePrepaintState,
}

/// The inline text state, used RefCell to keep the selection state.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct InlineState {
    hovered_index: Option<usize>,
    /// The text that actually rendering, matched with selection.
    pub(super) text: SharedString,
    pub(super) selection: Option<Selection>,
    visual_lines: Option<VisualLineCache>,
}

pub(crate) struct InlinePrepaintState {
    hitbox: Hitbox,
    text_hitboxes: Vec<Hitbox>,
    text_bounds: Vec<Bounds<Pixels>>,
    visual_lines: VisualLineCache,
}

impl InlineState {
    /// Save actually rendered text for selected text to use.
    pub(crate) fn set_text(&mut self, text: SharedString) {
        if self.text != text {
            self.visual_lines = None;
        }
        self.text = text;
    }
}

impl Inline {
    pub(super) fn new(
        id: impl Into<ElementId>,
        state: Arc<Mutex<InlineState>>,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> Self {
        let mut hasher = DefaultHasher::new();
        highlights.hash(&mut hasher);
        let highlight_layout_hash = hasher.finish();
        let text = state
            .lock()
            .map(|state| state.text.clone())
            .unwrap_or_default();

        Self {
            id: id.into(),
            links: Rc::new(links),
            highlights,
            text: text.clone(),
            styled_text: StyledText::new(text),
            highlight_layout_hash,
            state,
        }
    }

    /// Get link at given mouse position.
    fn link_for_position(
        layout: &TextLayout,
        links: &Vec<(Range<usize>, LinkMark)>,
        position: Point<Pixels>,
    ) -> Option<LinkMark> {
        let offset = layout.index_for_position(position).ok()?;
        for (range, link) in links.iter() {
            if range.contains(&offset) {
                return Some(link.clone());
            }
        }

        None
    }

    /// Paint selected bounds for debug.
    #[allow(unused)]
    fn paint_selected_bounds(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        window.paint_quad(gpui::PaintQuad {
            bounds,
            background: cx.theme().blue.alpha(0.01).into(),
            corner_radii: Corners::default(),
            border_color: gpui::transparent_black(),
            border_style: BorderStyle::default(),
            border_widths: gpui::Edges::all(px(0.)),
        });
    }

    fn layout_selections(
        &self,
        text_layout: &TextLayout,
        bounds: &Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> (bool, bool, Option<Selection>) {
        let Some(text_view_state) = GlobalState::global(cx).text_view_state() else {
            return (false, false, None);
        };

        let text_view_state = text_view_state.read(cx);
        let is_selectable = text_view_state.is_selectable();
        if !is_selectable {
            return (false, false, None);
        }

        if text_view_state.is_all_selected() {
            return (is_selectable, true, Some((0..self.text.len()).into()));
        }

        if let Some(selection) = text_view_state.multi_click_selection() {
            return (
                is_selectable,
                true,
                selection_for_multi_click(
                    &self.text,
                    text_layout,
                    *bounds,
                    selection.pos,
                    selection.kind,
                )
                .map(Selection::from),
            );
        }

        let Some((selection_start, selection_end)) = text_view_state.selection_points(window, cx)
        else {
            return (is_selectable, false, None);
        };
        let line_height = window.line_height();
        let mask_bounds = window.content_mask().bounds;

        // Use for debug selection bounds
        // self.paint_selected_bounds(Bounds::from_corners(selection_start, selection_end), window, cx);

        let mut selection: Option<Selection> = None;
        let mut offset = 0;
        let mut chars = self.text.chars().peekable();
        while let Some(c) = chars.next() {
            let Some(pos) = text_layout.position_for_index(offset) else {
                offset += c.len_utf8();
                continue;
            };

            let next_offset = offset + c.len_utf8();
            let mut char_width = line_height.half();
            if let Some(next_pos) = text_layout.position_for_index(next_offset) {
                if next_pos.y == pos.y {
                    char_width = next_pos.x - pos.x;
                }
            }

            let char_center = point(pos.x + char_width.half(), pos.y + line_height.half());
            if mask_bounds.contains(&char_center)
                && point_in_text_selection(
                    pos,
                    char_width,
                    selection_start,
                    selection_end,
                    line_height,
                )
            {
                if selection.is_none() {
                    selection = Some((offset..offset).into());
                }

                if let Some(selection) = selection.as_mut() {
                    selection.end = next_offset;
                }
            }

            offset = next_offset;
        }

        (true, true, selection)
    }

    fn visual_lines(
        &self,
        text_layout: &TextLayout,
        window: &Window,
    ) -> VisualLineCache {
        let text_style = window.text_style();
        let key = VisualLineCacheKey {
            width: text_layout.bounds().size.width,
            line_height: text_layout.line_height(),
            font: text_style.font(),
            font_size: text_style.font_size.to_pixels(window.rem_size()),
            white_space: text_style.white_space,
            highlight_layout_hash: self.highlight_layout_hash,
        };

        if let Ok(state) = self.state.lock()
            && let Some(cache) = &state.visual_lines
            && cache.key == key
        {
            return cache.clone();
        }

        let line_height = key.line_height;
        let mut line_top = Pixels::ZERO;
        let mut line_bottoms = Vec::new();
        let lines = text_layout
            .line_layouts()
            .into_iter()
            .map(|line| {
                let row_ends: Arc<[Pixels]> = line
                    .wrap_boundaries()
                    .iter()
                    .map(|boundary| {
                        line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
                            .position
                            .x
                    })
                    .chain([line.unwrapped_layout.width])
                    .collect();
                let geometry = LogicalLineGeometry {
                    top: line_top,
                    row_ends: row_ends.clone(),
                };
                line_top += line_height * row_ends.len();
                line_bottoms.push(line_top);
                geometry
            })
            .collect::<Arc<[_]>>();
        let cache = VisualLineCache {
            key,
            lines,
            line_bottoms: line_bottoms.into(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.visual_lines = Some(cache.clone());
        }
        cache
    }

    fn text_line_bounds(
        &self,
        visual_lines: &VisualLineCache,
        text_bounds: Bounds<Pixels>,
        line_height: Pixels,
        mask_bounds: Bounds<Pixels>,
    ) -> Vec<Bounds<Pixels>> {
        let mut line_bounds = Vec::new();
        let visible_top = (mask_bounds.top() - text_bounds.top()).max(Pixels::ZERO);
        let visible_bottom = mask_bounds.bottom() - text_bounds.top();
        let first_line = visual_lines
            .line_bottoms
            .partition_point(|bottom| *bottom <= visible_top);

        for line in visual_lines.lines.iter().skip(first_line) {
            if line.top >= visible_bottom {
                break;
            }
            let first_row = (((visible_top - line.top).max(Pixels::ZERO)).as_f32()
                / line_height.as_f32())
            .floor() as usize;
            let visible_rows = (((visible_bottom - line.top).max(Pixels::ZERO)).as_f32()
                / line_height.as_f32())
            .ceil() as usize;
            let last_row = visible_rows.min(line.row_ends.len());

            for row_index in first_row.min(last_row)..last_row {
                let row_start_x = if row_index == 0 {
                    Pixels::ZERO
                } else {
                    line.row_ends[row_index - 1]
                };
                let row_end_x = line.row_ends[row_index];
                let row_top = text_bounds.top() + line.top + line_height * row_index;
                let width = (row_end_x - row_start_x).max(line_height.half());
                let bounds = Bounds::from_corners(
                    point(text_bounds.left(), row_top),
                    point(text_bounds.left() + width, row_top + line_height),
                )
                .intersect(&mask_bounds);
                if bounds.size.width > px(0.) && bounds.size.height > px(0.) {
                    line_bounds.push(bounds);
                }
            }
        }

        line_bounds
    }

    /// Paint the selection background.
    fn paint_selection(
        selection: &Selection,
        text_layout: &TextLayout,
        bounds: &Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut start = selection.start;
        let mut end = selection.end;
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }
        let Some(start_position) = text_layout.position_for_index(start) else {
            return;
        };
        let Some(end_position) = text_layout.position_for_index(end) else {
            return;
        };

        let line_height = text_layout.line_height();
        if start_position.y == end_position.y {
            window.paint_quad(quad(
                Bounds::from_corners(
                    start_position,
                    point(end_position.x, end_position.y + line_height),
                ),
                px(0.),
                cx.theme().selection,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));
        } else {
            window.paint_quad(quad(
                Bounds::from_corners(
                    start_position,
                    point(bounds.right(), start_position.y + line_height),
                ),
                px(0.),
                cx.theme().selection,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));

            if end_position.y > start_position.y + line_height {
                window.paint_quad(quad(
                    Bounds::from_corners(
                        point(bounds.left(), start_position.y + line_height),
                        point(bounds.right(), end_position.y),
                    ),
                    px(0.),
                    cx.theme().selection,
                    Edges::default(),
                    gpui::transparent_black(),
                    BorderStyle::default(),
                ));
            }

            window.paint_quad(quad(
                Bounds::from_corners(
                    point(bounds.left(), end_position.y),
                    point(end_position.x, end_position.y + line_height),
                ),
                px(0.),
                cx.theme().selection,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }
}

impl IntoElement for Inline {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = SelectableTextPrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inline.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.inline.source_location()
    }

    fn request_layout(
        &mut self,
        global_element_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.inline
            .request_layout(global_element_id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        SelectableTextPrepaintState(self.inline.prepaint(
            id,
            inspector_id,
            bounds,
            request_layout,
            window,
            cx,
        ))
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inline.paint(
            global_id,
            inspector_id,
            bounds,
            request_layout,
            &mut prepaint.0,
            window,
            cx,
        );
        if let Some(gutter) = &self.line_number_gutter {
            let text_layout = self.inline.styled_text.layout();
            let line_height = text_layout.line_height();
            let mask_bounds = window.content_mask().bounds;
            let text_style = window.text_style();
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            let mut line_top = bounds.top();
            for (index, source_line) in text_layout.line_layouts().iter().enumerate() {
                let visual_row_count = source_line.wrap_boundaries().len() + 1;
                let line_bottom = line_top + line_height * visual_row_count;
                if line_bottom <= mask_bounds.top() {
                    line_top = line_bottom;
                    continue;
                }
                if line_top >= mask_bounds.bottom() {
                    break;
                }
                #[cfg(test)]
                PAINTED_LINE_NUMBERS.with(|painted| {
                    painted.borrow_mut().push((index + 1, visual_row_count));
                });
                let number: SharedString = (index + 1).to_string().into();
                let run = TextRun {
                    len: number.len(),
                    font: text_style.font(),
                    color: gutter.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window
                    .text_system()
                    .shape_line(number, font_size, &[run], None);
                let origin = point(
                    bounds.left() - gutter.right_margin - shaped.width(),
                    line_top,
                );
                let _ = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx);
                line_top = line_bottom;
            }
        }
    }
}

impl Element for Inline {
    type RequestLayoutState = ();
    type PrepaintState = InlinePrepaintState;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_element_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text_style = window.text_style();

        let mut runs = Vec::new();
        let mut ix = 0;
        for (range, highlight) in self.highlights.iter() {
            if ix < range.start {
                runs.push(text_style.clone().to_run(range.start - ix));
            }
            runs.push(text_style.clone().highlight(*highlight).to_run(range.len()));
            ix = range.end;
        }
        if ix < self.text.len() {
            runs.push(text_style.to_run(self.text.len() - ix));
        }

        self.styled_text = StyledText::new(self.text.clone()).with_runs(runs);
        let (layout_id, _) =
            self.styled_text
                .request_layout(global_element_id, inspector_id, window, cx);

        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled_text
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let text_layout = self.styled_text.layout();
        let text_bounds = self.text_line_bounds(
            text_layout,
            text_layout.line_height(),
            window.content_mask().bounds,
        );
        let text_hitboxes = text_bounds
            .iter()
            .map(|bounds| window.insert_hitbox(*bounds, HitboxBehavior::Normal))
            .collect();
        InlinePrepaintState {
            hitbox,
            text_hitboxes,
            text_bounds,
        }
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let current_view = window.current_view();
        let hitbox = &prepaint.hitbox;
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        let text_layout = self.styled_text.layout().clone();
        self.styled_text
            .paint(global_id, None, bounds, &mut (), &mut (), window, cx);

        // layout selections
        let (is_selectable, is_selection, selection) =
            self.layout_selections(&text_layout, &bounds, window, cx);

        state.selection = selection;

        if is_selection || is_selectable {
            for text_hitbox in &prepaint.text_hitboxes {
                window.set_cursor_style(CursorStyle::IBeam, text_hitbox);
            }
        }

        // link cursor pointer
        let mouse_position = window.mouse_position();
        if let Some(_) = Self::link_for_position(&text_layout, &self.links, mouse_position) {
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
        }

        if let Some(selection) = &state.selection {
            Self::paint_selection(selection, &text_layout, &bounds, window, cx);
        }

        if is_selectable {
            if let Some(text_view_state) = GlobalState::global(cx).text_view_state().cloned() {
                crate::Root::register_selectable_text_inline(
                    &text_view_state,
                    prepaint.text_bounds.clone(),
                    window,
                    cx,
                );
            }

            window.on_mouse_event({
                let hitbox = hitbox.clone();
                let text_layout = text_layout.clone();
                let inline_state = self.state.clone();
                let text = self.text.clone();
                let text_view_state = GlobalState::global(cx).text_view_state().cloned();

                move |event: &MouseDownEvent, phase, window, cx| {
                    if !phase.bubble()
                        || !hitbox.is_hovered(window)
                        || event.button != MouseButton::Left
                    {
                        return;
                    }

                    let kind = match event.click_count {
                        2 => TextViewMultiClickKind::Word,
                        3 => TextViewMultiClickKind::Paragraph,
                        _ => return,
                    };

                    let Some(range) = selection_for_multi_click(
                        &text,
                        &text_layout,
                        hitbox.bounds,
                        event.position,
                        kind,
                    ) else {
                        return;
                    };

                    let selected_text = text[range.clone()].to_string();

                    if let Ok(mut inline_state) = inline_state.lock() {
                        inline_state.selection = Some(range.into());
                    }
                    if let Some(text_view_state) = &text_view_state {
                        text_view_state.update(cx, |state, _| {
                            state.set_multi_click_selection(event.position, kind, selected_text);
                        });
                    }
                    cx.notify(current_view);
                }
            });
        }

        // mouse move, update hovered link
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let text_layout = text_layout.clone();
            let mut hovered_index = state.hovered_index;
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() || !hitbox.is_hovered(window) {
                    return;
                }

                let current = hovered_index;
                let updated = text_layout.index_for_position(event.position).ok();
                //  notify update when hovering over different links
                if current != updated {
                    hovered_index = updated;
                    cx.notify(current_view);
                }
            }
        });

        if !is_selection {
            // click to open link
            window.on_mouse_event({
                let links = self.links.clone();
                let text_layout = text_layout.clone();
                let hitbox = hitbox.clone();
                let text_view_state = GlobalState::global(cx).text_view_state().cloned();

                move |event: &MouseUpEvent, phase, window, cx| {
                    if !phase.bubble() || !hitbox.is_hovered(window) {
                        return;
                    }
                    if text_view_state
                        .as_ref()
                        .is_some_and(|state| state.read(cx).has_selection(window, cx))
                    {
                        return;
                    }

                    if let Some(link) =
                        Self::link_for_position(&text_layout, &links, event.position)
                    {
                        window.end_text_selection(cx);
                        cx.stop_propagation();
                        cx.open_url(&link.url);
                    }
                }
            });
        }
    }
}

fn selection_for_multi_click(
    text: &str,
    text_layout: &TextLayout,
    bounds: Bounds<Pixels>,
    pos: Point<Pixels>,
    kind: TextViewMultiClickKind,
) -> Option<std::ops::Range<usize>> {
    if !bounds.contains(&pos) {
        return None;
    }

    let offset = text_layout.index_for_position(pos).ok()?;

    match kind {
        TextViewMultiClickKind::Word => word_range_at(text, offset),
        // Known limitation: a paragraph maps to a single Inline run here. When a
        // paragraph embeds an inline image it is split into multiple Inline runs,
        // so triple-click only selects the run on the clicked side of the image.
        TextViewMultiClickKind::Paragraph => (!text.is_empty()).then_some(0..text.len()),
    }
}

/// Check if a `pos` is within a `bounds`, considering multi-line selections.
pub(super) fn point_in_text_selection(
    pos: Point<Pixels>,
    char_width: Pixels,
    selection_start: Point<Pixels>,
    selection_end: Point<Pixels>,
    line_height: Pixels,
) -> bool {
    let point_in_line = |point: Point<Pixels>| point.y >= pos.y && point.y < pos.y + line_height;
    let top = selection_start.y.min(selection_end.y);
    let bottom = selection_start.y.max(selection_end.y);
    let x = pos.x + char_width.half();

    // Out of the vertical bounds
    if pos.y + line_height <= top || pos.y > bottom {
        return false;
    }

    // Treat the selection as single-line when both drag points fall within the
    // same rendered line, even if their y coordinates differ inside that line.
    if point_in_line(selection_start) && point_in_line(selection_end) {
        let left = selection_start.x.min(selection_end.x);
        let right = selection_start.x.max(selection_end.x);
        return x >= left && x <= right;
    }

    let (top_point, bottom_point) = if selection_start.y < selection_end.y {
        (selection_start, selection_end)
    } else {
        (selection_end, selection_start)
    };
    let is_top_line = point_in_line(top_point);
    let is_bottom_line = point_in_line(bottom_point);

    if is_top_line {
        return x >= top_point.x;
    } else if is_bottom_line {
        return x <= bottom_point.x;
    } else {
        return true;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PAINTED_LINE_NUMBERS, SelectableText, SelectableTextState, point_in_text_selection,
    };
    use crate::{
        ActiveTheme as _, Root,
        text::{MarkdownExtensions, MarkdownNode, TextView, TextViewState},
    };
    use gpui::{
        AppContext as _, Context, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
        ParentElement as _, Render, Styled as _, Window, point, px,
    };

    struct SelectableTextTestRoot {
        body: gpui::Entity<TextViewState>,
        extensions: MarkdownExtensions,
    }

    struct ClippedSelectableTextTestRoot {
        body: gpui::Entity<TextViewState>,
        extensions: MarkdownExtensions,
    }

    impl Render for SelectableTextTestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            TextView::new(&self.body)
                .selectable(true)
                .markdown_extensions(self.extensions.clone())
        }
    }

    impl Render for ClippedSelectableTextTestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div().w(px(160.)).h(px(48.)).overflow_hidden().child(
                TextView::new(&self.body)
                    .selectable(true)
                    .markdown_extensions(self.extensions.clone()),
            )
        }
    }

    #[gpui::test]
    fn selectable_text_preserves_multiline_drag_copy(cx: &mut gpui::TestAppContext) {
        const SOURCE: &str = "first\n\nthird";

        cx.update(crate::init);
        let state = SelectableTextState::new(SOURCE);
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            move |window, cx| {
                let parser_state = state.clone();
                let extensions = MarkdownExtensions::default()
                    .block_parser(move |node, _| {
                        let markdown::mdast::Node::Code(code) = node else {
                            return None;
                        };
                        Some(
                            MarkdownNode::new("selectable-text-test", code.value.clone())
                                .text(code.value.clone())
                                .selectable_text_state(parser_state.clone()),
                        )
                    })
                    .block_renderer("selectable-text-test", move |node, _, _| {
                        let state = node
                            .attached_selectable_text_state()
                            .cloned()
                            .unwrap_or_default();
                        gpui::div()
                            .debug_selector(|| "selectable-text".to_string())
                            .child(SelectableText::new("selectable-text-inner", state, []))
                            .into_any_element()
                    });
                let body = cx.new(|cx| TextViewState::markdown("```text\nfirst\n\nthird\n```", cx));
                let view = cx.new(|_| SelectableTextTestRoot { body, extensions });
                Root::new(view, window, cx)
            }
        });
        let cx: &mut gpui::VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let bounds = cx.debug_bounds("selectable-text").expect("text bounds");
        let line_height = bounds.size.height / 3.;
        let start = point(bounds.left() + px(1.), bounds.top() + line_height / 2.);
        let end = point(bounds.right() - px(1.), bounds.bottom() - line_height / 2.);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());

        assert_eq!(state.selected_text().trim_end_matches('\n'), SOURCE);
    }

    #[gpui::test]
    fn line_number_gutter_numbers_logical_lines_not_soft_wrapped_rows(
        cx: &mut gpui::TestAppContext,
    ) {
        const SOURCE: &str =
            "Unicode first line is deliberately long: 你好，世界，こんにちは，世界\n\n🙂 third";

        cx.update(crate::init);
        let state = SelectableTextState::new(SOURCE);
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            move |window, cx| {
                let parser_state = state.clone();
                let extensions = MarkdownExtensions::default()
                    .block_parser(move |node, _| {
                        let markdown::mdast::Node::Code(code) = node else {
                            return None;
                        };
                        Some(
                            MarkdownNode::new("numbered-selectable-text-test", code.value.clone())
                                .text(code.value.clone())
                                .selectable_text_state(parser_state.clone()),
                        )
                    })
                    .block_renderer("numbered-selectable-text-test", move |node, _, cx| {
                        let state = node
                            .attached_selectable_text_state()
                            .cloned()
                            .unwrap_or_default();
                        gpui::div()
                            .w(px(120.))
                            .whitespace_normal()
                            .child(
                                SelectableText::new("numbered-selectable-text-inner", state, [])
                                    .line_number_gutter(px(12.), cx.theme().muted_foreground),
                            )
                            .into_any_element()
                    });
                let markdown = format!("```text\n{SOURCE}\n```");
                let body = cx.new(|cx| TextViewState::markdown(&markdown, cx));
                let view = cx.new(|_| SelectableTextTestRoot { body, extensions });
                Root::new(view, window, cx)
            }
        });
        let cx: &mut gpui::VisualTestContext = cx;

        cx.run_until_parked();
        PAINTED_LINE_NUMBERS.with(|painted| painted.borrow_mut().clear());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let painted = PAINTED_LINE_NUMBERS.with(|painted| painted.borrow().clone());
        assert_eq!(painted.len(), 3, "one number per logical source line");
        assert_eq!(
            painted
                .iter()
                .map(|(number, _)| *number)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(painted[0].1 > 1, "the long Unicode line must soft-wrap");
        assert_eq!(painted[1].1, 1, "an empty logical line still owns one row");
    }

    #[gpui::test]
    fn line_number_gutter_shapes_only_lines_inside_the_content_mask(cx: &mut gpui::TestAppContext) {
        const LINE_COUNT: usize = 200;

        cx.update(crate::init);
        let source = (1..=LINE_COUNT)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let state = SelectableTextState::new(source.clone());
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            move |window, cx| {
                let parser_state = state.clone();
                let extensions = MarkdownExtensions::default()
                    .block_parser(move |node, _| {
                        let markdown::mdast::Node::Code(code) = node else {
                            return None;
                        };
                        Some(
                            MarkdownNode::new("clipped-numbered-text-test", code.value.clone())
                                .text(code.value.clone())
                                .selectable_text_state(parser_state.clone()),
                        )
                    })
                    .block_renderer("clipped-numbered-text-test", move |node, _, cx| {
                        let state = node
                            .attached_selectable_text_state()
                            .cloned()
                            .unwrap_or_default();
                        gpui::div()
                            .whitespace_nowrap()
                            .child(
                                SelectableText::new("clipped-numbered-text-inner", state, [])
                                    .line_number_gutter(px(12.), cx.theme().muted_foreground),
                            )
                            .into_any_element()
                    });
                let markdown = format!("```text\n{source}\n```");
                let body = cx.new(|cx| TextViewState::markdown(&markdown, cx));
                let view = cx.new(|_| ClippedSelectableTextTestRoot { body, extensions });
                Root::new(view, window, cx)
            }
        });
        let cx: &mut gpui::VisualTestContext = cx;

        cx.run_until_parked();
        PAINTED_LINE_NUMBERS.with(|painted| painted.borrow_mut().clear());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let painted = PAINTED_LINE_NUMBERS.with(|painted| painted.borrow().clone());
        assert!(!painted.is_empty(), "the visible gutter must still paint");
        assert!(
            painted.len() < LINE_COUNT / 10,
            "a 48px content mask must not shape all {LINE_COUNT} line numbers: {painted:?}"
        );
        assert_eq!(painted[0].0, 1, "the viewport starts at the first line");
    }

    #[test]
    fn test_point_in_text_selection() {
        let line_height = px(20.);
        let char_width = px(10.);
        let start = point(px(50.), px(50.));
        let end = point(px(150.), px(150.));

        // First line but haft line height, true
        // | p --------|
        // | selection |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(50.), px(40.)),
            char_width,
            start,
            end,
            line_height
        ));

        // First line in selection, true
        // | p --------|
        // | selection |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(50.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        // First line, but left out of selection, false
        // p |-----------|
        //   | selection |
        //   |-----------|
        assert!(!point_in_text_selection(
            point(px(40.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        // First line but right out of selection, true
        // |-----------| p
        // | selection |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(160.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));

        // Middle line in selection, true
        // |-----------|
        // |     p     |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(100.), px(70.)),
            char_width,
            start,
            end,
            line_height
        ));
        // Middle line, but left out of selection, true
        //   |-----------|
        // p | selection |
        //   |-----------|
        assert!(point_in_text_selection(
            point(px(40.), px(70.)),
            char_width,
            start,
            end,
            line_height
        ));
        // Middle line, but right out of selection, true
        // |-----------|
        // | selection | p
        // |-----------|
        assert!(point_in_text_selection(
            point(px(160.), px(70.)),
            char_width,
            start,
            end,
            line_height
        ));

        // Last line in selection, true
        // |-----------|
        // | selection |
        // |------- p -|
        assert!(point_in_text_selection(
            point(px(100.), px(140.)),
            char_width,
            start,
            end,
            line_height
        ));
        // Last line, but left out of selection, true
        //
        //   |-----------|
        //   | selection |
        // p |-----------|
        assert!(point_in_text_selection(
            point(px(40.), px(140.)),
            char_width,
            start,
            end,
            line_height
        ));
        // Last line, but right out of selection, false
        // |-----------|
        // | selection |
        // |-----------| p
        assert!(!point_in_text_selection(
            point(px(160.), px(140.)),
            char_width,
            start,
            end,
            line_height
        ));

        // Out of vertical bounds (top), false
        //       p
        // |-----------|
        // | selection |
        // |-----------|
        assert!(!point_in_text_selection(
            point(px(100.), px(20.)),
            char_width,
            start,
            end,
            line_height
        ));
        // Out of vertical bounds (bottom), false
        // |-----------|
        // | selection |
        // |-----------|
        //       p
        assert!(!point_in_text_selection(
            point(px(100.), px(160.)),
            char_width,
            start,
            end,
            line_height
        ));
    }

    #[test]
    fn test_point_in_text_selection_reversed_drag_direction() {
        let line_height = px(20.);
        let char_width = px(10.);

        // Mouse down on lower line then drag upward to x=150.
        // Top line should follow current mouse x, bottom line should keep anchor x.
        let start = point(px(80.), px(150.));
        let end = point(px(150.), px(50.));

        // On top line, selection starts from top cursor x (150), so x=140 should be excluded.
        assert!(!point_in_text_selection(
            point(px(140.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        assert!(point_in_text_selection(
            point(px(150.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));

        // On bottom line, selection ends at anchor x (80), so x=90 should be excluded.
        assert!(point_in_text_selection(
            point(px(75.), px(140.)),
            char_width,
            start,
            end,
            line_height
        ));
        assert!(!point_in_text_selection(
            point(px(80.), px(140.)),
            char_width,
            start,
            end,
            line_height
        ));
    }

    #[test]
    fn test_point_in_text_selection_same_visual_line_with_different_y() {
        let line_height = px(20.);
        let char_width = px(10.);
        let start = point(px(100.), px(55.));
        let end = point(px(60.), px(58.));

        assert!(!point_in_text_selection(
            point(px(40.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        assert!(point_in_text_selection(
            point(px(70.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        assert!(!point_in_text_selection(
            point(px(110.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
    }

    #[test]
    fn test_point_in_text_selection_same_visual_line_with_reversed_y() {
        let line_height = px(20.);
        let char_width = px(10.);
        let start = point(px(60.), px(58.));
        let end = point(px(100.), px(55.));

        assert!(!point_in_text_selection(
            point(px(40.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        assert!(point_in_text_selection(
            point(px(70.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
        assert!(!point_in_text_selection(
            point(px(110.), px(50.)),
            char_width,
            start,
            end,
            line_height
        ));
    }
}
