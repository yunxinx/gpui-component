use futures::Stream as _;
use std::{
    ops::RangeInclusive,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

use gpui::{
    App, AppContext as _, Bounds, Context, FocusHandle, IntoElement, KeyBinding, ListState,
    ParentElement as _, Pixels, Point, Render, SharedString, Styled as _, Task, Window,
    prelude::FluentBuilder as _, px,
};

use crate::{
    AutoScroll, ElementExt, TextSelection,
    async_util::{Receiver, Sender, unbounded},
    input::{self, SelectAll},
    text::{
        CodeBlockActionsFn, CodeBlockHighlighterFn, LinkClickHandlerFn, MarkdownExtensions,
        TableActionsFn, TextViewStyle,
        block_heights::{
            BlockHeightCache, DEFAULT_ESTIMATED_BLOCK_HEIGHT, WindowedLayout, width_bucket,
        },
        document::ParsedDocument,
        format,
        node::{self, NodeContext},
        selection_adapter::TextViewSelectionAdapter,
    },
    v_flex,
};

const CONTEXT: &'static str = "TextView";
// Keep coalescing bounded so sustained streams still render intermediate updates.
const MAX_COALESCED_UPDATES_PER_PARSE: usize = 64;
// Preserve exact first-layout height for small documents while bounding the
// amount of source parsed synchronously on the UI thread.
const MAX_SYNC_FULL_REPLACE_BYTES: usize = 4 * 1024;

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", input::Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", input::Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", input::SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", input::SelectAll, Some(CONTEXT)),
    ]);
}

/// The content format of the text view.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TextViewFormat {
    /// Plain-text view
    Plain,
    /// Markdown view
    Markdown,
    /// HTML view
    Html,
}

/// The format of the text returned by
/// [`TextViewState::selected_text`], which is also what copy writes to the
/// clipboard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionFormat {
    /// The rendered text, without any markup.
    #[default]
    Plain,
    /// The source of the selection.
    ///
    /// Select-all returns the original source verbatim, a partial selection is
    /// reconstructed as Markdown from the parsed nodes (e.g. selecting inside
    /// a `**bold**` run yields `**bold**`).
    Source,
}

/// One text element's laid-out vertical extent, reported by `Inline` during
/// prepaint so `TextView` can snap its `max_lines` clip to a whole-line
/// boundary.
#[derive(Clone, Copy)]
pub(super) struct LineSpan {
    pub(super) top: Pixels,
    pub(super) bottom: Pixels,
    pub(super) line_height: Pixels,
}

/// The state of a TextView.
pub struct TextViewState {
    pub(super) focus_handle: FocusHandle,
    pub(super) list_state: ListState,

    /// The bounds of the text view
    bounds: Bounds<Pixels>,

    pub(super) selectable: bool,
    pub(super) selection_format: SelectionFormat,
    pub(super) scrollable: bool,
    pub(super) max_lines: Option<usize>,
    /// Windowed block layout: only the blocks intersecting the window viewport
    /// (plus overdraw) lay out and paint; the rest are fixed-height spacers.
    /// The document keeps its natural height. Requires `!scrollable` and no
    /// `max_lines` clamp, both of which need the full document laid out.
    pub(super) windowed: bool,
    /// Bumped whenever the view's [`TextViewStyle`] changes. Heights measured
    /// under an older revision describe a layout that no longer holds.
    pub(super) typography_revision: u64,
    /// Per-block heights backing [`Self::windowed`], aligned with the parsed
    /// document's blocks.
    pub(super) block_heights: BlockHeightCache,
    /// Line spans reported by `Inline` during prepaint (collected only while
    /// [`Self::max_lines`] is set); cleared by `TextView` at each frame start.
    pub(super) line_spans: Arc<Mutex<Vec<LineSpan>>>,
    /// Whether the last painted frame clipped content due to `max_lines`.
    pub(super) clamped: bool,
    pub(super) text_view_style: TextViewStyle,
    pub(super) code_block_actions: Option<std::sync::Arc<CodeBlockActionsFn>>,
    pub(super) code_block_highlighter: Option<std::sync::Arc<CodeBlockHighlighterFn>>,
    pub(super) table_actions: Option<std::sync::Arc<TableActionsFn>>,
    pub(super) link_click_handler: Option<std::sync::Arc<LinkClickHandlerFn>>,
    pub(super) markdown_extensions: Arc<MarkdownExtensions>,

    pub(super) is_selecting: bool,
    multi_click_selection: Option<TextViewMultiClickSelection>,
    selected_text_override: Option<String>,
    select_all: bool,
    pub(super) auto_scroll: AutoScroll,
    pub(super) selection_adapter: TextViewSelectionAdapter,

    pub(super) parsed_content: ParsedContent,
    /// Content format (markdown / html), used for bounded synchronous parsing
    /// of small full-replace updates.
    format: TextViewFormat,
    text: String,
    revision: usize,
    pub(super) selection_revision: usize,
    compatible_layout_update: bool,
    /// Height hint for blocks appended before they are measured, captured
    /// from the last render (the worker result arrives without a window).
    estimated_block_height: Option<Pixels>,
    parsed_error: Option<SharedString>,
    tx: Sender<UpdateOptions>,
    _parse_task: Task<()>,
    _receive_task: Task<()>,
}

impl TextViewState {
    /// Create a plain-text TextViewState.
    pub fn plain(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Plain, text, cx)
    }

    /// Create a Markdown TextViewState.
    pub fn markdown(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Markdown, text, cx)
    }

    /// Create a Markdown state whose first parse and block measurement are
    /// deferred until the owning view has installed its extensions.
    ///
    /// This is a narrow compatibility entry point for streaming consumers that
    /// register custom Markdown nodes immediately after creating the state.
    pub fn markdown_with_lazy_scroll_measurement(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new_with_options(TextViewFormat::Markdown, text, false, cx)
    }

    /// Create a HTML TextViewState.
    pub fn html(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Html, text, cx)
    }

    /// Create a new TextViewState.
    fn new(format: TextViewFormat, text: &str, cx: &mut Context<Self>) -> Self {
        Self::new_with_options(format, text, true, cx)
    }

    fn new_with_options(
        format: TextViewFormat,
        text: &str,
        eager_scroll_measurement: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let selection_adapter = TextViewSelectionAdapter::new(cx.entity().downgrade(), cx);
        // Plain text is copied verbatim: its leading/trailing whitespace and
        // final newline are content, not rendering artifacts.
        selection_adapter.set_preserve_copy_boundaries(format == TextViewFormat::Plain, cx);

        let (tx, rx) = unbounded::<UpdateOptions>();
        let (tx_result, rx_result) = unbounded::<ParsedUpdate>();
        let _receive_task = cx.spawn({
            async move |weak_self, cx| {
                while let Ok(parsed_update) = rx_result.recv().await {
                    _ = weak_self.update(cx, |state, cx| {
                        if parsed_update.revision != state.revision {
                            return;
                        }
                        if parsed_update.baseline_ack {
                            debug_assert!(parsed_update.full_parse);
                            return;
                        }

                        match parsed_update.result {
                            Ok(content) => {
                                // Splice the windowed height cache before the
                                // document is swapped in: the common prefix is
                                // computed from the blocks this update left
                                // untouched, so measured heights survive a
                                // streaming append.
                                let windowed_prefix = (parsed_update.selection_compatible
                                    && state.windowed)
                                    .then(|| {
                                        state
                                            .parsed_content
                                            .document
                                            .blocks
                                            .iter()
                                            .zip(content.document.blocks.iter())
                                            .take_while(|(old, new)| old.span() == new.span())
                                            .count()
                                    });
                                let new_count = content.document.blocks.len();
                                if parsed_update.selection_compatible {
                                    let old_count = state.list_state.item_count();
                                    let new_list_count = content.document.blocks.len();
                                    if new_list_count > old_count {
                                        // Appended blocks keep a bounded height
                                        // estimate until measured so the
                                        // scrollbar keeps tracking the stream.
                                        match state.estimated_block_height {
                                            Some(height) => {
                                                state.list_state.splice_with_uniform_height(
                                                    old_count..old_count,
                                                    new_list_count - old_count,
                                                    height,
                                                )
                                            }
                                            None => state.list_state.splice(
                                                old_count..old_count,
                                                new_list_count - old_count,
                                            ),
                                        }
                                    } else if new_list_count < old_count {
                                        state.list_state.splice(new_list_count..old_count, 0);
                                    }
                                }
                                state.parsed_content = content;
                                state.parsed_error = None;
                                state.compatible_layout_update = parsed_update.selection_compatible;
                                if state.windowed {
                                    let estimated = state
                                        .estimated_block_height
                                        .unwrap_or(DEFAULT_ESTIMATED_BLOCK_HEIGHT);
                                    match windowed_prefix {
                                        Some(prefix) => {
                                            state.block_heights.splice(prefix, new_count, estimated)
                                        }
                                        None => state.block_heights.reset(new_count, estimated),
                                    }
                                }
                                if parsed_update.selection_compatible && state.scrollable {
                                    // Appends can change the height of the
                                    // retained block at the viewport boundary.
                                    // Ask ListState to remeasure with its
                                    // absolute scroll anchor so a manually
                                    // chosen offset is not rebased to the new
                                    // content tail.
                                    let count = state.list_state.item_count();
                                    state.list_state.remeasure_items(0..count);
                                }
                            }
                            Err(_)
                                if parsed_update.selection_compatible
                                    && state.parsed_error.is_none() =>
                            {
                                // A streamed append can be transiently invalid
                                // (an unclosed extension construct, a source
                                // preparer that refuses a half-written token).
                                // Keep the last successful document visible; the
                                // worker retains the authoritative source and
                                // reparses it in full on the next append.
                                return;
                            }
                            Err(err) => {
                                state.parsed_error = Some(err);
                            }
                        }
                        // Don't interrupt an active drag-selection; the stored
                        // positions remain valid for append-only updates and will
                        // self-correct on the next mouse-move event.
                        if !parsed_update.selection_compatible && !state.is_selecting {
                            state.reset_selection_and_adapter(cx);
                        }
                        cx.notify();
                    });
                }
            }
        });

        let _parse_task = cx.background_spawn(UpdateFuture::new(format, rx, tx_result));

        let mut this = Self {
            focus_handle,
            bounds: Bounds::default(),
            multi_click_selection: None,
            selected_text_override: None,
            select_all: false,
            selectable: false,
            selection_format: SelectionFormat::default(),
            scrollable: false,
            max_lines: None,
            windowed: false,
            typography_revision: 0,
            block_heights: BlockHeightCache::default(),
            line_spans: Arc::default(),
            clamped: false,
            // Measure all blocks (not just visible ones) so the scrollbar
            // thumb size stays stable. Without this, off-screen blocks count
            // as zero height until scrolled into view, which makes the
            // scrollbar jitter as more blocks get measured during scrolling.
            list_state: if eager_scroll_measurement {
                ListState::new(0, gpui::ListAlignment::Top, px(1000.)).measure_all()
            } else {
                ListState::new(0, gpui::ListAlignment::Top, px(1000.))
            },
            text_view_style: TextViewStyle::default(),
            code_block_actions: None,
            code_block_highlighter: None,
            table_actions: None,
            link_click_handler: None,
            markdown_extensions: Arc::default(),
            is_selecting: false,
            auto_scroll: AutoScroll::default(),
            selection_adapter,
            parsed_content: Default::default(),
            format,
            parsed_error: None,
            text: text.to_string(),
            revision: 0,
            selection_revision: 0,
            compatible_layout_update: false,
            estimated_block_height: None,
            tx,
            _parse_task,
            _receive_task,
        };
        if eager_scroll_measurement {
            this.increment_update(&text, false, cx);
        } else {
            this.queue_initial_parse(&text);
        }
        this
    }

    /// Get the text content.
    pub(crate) fn source(&self) -> SharedString {
        self.parsed_content.document.source.clone()
    }

    /// The number of top-level blocks in the parsed document.
    ///
    /// Lets a consumer gate expensive layout modes (e.g. windowed rendering)
    /// on structure as well as byte size: many small blocks cost more to lay
    /// out than one block of the same total bytes.
    pub fn block_count(&self) -> usize {
        self.parsed_content.document.blocks.len()
    }

    /// Set whether the text is selectable, default false.
    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    /// Set whether the text is selectable, default false.
    pub fn set_selectable(&mut self, selectable: bool, cx: &mut Context<Self>) {
        self.selectable = selectable;
        cx.notify();
    }

    /// Set the [`SelectionFormat`], default is [`SelectionFormat::Plain`].
    pub fn selection_format(mut self, selection_format: SelectionFormat) -> Self {
        self.selection_format = selection_format;
        self
    }

    /// Set the [`SelectionFormat`], default is [`SelectionFormat::Plain`].
    pub fn set_selection_format(
        &mut self,
        selection_format: SelectionFormat,
        cx: &mut Context<Self>,
    ) {
        self.selection_format = selection_format;
        cx.notify();
    }

    /// Set whether the text view scrolls internally, default false.
    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    /// Set whether the text view scrolls internally, default false.
    pub fn set_scrollable(&mut self, scrollable: bool, cx: &mut Context<Self>) {
        if !scrollable {
            self.reset_selection_and_adapter(cx);
        }
        self.scrollable = scrollable;
        cx.notify();
    }

    /// Whether the last painted frame clipped content because of
    /// [`TextView::max_lines`](crate::text::TextView::max_lines).
    pub fn is_clamped(&self) -> bool {
        self.clamped
    }

    /// Set the text content.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text.as_str() == text {
            return;
        }

        self.text.clear();
        self.text.push_str(text);
        self.parsed_error = None;
        self.increment_update(text, false, cx);
    }

    /// Append partial text content to the existing text.
    pub fn push_str(&mut self, new_text: &str, cx: &mut Context<Self>) {
        if new_text.is_empty() {
            return;
        }
        self.text.push_str(new_text);
        self.increment_update(new_text, true, cx);
    }

    pub(crate) fn set_markdown_extensions(
        &mut self,
        markdown_extensions: Arc<MarkdownExtensions>,
        cx: &mut Context<Self>,
    ) {
        if self.markdown_extensions.revision() == markdown_extensions.revision() {
            return;
        }

        let parser_configuration_changed = !self
            .markdown_extensions
            .has_same_parser_configuration(&markdown_extensions);
        self.markdown_extensions = markdown_extensions;
        if parser_configuration_changed && self.format == TextViewFormat::Markdown {
            let text = self.text.clone();
            self.increment_update(&text, false, cx);
        }
    }

    /// The parse diagnostic as it should be shown to the user, if parsing
    /// failed.
    ///
    /// The parser (which may run on a background task) keeps the raw,
    /// locale-independent diagnostic; the extensions' `parse_error_formatter`
    /// resolves the presentation at render time so it can consult the current
    /// locale. Without a formatter the raw diagnostic is shown.
    pub fn display_error(&self) -> Option<SharedString> {
        self.parsed_error
            .as_ref()
            .map(|error| self.markdown_extensions.format_parse_error(error))
    }

    /// Return the selected text, in the view's [`SelectionFormat`].
    pub fn selected_text(&self) -> String {
        self.selected_text_in(None)
    }

    /// The format to copy in, which is [`SelectionFormat::Plain`] whenever the
    /// requested one cannot be produced.
    ///
    /// Only a Markdown view can return source. Reconstructing HTML would mean
    /// spelling every attribute back out — a mark's color, an image's
    /// dimensions, a cell's alignment — with a new way to lose one at each
    /// step, and html5ever records no source offsets to fall back on (it
    /// reports only line numbers), so there is no original text to copy from
    /// either.
    fn effective_format(&self) -> SelectionFormat {
        match self.format {
            TextViewFormat::Markdown => self.selection_format,
            TextViewFormat::Plain | TextViewFormat::Html => SelectionFormat::Plain,
        }
    }

    /// Return the selected text, with `blocks` bounding which top-level blocks
    /// the selection covers.
    ///
    /// The range comes from the selection endpoints, which know their block
    /// even after it scrolls out of view; see
    /// [`ParsedDocument::selected_text`](crate::text::document::ParsedDocument).
    pub(super) fn selected_text_in(&self, blocks: Option<RangeInclusive<usize>>) -> String {
        let format = self.effective_format();

        if self.select_all {
            // Plain text is its own source: return it verbatim rather than the
            // paragraph rendering, which appends a block-terminating newline.
            if format == SelectionFormat::Source || self.format == TextViewFormat::Plain {
                return self.source().to_string();
            }

            return self.parsed_content.document.text();
        }

        // A multi-click stores the plain text it selected, which is a shortcut
        // past the block walk. Source mode cannot take it: the word it stored
        // has lost its markup. The click also set the inline selection it came
        // from, so the walk reconstructs the same range with the markup intact.
        if format != SelectionFormat::Source
            && let Some(text) = &self.selected_text_override
        {
            return text.clone();
        }

        self.parsed_content.document.selected_text(format, blocks)
    }

    fn increment_update(&mut self, text: &str, append: bool, cx: &mut Context<Self>) {
        self.revision += 1;
        if !append {
            self.selection_revision = self.selection_revision.wrapping_add(1);
        }
        let parse_synchronously = !append && text.len() <= MAX_SYNC_FULL_REPLACE_BYTES;
        let update_options = UpdateOptions {
            revision: self.revision,
            append,
            mode: if append {
                ParseMode::Compatible
            } else if parse_synchronously {
                ParseMode::BaselineAck
            } else {
                ParseMode::Replace
            },
            pending_text: text.to_string(),
            markdown_extensions: self.markdown_extensions.clone(),
        };

        // Keep small full replacements synchronous so their first layout has
        // the exact content height. Larger replacements use the existing
        // background parser, bounding synchronous parser input on the UI thread.
        if parse_synchronously {
            match parse_content(self.format, ParsedContent::default(), &update_options) {
                Ok(content) => {
                    if self.windowed {
                        // A full replacement orphans every measurement.
                        let estimated = self
                            .estimated_block_height
                            .unwrap_or(DEFAULT_ESTIMATED_BLOCK_HEIGHT);
                        self.block_heights
                            .reset(content.document.blocks.len(), estimated);
                    }
                    self.parsed_content = content;
                    self.parsed_error = None;
                    if !self.is_selecting {
                        self.reset_selection_and_adapter(cx);
                    }
                }
                Err(err) => {
                    self.parsed_error = Some(err);
                }
            }
            // Keep the background parser's accumulated document in sync so a
            // later append extends this baseline instead of parsing the delta
            // as a standalone document.
            _ = self.tx.try_send(update_options);
            cx.notify();
            return;
        }

        _ = self.tx.try_send(update_options);
    }

    fn queue_initial_parse(&mut self, text: &str) {
        self.revision += 1;
        self.selection_revision = self.selection_revision.wrapping_add(1);
        _ = self.tx.try_send(UpdateOptions {
            revision: self.revision,
            append: false,
            mode: ParseMode::Replace,
            pending_text: text.to_string(),
            markdown_extensions: self.markdown_extensions.clone(),
        });
    }

    /// Save bounds and unselect if bounds changed.
    pub(super) fn update_bounds(&mut self, bounds: Bounds<Pixels>, _cx: &mut App) {
        self.bounds = bounds;
    }

    /// The index of the top-level block at `content_y`, in this view's content
    /// coordinates (the same space the base selection endpoint stores its point in).
    ///
    /// Only laid-out blocks can be located, which is enough for a selection
    /// endpoint: the user can only put one where they can see it. Returns
    /// `None` for a view that is neither virtualized nor windowed, where every
    /// block paints and the range is not needed.
    pub(super) fn block_ix_at(&self, content_y: Pixels) -> Option<usize> {
        if self.scrollable {
            let origin = self.bounds.origin.y + self.scroll_offset().y;
            let count = self.list_state.item_count();
            let mut ix = self.list_state.logical_scroll_top().item_ix;
            while ix < count {
                let bounds = self.list_state.bounds_for_item(ix)?;
                if content_y < bounds.bottom() - origin {
                    return Some(ix);
                }
                ix += 1;
            }

            return count.checked_sub(1);
        }

        // The windowed document starts at the element top (no inner scroll),
        // so the content y maps straight onto the height cache. The prefix
        // sums let an endpoint resolve its block even when that block has
        // never painted, which is what lets `selected_text` cover it whole.
        if self.windowed {
            return self.block_heights.block_ix_at_y(content_y);
        }

        None
    }

    #[doc(hidden)]
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }

    #[doc(hidden)]
    pub fn list_state(&self) -> &ListState {
        &self.list_state
    }

    #[doc(hidden)]
    pub fn is_selecting(&self) -> bool {
        self.is_selecting
    }

    #[doc(hidden)]
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Whether this view has a view-local selection (select-all, multi-click, or override),
    /// independent of the window-level selection.
    pub(super) fn has_view_selection(&self) -> bool {
        self.select_all
            || self.multi_click_selection.is_some()
            || self.selected_text_override.is_some()
    }

    pub(super) fn stop_auto_scroll(&mut self) {
        self.auto_scroll.stop();
    }

    pub(super) fn reset_selection(&mut self) {
        self.multi_click_selection = None;
        self.selected_text_override = None;
        self.select_all = false;
        self.is_selecting = false;
        self.auto_scroll.stop();
        // Clear the inline selection state synchronously, so offscreen
        // (virtualized) views that won't repaint don't leak stale selection
        // text into a new cross-view copy.
        self.parsed_content.document.clear_selection();
    }

    fn reset_selection_and_adapter(&mut self, cx: &mut App) {
        self.reset_selection();
        self.selection_adapter.set_local_selection(false, cx);
    }

    /// Clear the current text selection.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.reset_selection_and_adapter(cx);
        cx.notify();
    }

    pub(super) fn scroll_offset(&self) -> Point<Pixels> {
        if self.scrollable {
            self.list_state.scroll_px_offset_for_scrollbar()
        } else {
            Point::default()
        }
    }

    /// Return the retained scroll state used by scrollable text views.
    ///
    /// Consumers can coordinate an enclosing scrollbar or follow-tail behavior
    /// without replacing the document's state or introducing a second list.
    pub fn scroll_state(&self) -> ListState {
        self.list_state.clone()
    }

    /// Select all rendered text in this view.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.multi_click_selection = None;
        self.selected_text_override = None;
        self.select_all = true;
        self.is_selecting = false;
        self.auto_scroll.stop();
        self.selection_adapter.set_local_selection(true, cx);
        cx.notify();
    }

    pub(crate) fn set_multi_click_selection(
        &mut self,
        pos: Point<Pixels>,
        kind: TextViewMultiClickKind,
        selected_text: String,
        cx: &mut App,
    ) {
        let scroll_offset = self.scroll_offset();
        let pos = pos - self.bounds.origin - scroll_offset;
        self.multi_click_selection = Some(TextViewMultiClickSelection { pos, kind });
        self.selected_text_override = Some(selected_text);
        self.select_all = false;
        self.is_selecting = false;
        self.auto_scroll.stop();
        self.selection_adapter.set_local_selection(true, cx);
    }

    pub(super) fn set_auto_scroll(&mut self, delta: Option<Pixels>, cx: &mut Context<Self>) {
        self.auto_scroll.set(delta, cx, |delta, state, cx| {
            state.list_state.scroll_by(delta);
            cx.notify();
        });
    }

    /// Return the window selection (anchor, cursor) in window coordinates if
    /// this view participates in it.
    ///
    /// Single-view fast path: when both endpoints are anchored inside one
    /// TextView, only that view participates (identical to the previous
    /// per-view behavior).
    pub(crate) fn selection_points(&self, cx: &App) -> Option<(Point<Pixels>, Point<Pixels>)> {
        if !self.selectable {
            return None;
        }
        self.selection_adapter.selection_points(cx)
    }

    pub(crate) fn has_selection(&self, cx: &App) -> bool {
        self.has_view_selection() || self.selection_points(cx).is_some()
    }

    pub(super) fn on_action_select_all(
        &mut self,
        _: &SelectAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selectable {
            cx.propagate();
            return;
        }

        self.select_all(cx);
    }

    pub(crate) fn is_selectable(&self) -> bool {
        self.selectable
    }

    pub(crate) fn is_all_selected(&self) -> bool {
        self.select_all
    }

    pub(crate) fn multi_click_selection(&self) -> Option<TextViewMultiClickSelection> {
        let scroll_offset = self.scroll_offset();
        self.multi_click_selection.map(|selection| {
            let pos = selection.pos + scroll_offset + self.bounds.origin;
            TextViewMultiClickSelection { pos, ..selection }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextViewMultiClickSelection {
    pub(crate) pos: Point<Pixels>,
    pub(crate) kind: TextViewMultiClickKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextViewMultiClickKind {
    Word,
    Paragraph,
}

impl Render for TextViewState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.estimated_block_height = Some(super::document::estimated_block_height(
            &self.text_view_style,
            window,
        ));
        let state = cx.entity();
        let document = self.parsed_content.document.clone();
        let mut node_cx = self.parsed_content.node_cx.clone();

        node_cx.code_block_actions = self.code_block_actions.clone();
        node_cx.code_block_highlighter = self.code_block_highlighter.clone();
        node_cx.table_actions = self.table_actions.clone();
        node_cx.link_click_handler = self.link_click_handler.clone();
        node_cx.markdown_extensions = self.markdown_extensions.clone();
        node_cx.style = self.text_view_style.clone();
        let parsed_error = self.display_error();

        let windowed = self.windowed && !self.scrollable;
        let layout = WindowedLayout {
            element_bounds: self.bounds,
            viewport_size: window.viewport_size(),
        };
        if windowed {
            // Keep the cache aligned even when no parsed update ran (a late
            // `windowed` enable), and re-derive the measurement inputs: a
            // resized or restyled view must drop heights taken under the old
            // layout.
            let estimate = self
                .estimated_block_height
                .unwrap_or(DEFAULT_ESTIMATED_BLOCK_HEIGHT);
            self.block_heights.align(document.blocks.len(), estimate);
            self.block_heights.invalidate(
                width_bucket(layout.element_bounds.size.width),
                self.typography_revision,
            );
        }
        let measure_state = state.downgrade();

        v_flex()
            .w_full()
            // Clamped content must keep its natural height: stretching it to
            // the capped box would hide the overflow the clamp has to measure.
            .when(self.max_lines.is_none(), |this| this.h_full())
            .map(|this| match parsed_error {
                None => this.child(match windowed {
                    false => document
                        .render_root(
                            if self.scrollable {
                                Some(self.list_state.clone())
                            } else {
                                None
                            },
                            &node_cx,
                            window,
                            cx,
                        )
                        .into_any_element(),
                    true => document
                        .render_windowed(
                            &self.block_heights,
                            layout,
                            move |ix, height, cx| {
                                if let Some(state) = measure_state.upgrade() {
                                    state.update(cx, |state, cx| {
                                        if state.block_heights.measure(ix, height) {
                                            cx.notify();
                                        }
                                    });
                                }
                            },
                            &node_cx,
                            window,
                            cx,
                        )
                        .into_any_element(),
                }),
                Some(err) => this.child(
                    v_flex()
                        .gap_1()
                        .child("Failed to parse content")
                        .child(err.to_string()),
                ),
            })
            .on_prepaint(move |bounds, window, cx| {
                let (
                    size_changed,
                    selection_involves_view,
                    has_selection_snapshot,
                    is_selecting,
                    compatible_layout_update,
                ) = {
                    let state = state.read(cx);
                    (
                        state.bounds().size != bounds.size,
                        state.selection_adapter.is_part_of_window_selection(cx),
                        state.selection_adapter.has_selection_snapshot(cx),
                        state.is_selecting,
                        state.compatible_layout_update,
                    )
                };
                let mut revision_changed = false;
                state.update(cx, |state, cx| {
                    revision_changed = state
                        .selection_adapter
                        .update_layout_revision(state.selection_revision, state.is_selecting);
                    state.update_bounds(bounds, cx);
                    state.compatible_layout_update = false;
                });
                if !is_selecting
                    && ((size_changed && selection_involves_view && !compatible_layout_update)
                        || (revision_changed && has_selection_snapshot))
                {
                    TextSelection::clear(window, cx);
                }
            })
    }
}

#[derive(Clone, PartialEq, Default)]
pub(crate) struct ParsedContent {
    pub(crate) document: ParsedDocument,
    pub(crate) node_cx: node::NodeContext,
}

struct UpdateFuture {
    format: TextViewFormat,
    content: ParsedContent,
    /// Authoritative source consumed by this worker, retained independently
    /// from the last successfully parsed document so an append can recover by
    /// reparsing the full source after an earlier parse error.
    source: String,
    /// Whether `content` was successfully parsed from the current source and
    /// Markdown extensions. Source equality alone is insufficient because an
    /// extension-only replacement can fail while keeping the same raw text.
    content_is_current: bool,
    rx: Pin<Box<Receiver<UpdateOptions>>>,
    tx_result: Sender<ParsedUpdate>,
}

impl UpdateFuture {
    fn new(
        format: TextViewFormat,
        rx: Receiver<UpdateOptions>,
        tx_result: Sender<ParsedUpdate>,
    ) -> Self {
        Self {
            format,
            content: Default::default(),
            source: String::new(),
            content_is_current: true,
            rx: Box::pin(rx),
            tx_result,
        }
    }

    fn parse(&mut self, options: &UpdateOptions) -> Result<ParsedContent, SharedString> {
        let previous_source = self.source.clone();
        if options.append {
            self.source.push_str(&options.pending_text);
        } else {
            self.source.clear();
            self.source.push_str(&options.pending_text);
        }

        // Incremental parsing is valid only when the last successfully parsed
        // document represents the exact source preceding this append. If a
        // replacement failed to parse, keep its raw source and recover with a
        // full parse once a later append makes the document valid.
        let can_append = options.append
            && self.content_is_current
            && self.content.document.source.as_ref() == previous_source;
        let effective_options = if can_append {
            options.clone()
        } else {
            UpdateOptions {
                revision: options.revision,
                pending_text: self.source.clone(),
                append: false,
                mode: options.mode,
                markdown_extensions: options.markdown_extensions.clone(),
            }
        };

        parse_content(self.format, self.content.clone(), &effective_options)
    }
}

impl Future for UpdateFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        loop {
            match self.rx.as_mut().poll_next(cx) {
                Poll::Ready(Some(mut options)) => {
                    let hit_coalesce_budget =
                        merge_pending_options(&mut options, self.rx.as_ref().get_ref());

                    let res = self.parse(&options);
                    if let Ok(content) = &res {
                        self.content = content.clone();
                        self.content_is_current = true;
                    } else {
                        self.content_is_current = false;
                    }
                    _ = self.tx_result.try_send(ParsedUpdate {
                        revision: options.revision,
                        full_parse: !options.append,
                        selection_compatible: options.mode == ParseMode::Compatible,
                        baseline_ack: options.mode == ParseMode::BaselineAck,
                        result: res,
                    });
                    if hit_coalesce_budget {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Clone)]
struct UpdateOptions {
    revision: usize,
    pending_text: String,
    append: bool,
    mode: ParseMode,
    markdown_extensions: Arc<MarkdownExtensions>,
}

impl UpdateOptions {
    fn merge(&mut self, next: UpdateOptions) {
        if next.append {
            self.pending_text.push_str(&next.pending_text);
            self.revision = next.revision;
            if self.mode != ParseMode::Replace {
                self.mode = ParseMode::Compatible;
            }
        } else {
            *self = next;
        }
    }
}

struct ParsedUpdate {
    revision: usize,
    full_parse: bool,
    selection_compatible: bool,
    baseline_ack: bool,
    result: Result<ParsedContent, SharedString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParseMode {
    BaselineAck,
    Replace,
    Compatible,
}

fn merge_pending_options(options: &mut UpdateOptions, rx: &Receiver<UpdateOptions>) -> bool {
    let mut update_count = 1;

    while update_count < MAX_COALESCED_UPDATES_PER_PARSE {
        match rx.try_recv() {
            Ok(next_options) => {
                options.merge(next_options);
                update_count += 1;
            }
            Err(_) => return false,
        }
    }

    true
}

fn parse_full_replacement(
    format: TextViewFormat,
    source: String,
    options: &UpdateOptions,
) -> Result<ParsedContent, SharedString> {
    parse_content(
        format,
        ParsedContent::default(),
        &UpdateOptions {
            revision: options.revision,
            pending_text: source,
            append: false,
            mode: options.mode,
            markdown_extensions: options.markdown_extensions.clone(),
        },
    )
}

fn parse_appended_full_replacement(
    format: TextViewFormat,
    content: &ParsedContent,
    options: &UpdateOptions,
) -> Result<ParsedContent, SharedString> {
    let mut full_source =
        String::with_capacity(content.document.source.len() + options.pending_text.len());
    full_source.push_str(&content.document.source);
    full_source.push_str(&options.pending_text);
    parse_full_replacement(format, full_source, options)
}

fn parse_content(
    format: TextViewFormat,
    mut content: ParsedContent,
    options: &UpdateOptions,
) -> Result<ParsedContent, SharedString> {
    // Incremental parses only receive the trailing block, so the definitions
    // earlier blocks contributed are carried forward as metadata and replayed
    // in front of the fragment. The resulting context is retained with the
    // parsed document for render-time resolution of reference links, reference
    // images, and custom inline nodes.
    let previous_reference_identifiers = options
        .append
        .then(|| {
            content
                .node_cx
                .link_refs
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let previous_footnote_identifiers = options
        .append
        .then(|| content.node_cx.footnote_definition_identifiers())
        .unwrap_or_default();
    let link_refs = if options.append {
        std::mem::take(&mut content.node_cx.link_refs)
    } else {
        Default::default()
    };
    let link_ref_source_identifiers = if options.append {
        std::mem::take(&mut content.node_cx.link_ref_source_identifiers)
    } else {
        Default::default()
    };
    let mut node_cx = NodeContext {
        link_refs,
        link_ref_source_identifiers,
        markdown_extensions: options.markdown_extensions.clone(),
        ..NodeContext::default()
    };
    if options.append {
        node_cx.take_definition_metadata_from(&mut content.node_cx);
    }

    // Re-parse the last block together with the appended text, so a block the
    // new text continues (an unclosed list, a fenced code block) is not split
    // in two. A block without a span cannot be located in `source` — the HTML
    // parser never records spans — so it is left in place and only the
    // appended text is parsed, positioned at the end of the current source.
    let mut source = String::new();
    if options.append {
        let last_span = content
            .document
            .blocks
            .last()
            .and_then(|block| block.span());
        if let Some(span) = last_span {
            Arc::make_mut(&mut content.document.blocks).pop();
            // The last block is reparsed below. Remove any definitions it
            // contributed so invalidated or renamed definitions cannot linger.
            if !node_cx.retain_definitions_before(span.start) {
                return parse_appended_full_replacement(format, &content, options);
            }
            node_cx.offset = span.start;
            source.push_str(&content.document.source[span.start..]);
        } else {
            node_cx.offset = content.document.source.len();
        }
    }
    source.push_str(&options.pending_text);

    // A rare definition may have no equivalent label that can be replayed
    // outside its original container. Preserve correctness by parsing the real
    // full source instead of constructing an incomplete reference prefix.
    if options.append && node_cx.link_ref_source_identifiers.len() != node_cx.link_refs.len() {
        return parse_appended_full_replacement(format, &content, options);
    }

    // markdown-rs resolves links and footnotes while building the AST, so the
    // fragment parser must also know definitions retained in earlier blocks.
    let retained_reference_identifiers = options
        .append
        .then(|| node_cx.reference_replay_identifiers())
        .unwrap_or_default();
    let retained_footnote_identifiers = if options.append {
        let Some(identifiers) = node_cx.footnote_replay_identifiers() else {
            return parse_appended_full_replacement(format, &content, options);
        };
        identifiers
    } else {
        Vec::new()
    };

    let new_document = match format {
        TextViewFormat::Plain => format::plain::parse(&source, &mut node_cx),
        TextViewFormat::Markdown
            if retained_reference_identifiers.is_empty()
                && retained_footnote_identifiers.is_empty() =>
        {
            format::markdown::parse(&source, &mut node_cx)
        }
        TextViewFormat::Markdown => {
            match format::markdown::parse_with_retained_definitions(
                &source,
                &retained_reference_identifiers,
                &retained_footnote_identifiers,
                &mut node_cx,
            ) {
                Ok(document) => Ok(document),
                // A source preparer may legally rewrite the synthetic
                // definitions into a shape that cannot be reconstructed or
                // may merge their identifiers. Reparse the authoritative full
                // source instead of rejecting an otherwise valid append.
                Err(_) if options.append => {
                    return parse_appended_full_replacement(format, &content, options);
                }
                Err(error) => Err(error),
            }
        }
        TextViewFormat::Html => format::html::parse(&source, &mut node_cx),
    }?;

    if options.append {
        content.document.source =
            format!("{}{}", content.document.source, options.pending_text).into();
        Arc::make_mut(&mut content.document.blocks)
            .extend(Arc::unwrap_or_clone(new_document.blocks));
    } else {
        content.document = new_document;
    }

    let reference_identifiers_changed = previous_reference_identifiers.len()
        != node_cx.link_refs.len()
        || previous_reference_identifiers
            .iter()
            .any(|identifier| !node_cx.link_refs.contains_key(identifier));
    let footnote_identifiers_changed =
        previous_footnote_identifiers != node_cx.footnote_definition_identifiers();
    // Retained blocks may contain literal references or resolved reference
    // nodes depending on the global definition sets. Reparse the whole
    // document only when either set changes; URL/title-only edits keep state.
    if options.append && (reference_identifiers_changed || footnote_identifiers_changed) {
        return parse_full_replacement(format, content.document.source.to_string(), options);
    }
    content.node_cx = node_cx;

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::MarkdownNode;
    use gpui::TestAppContext;

    #[gpui::test]
    fn small_full_replace_parses_before_background_executor_runs(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let markdown = "# ready";
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown(markdown, cx)));

        state.read_with(cx, |state, _| {
            assert_eq!(state.source().as_str(), markdown);
            assert_eq!(state.parsed_content.document.blocks.len(), 1);
        });
    }

    #[gpui::test]
    fn large_markdown_and_html_full_replacements_wait_for_background_executor(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let markdown = "# x\n\n".repeat(MAX_SYNC_FULL_REPLACE_BYTES / 5 + 1);
        let html = format!("<p>{}</p>", "x".repeat(MAX_SYNC_FULL_REPLACE_BYTES + 1));
        assert!(markdown.len() > MAX_SYNC_FULL_REPLACE_BYTES);
        assert!(html.len() > MAX_SYNC_FULL_REPLACE_BYTES);

        let (markdown_state, html_state) = cx.update(|cx| {
            (
                cx.new(|cx| TextViewState::markdown(&markdown, cx)),
                cx.new(|cx| TextViewState::html(&html, cx)),
            )
        });

        markdown_state.read_with(cx, |state, _| {
            assert_eq!(state.text.as_str(), markdown.as_str());
            assert!(state.source().as_str().is_empty());
            assert!(state.parsed_content.document.blocks.is_empty());
        });
        html_state.read_with(cx, |state, _| {
            assert_eq!(state.text.as_str(), html.as_str());
            assert!(state.source().as_str().is_empty());
            assert!(state.parsed_content.document.blocks.is_empty());
        });

        cx.run_until_parked();

        markdown_state.read_with(cx, |state, _| {
            assert_eq!(state.source().as_str(), markdown.as_str());
            assert!(!state.parsed_content.document.blocks.is_empty());
        });
        html_state.read_with(cx, |state, _| {
            assert_eq!(state.source().as_str(), html.as_str());
            assert!(!state.parsed_content.document.blocks.is_empty());
        });
    }

    #[gpui::test]
    fn async_full_replace_then_push_str_preserves_complete_source(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("old", cx)));
        cx.run_until_parked();

        let replacement = "x".repeat(MAX_SYNC_FULL_REPLACE_BYTES + 1);
        let expected = format!("{replacement} tail");
        state.update(cx, |state, cx| {
            state.set_text(&replacement, cx);
            state.push_str(" tail", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.text.as_str(), expected.as_str());
            assert_eq!(state.source().as_str(), expected.as_str());
        });
    }

    #[gpui::test]
    fn html_push_str_keeps_earlier_blocks(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::html("<p>first</p>", cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.push_str("<p>second</p>", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.source().as_str(), "<p>first</p><p>second</p>");
            let text = state
                .parsed_content
                .document
                .blocks
                .iter()
                .map(|block| block.text())
                .collect::<String>();
            assert!(text.contains("first"), "lost the first block: {text:?}");
            assert!(text.contains("second"), "lost the appended block: {text:?}");
        });
    }

    #[gpui::test]
    fn set_text_then_push_str_appends_to_replaced_content(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("old", cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.set_text("", cx);
            state.push_str("new", cx);
            state.push_str(" text", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.text.as_str(), "new text");
            assert_eq!(state.source().as_str(), "new text");
        });

        state.update(cx, |state, cx| {
            state.set_text("", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.text.as_str(), "");
            assert_eq!(state.source().as_str(), "");
        });
    }

    #[gpui::test]
    fn full_parse_coalesced_with_append_preserves_new_select_all(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("old", cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.set_text("new", cx);
            state.push_str(" text", cx);
            state.select_all(cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert!(state.select_all);
            assert_eq!(state.selected_text().trim(), "new text");
        });
    }

    #[test]
    fn update_options_merge_keeps_latest_full_text() {
        let mut options = UpdateOptions {
            revision: 1,
            pending_text: "old".to_string(),
            append: true,
            mode: ParseMode::Compatible,
            markdown_extensions: Arc::default(),
        };

        options.merge(UpdateOptions {
            revision: 2,
            pending_text: "new".to_string(),
            append: false,
            mode: ParseMode::BaselineAck,
            markdown_extensions: Arc::default(),
        });
        options.merge(UpdateOptions {
            revision: 3,
            pending_text: " text".to_string(),
            append: true,
            mode: ParseMode::Compatible,
            markdown_extensions: Arc::default(),
        });

        assert_eq!(options.revision, 3);
        assert_eq!(options.pending_text, "new text");
        assert!(!options.append);
    }

    #[test]
    fn append_merged_into_async_replace_remains_a_replacement() {
        let mut options = UpdateOptions {
            revision: 1,
            pending_text: "new".to_string(),
            append: false,
            mode: ParseMode::Replace,
            markdown_extensions: Arc::default(),
        };

        options.merge(UpdateOptions {
            revision: 2,
            pending_text: " text".to_string(),
            append: true,
            mode: ParseMode::Compatible,
            markdown_extensions: Arc::default(),
        });

        assert_eq!(options.revision, 2);
        assert_eq!(options.pending_text, "new text");
        assert!(!options.append);
        assert_eq!(options.mode, ParseMode::Replace);
    }

    #[test]
    fn update_future_yields_before_coalescing_all_queued_updates() {
        let (tx, rx) = unbounded::<UpdateOptions>();
        let (tx_result, rx_result) = unbounded::<ParsedUpdate>();
        let total_updates = 128;

        for revision in 1..=total_updates {
            tx.try_send(UpdateOptions {
                revision,
                pending_text: format!("{revision}\n"),
                append: revision != 1,
                mode: if revision == 1 {
                    ParseMode::BaselineAck
                } else {
                    ParseMode::Compatible
                },
                markdown_extensions: Arc::default(),
            })
            .unwrap();
        }

        let mut future = Box::pin(UpdateFuture::new(TextViewFormat::Markdown, rx, tx_result));
        let waker = futures::task::noop_waker();
        let mut task_cx = std::task::Context::from_waker(&waker);

        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut task_cx),
            Poll::Pending
        ));
        let parsed_update = rx_result.try_recv().expect("parse result");

        assert!(
            parsed_update.revision < total_updates,
            "single poll coalesced every queued update through revision {}",
            parsed_update.revision
        );

        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut task_cx),
            Poll::Pending
        ));
        let parsed_update = rx_result.try_recv().expect("next parse result");
        assert_eq!(parsed_update.revision, total_updates);
    }

    #[gpui::test]
    fn select_all_returns_rendered_text(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("**quick** value", cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.select_all(cx);
        });

        state.read_with(cx, |state, _| {
            assert!(state.has_view_selection());
            assert_eq!(state.selected_text().trim(), "quick value");
        });

        state.update(cx, |state, cx| {
            state.clear_selection(cx);
        });

        state.read_with(cx, |state, _| {
            assert!(!state.has_view_selection());
            assert_eq!(state.selected_text(), "");
        });
    }

    #[gpui::test]
    fn select_all_in_source_format_returns_source(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let markdown = "**quick** value";
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown(markdown, cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| state.select_all(cx));

        // The default (plain) mode strips the markup.
        state.read_with(cx, |state, _| {
            assert_eq!(state.selected_text().trim(), "quick value");
        });

        state.update(cx, |state, cx| {
            state.set_selection_format(SelectionFormat::Source, cx)
        });

        // Source mode yields the whole source verbatim.
        state.read_with(cx, |state, _| {
            assert_eq!(state.selected_text().trim(), markdown);
        });
    }

    #[gpui::test]
    fn set_markdown_extensions_reparses_existing_text(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("$TSLA.US", cx)));
        cx.run_until_parked();

        let extensions = MarkdownExtensions::default().block_parser(|node, cx| {
            let markdown::mdast::Node::Paragraph(paragraph) = node else {
                return None;
            };
            let [markdown::mdast::Node::Text(text)] = paragraph.children.as_slice() else {
                return None;
            };
            let symbol = text.value.strip_prefix('$')?.to_string();
            let node_text = format!("${symbol}");

            Some(
                MarkdownNode::new("ticker", symbol)
                    .text(node_text)
                    .markdown(cx.node_source(node).unwrap_or_default()),
            )
        });

        state.update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(extensions), cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            let node::BlockNode::Custom(node) = &state.parsed_content.document.blocks[0] else {
                panic!("expected custom markdown node");
            };
            assert_eq!(node.name(), "ticker");
            assert_eq!(node.data::<String>().map(String::as_str), Some("TSLA.US"));
        });
    }

    fn first_paragraph_has_custom_inline(state: &TextViewState) -> bool {
        let node::BlockNode::Paragraph(paragraph) = &state.parsed_content.document.blocks[0] else {
            panic!("expected a paragraph");
        };
        paragraph
            .children
            .iter()
            .any(|child| child.custom.is_some())
    }

    #[gpui::test]
    fn set_markdown_extensions_reparses_when_only_the_inline_parser_shape_changes(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("see $TSLA$", cx)));
        cx.run_until_parked();

        // A block renderer that is never used keeps the block-parser shape
        // identical between both extension sets; only the inline parser
        // differs, which must still count as a parser change.
        let without_inline = MarkdownExtensions::default()
            .block_renderer("unused", |_, _, _| gpui::Empty.into_any_element());
        state.update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(without_inline), cx);
        });
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert!(!first_paragraph_has_custom_inline(state))
        });

        let with_inline = MarkdownExtensions::default()
            .block_renderer("unused", |_, _, _| gpui::Empty.into_any_element())
            .inline_parser(|node, cx| {
                let markdown::mdast::Node::Text(text) = node else {
                    return None;
                };
                let symbol = text.value.trim().strip_prefix("see $")?.strip_suffix('$')?;
                Some(
                    MarkdownNode::new("ticker", symbol.to_string())
                        .text(text.value.clone())
                        .markdown(cx.node_source(node).unwrap_or_default()),
                )
            });
        state.update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(with_inline), cx);
        });
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert!(
                first_paragraph_has_custom_inline(state),
                "adding an inline parser must reparse the existing document"
            );
        });
    }

    #[test]
    fn parser_configuration_tracks_every_parse_affecting_field() {
        let strict = MarkdownExtensions::default();
        for changed in [
            MarkdownExtensions::default().cjk_emphasis_compatibility(),
            MarkdownExtensions::default().parse_options(|_| {}),
            MarkdownExtensions::default().prepare_source(|source| source.to_string()),
            MarkdownExtensions::default().inline_parser(|_, _| None),
            MarkdownExtensions::default().inline_renderer("x", |_, _, _, _| None),
        ] {
            assert!(
                !strict.has_same_parser_configuration(&changed),
                "a parse-affecting field changed but the shape compared equal"
            );
        }

        // Rebuilding equivalent closures keeps the shape; so does a formatter,
        // which only runs while rendering.
        let renderers_a = MarkdownExtensions::default()
            .block_renderer("ticker", |_, _, _| gpui::Empty.into_any_element());
        let renderers_b = MarkdownExtensions::default()
            .block_renderer("ticker", |_, _, _| gpui::Empty.into_any_element())
            .parse_error_formatter(|error| error.to_string());
        assert_ne!(renderers_a.revision(), renderers_b.revision());
        assert!(renderers_a.has_same_parser_configuration(&renderers_b));
    }

    #[gpui::test]
    fn parse_errors_are_formatted_for_display_but_stored_raw(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let extensions = Arc::new(
            MarkdownExtensions::default()
                .try_prepare_source(|_| Err::<String, _>("preparation-code"))
                .parse_error_formatter(|error| format!("localized: {error}")),
        );
        let state = cx.update(|cx| {
            cx.new(|cx| {
                let mut state = TextViewState::markdown("body", cx);
                state.set_markdown_extensions(extensions.clone(), cx);
                state
            })
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.parsed_error.as_deref(), Some("preparation-code"));
            assert_eq!(
                state.display_error().as_deref(),
                Some("localized: preparation-code")
            );
        });

        // Without a formatter the raw diagnostic is what the user sees.
        let unformatted = Arc::new(
            MarkdownExtensions::default().try_prepare_source(|_| Err::<String, _>("raw-code")),
        );
        let state = cx.update(|cx| {
            cx.new(|cx| {
                let mut state = TextViewState::markdown("body", cx);
                state.set_markdown_extensions(unformatted, cx);
                state
            })
        });
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert_eq!(state.display_error().as_deref(), Some("raw-code"));
        });
    }

    fn parse_markdown(content: ParsedContent, source: &str, append: bool) -> ParsedContent {
        parse_markdown_with_extensions(content, source, append, Arc::default())
    }

    fn parse_markdown_with_extensions(
        content: ParsedContent,
        source: &str,
        append: bool,
        markdown_extensions: Arc<MarkdownExtensions>,
    ) -> ParsedContent {
        parse_content(
            TextViewFormat::Markdown,
            content,
            &UpdateOptions {
                revision: 1,
                pending_text: source.to_string(),
                append,
                markdown_extensions,
                mode: ParseMode::Compatible,
            },
        )
        .expect("test markdown should parse")
    }

    fn paragraph_has_reference(content: &ParsedContent, block_ix: usize, identifier: &str) -> bool {
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[block_ix] else {
            panic!("expected paragraph at block {block_ix}");
        };
        paragraph_contains_reference(paragraph, identifier)
    }

    fn paragraph_contains_reference(paragraph: &node::Paragraph, identifier: &str) -> bool {
        paragraph.children.iter().any(|child| {
            child.marks.iter().any(|(_, mark)| {
                mark.link
                    .as_ref()
                    .and_then(|link| link.identifier.as_deref())
                    == Some(identifier)
            })
        })
    }

    fn paragraph_reference_image_destination(
        content: &ParsedContent,
        block_ix: usize,
    ) -> (String, String) {
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[block_ix] else {
            panic!("expected paragraph at block {block_ix}");
        };
        let inline = paragraph
            .children
            .iter()
            .find(|child| child.image.is_some())
            .expect("expected reference image");
        let image = inline.image.as_ref().expect("checked image above");
        (
            image
                .resolved_url(inline.image_reference_identifier.as_ref(), &content.node_cx)
                .to_string(),
            image.resolved_title(inline.image_reference_identifier.as_ref(), &content.node_cx),
        )
    }

    fn block_contains_reference(block: &node::BlockNode, identifier: &str) -> bool {
        match block {
            node::BlockNode::Paragraph(paragraph) => {
                paragraph_contains_reference(paragraph, identifier)
            }
            node::BlockNode::Root { children, .. }
            | node::BlockNode::Blockquote { children, .. }
            | node::BlockNode::List { children, .. }
            | node::BlockNode::ListItem { children, .. } => children
                .iter()
                .any(|child| block_contains_reference(child, identifier)),
            _ => false,
        }
    }

    fn mdast_contains_link_reference(node: &markdown::mdast::Node) -> bool {
        matches!(node, markdown::mdast::Node::LinkReference(_))
            || node
                .children()
                .is_some_and(|children| children.iter().any(mdast_contains_link_reference))
    }

    fn paragraph_has_footnote_reference(
        content: &ParsedContent,
        block_ix: usize,
        identifier: &str,
    ) -> bool {
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[block_ix] else {
            panic!("expected paragraph at block {block_ix}");
        };
        let expected = format!("[{identifier}]");
        paragraph.children.iter().any(|child| {
            child.text.as_ref() == expected.as_str()
                && child.marks.iter().any(|(_, mark)| mark.italic)
        })
    }

    #[test]
    fn replacement_synchronizes_markdown_reference_definitions() {
        let content = parse_markdown(
            ParsedContent::default(),
            "[value][ref]\n\n[ref]: https://example.com/reference \"Reference title\"",
            false,
        );

        let reference = content
            .node_cx
            .link_refs
            .get("ref")
            .expect("reference definition should remain available for rendering");
        assert_eq!(reference.url.as_ref(), "https://example.com/reference");
        assert_eq!(reference.title.as_deref(), Some("Reference title"));

        let content = parse_markdown(content, "[ref]: https://example.com/replaced", false);
        assert_eq!(
            content.node_cx.link_refs["ref"].url.as_ref(),
            "https://example.com/replaced"
        );

        let content = parse_markdown(content, "plain text without a definition", false);
        assert!(content.node_cx.link_refs.is_empty());

        let content = parse_markdown(
            content,
            "[ref]: https://example.com/first\n[ref]: https://example.com/ignored\n\n[value][ref]",
            false,
        );
        assert_eq!(
            content.node_cx.link_refs["ref"].url.as_ref(),
            "https://example.com/first",
            "Markdown keeps the first duplicate definition"
        );
    }

    #[test]
    fn append_synchronizes_markdown_reference_definitions() {
        let content = parse_markdown(
            ParsedContent::default(),
            "[value][late]\n\ntrailing block",
            false,
        );
        assert!(!paragraph_has_reference(&content, 0, "late"));
        let content = parse_markdown(content, "\n\n[late]: https://example.com/late", true);
        assert!(
            paragraph_has_reference(&content, 0, "late"),
            "adding a definition must reparse references in retained blocks"
        );

        let content = parse_markdown(
            ParsedContent::default(),
            "> [ref]: https://example.com/original\n\n[value][ref]",
            false,
        );
        assert!(paragraph_has_reference(&content, 1, "ref"));
        let content = parse_markdown(content, " tail", true);
        assert_eq!(
            content.node_cx.link_refs["ref"].url.as_ref(),
            "https://example.com/original"
        );
        assert!(
            paragraph_has_reference(&content, 1, "ref"),
            "reparsing a reference must retain definitions from earlier blocks"
        );

        let content = parse_markdown(
            ParsedContent::default(),
            "> [a\\]b]: https://example.com/escaped\n\n[value][a\\]b]",
            false,
        );
        let escaped_identifier = content
            .node_cx
            .link_refs
            .keys()
            .next()
            .expect("escaped reference definition should parse")
            .clone();
        assert!(paragraph_has_reference(&content, 1, &escaped_identifier));
        let content = parse_markdown(content, " tail", true);
        assert!(
            paragraph_has_reference(&content, 1, &escaped_identifier),
            "synthetic definitions must preserve normalized escaped identifiers"
        );

        let content = parse_markdown(content, "\n\n[other]: https://example.com/appended", true);
        assert_eq!(
            content.node_cx.link_refs["other"].url.as_ref(),
            "https://example.com/appended"
        );

        let content = parse_markdown(
            ParsedContent::default(),
            "[value][stable]\n\ntrailing block\n\n[stable]: https://example.com/old",
            false,
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained reference paragraph");
        };
        let retained_state = paragraph.state.clone();
        let content = parse_markdown(content, "er", true);
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained reference paragraph after URL append");
        };
        assert!(
            Arc::ptr_eq(&retained_state, &paragraph.state),
            "changing only a definition URL should keep retained paragraph state"
        );
        assert_eq!(
            content.node_cx.link_refs["stable"].url.as_ref(),
            "https://example.com/older"
        );

        let content = parse_markdown(
            ParsedContent::default(),
            "![value][stable]\n\ntrailing block\n\n[stable]: https://example.com/old",
            false,
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained image-reference paragraph");
        };
        let retained_state = paragraph.state.clone();
        let content = parse_markdown(content, "er", true);
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained image-reference paragraph after definition edit");
        };
        assert!(Arc::ptr_eq(&retained_state, &paragraph.state));
        assert_eq!(
            paragraph_reference_image_destination(&content, 0),
            ("https://example.com/older".into(), "value".into())
        );
        let content = parse_markdown(content, " \"updated title\"", true);
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained image-reference paragraph after title append");
        };
        assert!(Arc::ptr_eq(&retained_state, &paragraph.state));
        assert_eq!(
            paragraph_reference_image_destination(&content, 0),
            ("https://example.com/older".into(), "updated title".into())
        );

        let content = parse_markdown(
            ParsedContent::default(),
            "[value][ref]\n\ntrailing block\n\n> [ref]: https://example.com/original",
            false,
        );
        assert!(paragraph_has_reference(&content, 0, "ref"));
        let content = parse_markdown(content, " \"unterminated title", true);
        assert!(
            !content.node_cx.link_refs.contains_key("ref"),
            "a reference that no longer has a valid definition must not retain its old URL"
        );
        assert!(
            !paragraph_has_reference(&content, 0, "ref"),
            "invalidating a definition must reparse references in retained blocks"
        );
    }

    #[test]
    fn nested_reference_images_track_appended_definition_updates() {
        let source = concat!(
            "**![bold][stable]** _![emphasis][stable]_ ",
            "[![linked][stable]](/outer)\n\n",
            "trailing block\n\n",
            "[stable]: https://example.com/old"
        );
        let content = parse_markdown(ParsedContent::default(), source, false);
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained image-reference paragraph");
        };
        let retained_state = paragraph.state.clone();
        assert_eq!(
            paragraph
                .children
                .iter()
                .filter(|child| child.image.is_some())
                .count(),
            3
        );

        let content = parse_markdown(content, "er \"updated title\"", true);
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained paragraph after definition edit");
        };
        assert!(Arc::ptr_eq(&retained_state, &paragraph.state));

        let destinations = paragraph
            .children
            .iter()
            .filter_map(|inline| {
                let image = inline.image.as_ref()?;
                Some((
                    inline.image_reference_identifier.as_deref(),
                    image
                        .resolved_url(inline.image_reference_identifier.as_ref(), &content.node_cx)
                        .to_string(),
                    image.resolved_title(
                        inline.image_reference_identifier.as_ref(),
                        &content.node_cx,
                    ),
                ))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            destinations,
            vec![
                (
                    Some("stable"),
                    "https://example.com/older".into(),
                    "updated title".into(),
                );
                3
            ]
        );
    }

    #[test]
    fn retained_definitions_participate_in_appended_source_preparation() {
        let extensions = Arc::new(
            MarkdownExtensions::default()
                .parse_options(|options| options.constructs.math_text = true)
                .prepare_source(|source| {
                    let has_resolved_reference =
                        markdown::to_mdast(source, &markdown::ParseOptions::gfm())
                            .is_ok_and(|root| mdast_contains_link_reference(&root));
                    if has_resolved_reference {
                        source.replace(r"\(", "$$").replace(r"\)", "$$")
                    } else {
                        source.to_string()
                    }
                })
                .inline_parser(|node, cx| {
                    let markdown::mdast::Node::InlineMath(_) = node else {
                        return None;
                    };
                    Some(
                        MarkdownNode::new(
                            "retained-reference-math",
                            (
                                cx.node_source(node)?.to_string(),
                                cx.prepared_node_source(node)?.to_string(),
                                cx.node_range(node)?,
                            ),
                        )
                        .text(cx.node_source(node)?),
                    )
                }),
        );
        let source = "untouched\n\n[label \\(x\\)]: https://example.com/math\n\ntrailing";
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            source,
            false,
            extensions.clone(),
        );
        let node::BlockNode::Paragraph(untouched) = &content.document.blocks[0] else {
            panic!("expected retained paragraph");
        };
        let retained_state = untouched.state.clone();

        let content =
            parse_markdown_with_extensions(content, "\n\n[label \\(x\\)][]", true, extensions);

        let node::BlockNode::Paragraph(untouched) = &content.document.blocks[0] else {
            panic!("expected retained paragraph after append");
        };
        assert!(
            Arc::ptr_eq(&retained_state, &untouched.state),
            "preparing the appended fragment must not rebuild unrelated retained blocks"
        );
        let custom = content
            .document
            .blocks
            .iter()
            .filter_map(|block| match block {
                node::BlockNode::Paragraph(paragraph) => Some(paragraph),
                _ => None,
            })
            .flat_map(|paragraph| &paragraph.children)
            .find(|child| {
                child
                    .custom
                    .as_ref()
                    .is_some_and(|node| node.name() == "retained-reference-math")
            })
            .expect("appended reference label should contain prepared inline math");
        let custom_node = custom.custom.as_ref().unwrap();
        let appended_formula_start = source.len() + "\n\n[label ".len();
        assert_eq!(
            custom_node.data::<(String, String, std::ops::Range<usize>)>(),
            Some(&(
                r"\(x\)".to_string(),
                "$$x$$".to_string(),
                appended_formula_start..appended_formula_start + r"\(x\)".len(),
            ))
        );
        assert!(custom.marks.iter().any(|(_, mark)| {
            mark.link.as_ref().is_some_and(|link| {
                link.identifier
                    .as_ref()
                    .is_some_and(|identifier| content.node_cx.link_refs.contains_key(identifier))
            })
        }));
        assert_eq!(
            content.document.source.as_ref(),
            format!("{source}\n\n[label \\(x\\)][]")
        );
    }

    #[test]
    fn retained_definition_replay_prepares_authoritative_spelling_once() {
        let extensions = Arc::new(MarkdownExtensions::default().prepare_source(|source| {
            source
                .chars()
                .map(|character| match character {
                    'a' => 'b',
                    'b' => 'c',
                    _ => character,
                })
                .collect()
        }));
        let source = "[aa]: /one\n\nuntouched\n\ntrailing";
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            source,
            false,
            extensions.clone(),
        );
        assert_eq!(
            content
                .node_cx
                .link_ref_source_identifiers
                .get("bb")
                .map(|identifier| identifier.as_ref()),
            Some("aa")
        );
        let node::BlockNode::Paragraph(untouched) = &content.document.blocks[1] else {
            panic!("expected retained paragraph");
        };
        let retained_state = untouched.state.clone();

        let content = parse_markdown_with_extensions(content, "\n\n[aa][]", true, extensions);
        assert!(
            content
                .document
                .blocks
                .iter()
                .any(|block| block_contains_reference(block, "bb")),
            "a non-idempotent preparer must not prepare a retained label twice"
        );
        assert_eq!(content.node_cx.link_refs["bb"].url.as_ref(), "/one");
        let node::BlockNode::Paragraph(untouched) = &content.document.blocks[1] else {
            panic!("expected retained paragraph after append");
        };
        assert!(Arc::ptr_eq(&retained_state, &untouched.state));
        assert_eq!(
            content.document.source.as_ref(),
            format!("{source}\n\n[aa][]")
        );
    }

    #[test]
    fn invalid_synthetic_definition_shape_falls_back_to_full_source() {
        let extensions = Arc::new(MarkdownExtensions::default().prepare_source(|source| {
            if source.contains("[ref]: /\n") {
                source.replacen("[ref]: /\n", "[ref]  /\n", 1)
            } else {
                source.to_string()
            }
        }));
        let source = "[ref]: /target\n\nuntouched\n\ntrailing";
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            source,
            false,
            extensions.clone(),
        );
        let content = parse_markdown_with_extensions(content, "\n\n[value][ref]", true, extensions);

        assert!(
            content
                .document
                .blocks
                .iter()
                .any(|block| block_contains_reference(block, "ref"))
        );
        assert_eq!(content.node_cx.link_refs["ref"].url.as_ref(), "/target");
        assert_eq!(
            content.document.source.as_ref(),
            format!("{source}\n\n[value][ref]")
        );
    }

    #[test]
    fn merged_synthetic_identifiers_fall_back_to_full_source() {
        let extensions = Arc::new(MarkdownExtensions::default().prepare_source(|source| {
            if source.contains("[left]: /\n") && source.contains("[rght]: /\n") {
                source
                    .replace("[left]: /\n", "[same]: /\n")
                    .replace("[rght]: /\n", "[same]: /\n")
            } else {
                source.to_string()
            }
        }));
        let source = "[left]: /left\n[rght]: /right\n\ntrailing";
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            source,
            false,
            extensions.clone(),
        );
        let content =
            parse_markdown_with_extensions(content, "\n\n[a][left] [b][rght]", true, extensions);

        for (identifier, url) in [("left", "/left"), ("rght", "/right")] {
            assert!(
                content
                    .document
                    .blocks
                    .iter()
                    .any(|block| block_contains_reference(block, identifier))
            );
            assert_eq!(content.node_cx.link_refs[identifier].url.as_ref(), url);
        }
        assert_eq!(
            content.document.source.as_ref(),
            format!("{source}\n\n[a][left] [b][rght]")
        );
    }

    #[test]
    fn append_retains_the_reparseable_reference_source_identifier() {
        // Unicode case normalization expands each U+0130 into multiple code
        // points. The normalized key no longer fits markdown-rs's definition
        // label limit even though the original, valid source label does.
        let source_identifier = "İ".repeat(499);
        let source = format!(
            "[{source_identifier}]: https://example.com/unicode\n\n[value][{source_identifier}]"
        );
        let content = parse_markdown(ParsedContent::default(), &source, false);
        let normalized_identifier = content
            .node_cx
            .link_refs
            .keys()
            .next()
            .expect("Unicode reference definition should parse")
            .clone();
        assert_ne!(normalized_identifier.as_ref(), source_identifier);
        assert_eq!(
            content
                .node_cx
                .link_ref_source_identifiers
                .get(&normalized_identifier)
                .map(|identifier| identifier.as_ref()),
            Some(source_identifier.as_str())
        );
        assert!(paragraph_has_reference(&content, 1, &normalized_identifier));

        let content = parse_markdown(content, " tail", true);
        assert!(
            paragraph_has_reference(&content, 1, &normalized_identifier),
            "append must use the valid source label instead of the expanded normalized key"
        );
        assert_eq!(
            content.node_cx.link_refs[&normalized_identifier]
                .url
                .as_ref(),
            "https://example.com/unicode"
        );
    }

    #[test]
    fn append_replays_multiline_definitions_outside_their_container() {
        let content = parse_markdown(
            ParsedContent::default(),
            "> [foo\n> bar]: https://example.com/container\n\n> [value][foo\n> bar]",
            false,
        );
        let normalized_identifier = content
            .node_cx
            .link_refs
            .keys()
            .next()
            .expect("container reference definition should parse")
            .clone();
        assert_eq!(normalized_identifier.as_ref(), "foo> bar");
        assert!(block_contains_reference(
            &content.document.blocks[1],
            &normalized_identifier
        ));

        let content = parse_markdown(content, " tail", true);
        assert!(
            block_contains_reference(&content.document.blocks[1], &normalized_identifier),
            "container continuation markers must not invalidate the retained prefix"
        );
        assert_eq!(
            content.node_cx.link_refs[&normalized_identifier]
                .url
                .as_ref(),
            "https://example.com/container"
        );
    }

    #[test]
    fn append_falls_back_when_a_reference_identifier_cannot_be_replayed() {
        let mut content = parse_markdown(
            ParsedContent::default(),
            "[ref]: https://example.com/fallback\n\n[value][ref]",
            false,
        );
        content.node_cx.link_ref_source_identifiers.clear();

        let content = parse_markdown(content, " tail", true);
        assert_eq!(
            content.document.source.as_ref(),
            "[ref]: https://example.com/fallback\n\n[value][ref] tail"
        );
        assert!(paragraph_has_reference(&content, 1, "ref"));
        assert_eq!(
            content.node_cx.link_refs["ref"].url.as_ref(),
            "https://example.com/fallback"
        );
    }

    #[test]
    fn append_preserves_gfm_footnote_definition_context() {
        let content = parse_markdown(
            ParsedContent::default(),
            "retained paragraph\n\n[^n]: note\n\ntrailing",
            false,
        );
        assert!(content.node_cx.has_footnote_definition("n"));
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained paragraph");
        };
        let retained_state = paragraph.state.clone();
        let content = parse_markdown(content, " [^n]", true);
        assert!(
            paragraph_has_footnote_reference(&content, 2, "n"),
            "a retained definition must resolve a newly appended footnote reference"
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained paragraph after append");
        };
        assert!(
            Arc::ptr_eq(&retained_state, &paragraph.state),
            "retained footnote context must not rebuild unrelated paragraph state"
        );

        let content = parse_markdown(
            ParsedContent::default(),
            "before [^late]\n\ntrailing",
            false,
        );
        assert!(!paragraph_has_footnote_reference(&content, 0, "late"));
        let content = parse_markdown(content, "\n\n[^late]: note", true);
        assert!(
            paragraph_has_footnote_reference(&content, 0, "late"),
            "a new definition must reparse retained literal footnote references"
        );
    }

    #[test]
    fn append_collects_footnote_definitions_before_custom_block_parsers() {
        let direct_extensions = Arc::new(MarkdownExtensions::default().block_parser(|node, _| {
            matches!(node, markdown::mdast::Node::FootnoteDefinition(_))
                .then(|| MarkdownNode::new("custom-footnote", ()))
        }));
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            "[^direct]: note\n\ntrailing",
            false,
            direct_extensions.clone(),
        );
        assert!(content.node_cx.has_footnote_definition("direct"));
        let content =
            parse_markdown_with_extensions(content, " [^direct]", true, direct_extensions);
        assert!(paragraph_has_footnote_reference(&content, 1, "direct"));

        let container_extensions =
            Arc::new(MarkdownExtensions::default().block_parser(|node, _| {
                matches!(node, markdown::mdast::Node::Blockquote(_))
                    .then(|| MarkdownNode::new("custom-footnote-container", ()))
            }));
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            "> [^nested]: note\n\ntrailing",
            false,
            container_extensions.clone(),
        );
        assert!(content.node_cx.has_footnote_definition("nested"));
        let content =
            parse_markdown_with_extensions(content, " [^nested]", true, container_extensions);
        assert!(paragraph_has_footnote_reference(&content, 1, "nested"));
    }

    #[test]
    fn append_collects_link_definitions_before_custom_block_parsers() {
        let direct_extensions = Arc::new(MarkdownExtensions::default().block_parser(|node, _| {
            matches!(node, markdown::mdast::Node::Definition(_))
                .then(|| MarkdownNode::new("custom-link-definition", ()))
        }));
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            "[direct]: https://example.com/direct\n\ntrailing",
            false,
            direct_extensions.clone(),
        );
        assert_eq!(
            content.node_cx.link_refs["direct"].url.as_ref(),
            "https://example.com/direct"
        );
        let content = parse_markdown_with_extensions(
            content,
            " [value][direct]",
            true,
            direct_extensions.clone(),
        );
        assert!(paragraph_has_reference(&content, 1, "direct"));

        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            "[value][stable]\n\n[stable]: https://example.com/old",
            false,
            direct_extensions.clone(),
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained link paragraph");
        };
        let retained_state = paragraph.state.clone();
        let content =
            parse_markdown_with_extensions(content, "er", true, direct_extensions.clone());
        assert_eq!(
            content.node_cx.link_refs["stable"].url.as_ref(),
            "https://example.com/older"
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained link paragraph after URL append");
        };
        assert!(Arc::ptr_eq(&retained_state, &paragraph.state));
        let content = parse_markdown_with_extensions(
            content,
            " \"unterminated title",
            true,
            direct_extensions,
        );
        assert!(!content.node_cx.link_refs.contains_key("stable"));
        assert!(!paragraph_has_reference(&content, 0, "stable"));

        let container_extensions =
            Arc::new(MarkdownExtensions::default().block_parser(|node, _| {
                matches!(node, markdown::mdast::Node::Blockquote(_))
                    .then(|| MarkdownNode::new("custom-link-container", ()))
            }));
        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            "> [nested]: https://example.com/nested\n\ntrailing",
            false,
            container_extensions.clone(),
        );
        assert_eq!(
            content.node_cx.link_refs["nested"].url.as_ref(),
            "https://example.com/nested"
        );
        let content = parse_markdown_with_extensions(
            content,
            " [value][nested]",
            true,
            container_extensions.clone(),
        );
        assert!(paragraph_has_reference(&content, 1, "nested"));

        let content = parse_markdown_with_extensions(
            ParsedContent::default(),
            "[value][nested]\n\n> [nested]: https://example.com/old",
            false,
            container_extensions.clone(),
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained nested link paragraph");
        };
        let retained_state = paragraph.state.clone();
        let content = parse_markdown_with_extensions(content, "er", true, container_extensions);
        assert_eq!(
            content.node_cx.link_refs["nested"].url.as_ref(),
            "https://example.com/older"
        );
        let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
            panic!("expected retained nested link paragraph after URL append");
        };
        assert!(Arc::ptr_eq(&retained_state, &paragraph.state));
    }

    #[test]
    fn append_retains_reparseable_footnote_source_identifiers() {
        for source_identifier in ["a\\]b".to_string(), "İ".repeat(499)] {
            let source = format!("retained paragraph\n\n[^{source_identifier}]: note\n\ntrailing");
            let content = parse_markdown(ParsedContent::default(), &source, false);
            let normalized_identifier = content
                .node_cx
                .footnote_definition_identifiers()
                .into_iter()
                .next()
                .expect("footnote definition should parse");
            let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
                panic!("expected retained paragraph");
            };
            let retained_state = paragraph.state.clone();

            let content = parse_markdown(content, &format!(" [^{source_identifier}]"), true);
            assert!(paragraph_has_footnote_reference(
                &content,
                2,
                &normalized_identifier
            ));
            let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
                panic!("expected retained paragraph after append");
            };
            assert!(
                Arc::ptr_eq(&retained_state, &paragraph.state),
                "footnote source identifier {source_identifier:?} should be replayed incrementally"
            );
        }
    }

    #[test]
    fn retained_footnote_prefix_keeps_indented_code_at_document_root() {
        for indentation in ["    ", "\t"] {
            let source = format!(
                "retained paragraph\n\n[^n]: note\n\nseparator paragraph\n\n{indentation}code"
            );
            let expected_source = format!("{source} more");
            let expected = parse_markdown(ParsedContent::default(), &expected_source, false);
            let content = parse_markdown(ParsedContent::default(), &source, false);
            let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
                panic!("expected retained paragraph");
            };
            let retained_state = paragraph.state.clone();

            let content = parse_markdown(content, " more", true);
            assert_eq!(content.document.source.as_ref(), expected_source);
            let node::BlockNode::CodeBlock(actual_code) = &content.document.blocks[3] else {
                panic!("expected appended indented code block");
            };
            let node::BlockNode::CodeBlock(expected_code) = &expected.document.blocks[3] else {
                panic!("expected baseline indented code block");
            };
            assert_eq!(actual_code.code(), expected_code.code());
            assert_eq!(actual_code.span, expected_code.span);
            let node::BlockNode::Paragraph(paragraph) = &content.document.blocks[0] else {
                panic!("expected retained paragraph after append");
            };
            assert!(Arc::ptr_eq(&retained_state, &paragraph.state));
        }
    }

    #[test]
    fn source_preparer_observes_the_retained_footnote_boundary() {
        let extensions = Arc::new(MarkdownExtensions::default().prepare_source(|source| {
            let root_has_code = markdown::to_mdast(source, &markdown::ParseOptions::gfm())
                .is_ok_and(|root| {
                    matches!(
                        root,
                        markdown::mdast::Node::Root(root)
                            if root
                                .children
                                .iter()
                                .any(|child| matches!(child, markdown::mdast::Node::Code(_)))
                    )
                });
            if root_has_code {
                source.replace("code", "root")
            } else {
                source.to_string()
            }
        }));

        for indentation in ["    ", "\t"] {
            let source = format!(
                "retained paragraph\n\n[^n]: note\n\nseparator paragraph\n\n{indentation}code"
            );
            let expected_source = format!("{source} more");
            let expected = parse_markdown_with_extensions(
                ParsedContent::default(),
                &expected_source,
                false,
                extensions.clone(),
            );
            let content = parse_markdown_with_extensions(
                ParsedContent::default(),
                &source,
                false,
                extensions.clone(),
            );
            let content =
                parse_markdown_with_extensions(content, " more", true, extensions.clone());
            let node::BlockNode::CodeBlock(actual_code) = &content.document.blocks[3] else {
                panic!("expected appended indented code block");
            };
            let node::BlockNode::CodeBlock(expected_code) = &expected.document.blocks[3] else {
                panic!("expected baseline indented code block");
            };
            assert_eq!(actual_code.code(), expected_code.code());
            assert_eq!(actual_code.code().as_ref(), "root more");
            assert_eq!(actual_code.span, expected_code.span);
        }
    }

    #[test]
    fn failed_same_source_replacement_forces_full_parse_before_append() {
        let (tx, rx) = unbounded::<UpdateOptions>();
        let (tx_result, rx_result) = unbounded::<ParsedUpdate>();
        let mut future = Box::pin(UpdateFuture::new(TextViewFormat::Markdown, rx, tx_result));
        let waker = futures::task::noop_waker();
        let mut task_cx = std::task::Context::from_waker(&waker);
        let source = "first block\n\nlast block";

        tx.try_send(UpdateOptions {
            revision: 1,
            pending_text: source.to_string(),
            append: false,
            markdown_extensions: Arc::default(),
            mode: ParseMode::BaselineAck,
        })
        .unwrap();
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut task_cx),
            Poll::Pending
        ));

        let invalid_extensions = Arc::new(MarkdownExtensions::default().prepare_source(|source| {
            if source.contains("first block") {
                format!("{source}!")
            } else {
                source.to_string()
            }
        }));
        tx.try_send(UpdateOptions {
            revision: 2,
            pending_text: source.to_string(),
            append: false,
            markdown_extensions: invalid_extensions.clone(),
            mode: ParseMode::BaselineAck,
        })
        .unwrap();
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut task_cx),
            Poll::Pending
        ));

        tx.try_send(UpdateOptions {
            revision: 3,
            pending_text: " tail".to_string(),
            append: true,
            markdown_extensions: invalid_extensions,
            mode: ParseMode::Compatible,
        })
        .unwrap();
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut task_cx),
            Poll::Pending
        ));

        // Baseline acknowledgements are published too; the append result is
        // the last one.
        let mut parsed_update = rx_result.try_recv().expect("parse results");
        while let Ok(next) = rx_result.try_recv() {
            parsed_update = next;
        }
        assert_eq!(parsed_update.revision, 3);
        assert!(
            parsed_update.result.is_err(),
            "append reused content from an extension revision whose replacement failed"
        );
    }

    #[gpui::test]
    fn failed_streaming_append_keeps_stable_document_and_later_recovers(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("stable", cx)));
        cx.run_until_parked();

        let extensions = MarkdownExtensions::default().try_prepare_source(|source| {
            if source.ends_with('[') {
                Err("temporarily incomplete")
            } else {
                Ok(source.to_string())
            }
        });
        state.update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(extensions), cx);
        });
        cx.run_until_parked();

        state.update(cx, |state, cx| state.push_str(" [", cx));
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert_eq!(state.text, "stable [");
            assert_eq!(state.source().as_ref(), "stable");
            assert_eq!(state.parsed_content.document.text().trim(), "stable");
            assert!(
                state.parsed_error.is_none(),
                "a transient append error must not replace stable content"
            );
        });

        state.update(cx, |state, cx| state.push_str("]", cx));
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert_eq!(state.text, "stable []");
            assert_eq!(state.source().as_ref(), "stable []");
            assert_eq!(state.parsed_content.document.text().trim(), "stable []");
            assert!(state.parsed_error.is_none());
        });
    }

    #[gpui::test]
    fn failed_append_coalesced_with_replacement_keeps_synchronous_document(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("old", cx)));
        cx.run_until_parked();

        let extensions = MarkdownExtensions::default().try_prepare_source(|source| {
            if source.ends_with('[') {
                Err("temporarily incomplete")
            } else {
                Ok(source.to_string())
            }
        });
        state.update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(extensions), cx);
        });
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.set_text("replacement", cx);
            state.push_str(" [", cx);
        });
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert_eq!(state.text, "replacement [");
            assert_eq!(state.source().as_ref(), "replacement");
            assert_eq!(state.parsed_content.document.text().trim(), "replacement");
            assert!(
                state.parsed_error.is_none(),
                "an appended tail stays transient when coalesced with a replacement"
            );
        });

        state.update(cx, |state, cx| state.push_str("]", cx));
        cx.run_until_parked();
        state.read_with(cx, |state, _| {
            assert_eq!(state.text, "replacement []");
            assert_eq!(state.source().as_ref(), "replacement []");
            assert_eq!(
                state.parsed_content.document.text().trim(),
                "replacement []"
            );
            assert!(state.parsed_error.is_none());
        });
    }

    #[gpui::test]
    fn synchronous_replacement_worker_sync_does_not_clear_new_selection(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("old", cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.set_text("replacement", cx);
            state.select_all(cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert!(state.has_view_selection());
            assert_eq!(state.selected_text().trim(), "replacement");
        });
    }

    #[gpui::test]
    fn inline_markdown_extensions_survive_streaming_append(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("before $x$", cx)));
        cx.run_until_parked();

        let extensions = MarkdownExtensions::default()
            .parse_options(|options| options.constructs.math_text = true)
            .inline_parser(|node, cx| {
                let markdown::mdast::Node::InlineMath(math) = node else {
                    return None;
                };
                Some(
                    MarkdownNode::new("streaming-math", math.value.clone())
                        .text(cx.node_source(node).unwrap_or_default()),
                )
            });
        state.update(cx, |state, cx| {
            state.set_markdown_extensions(Arc::new(extensions), cx);
            state.push_str(" after $y$", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.source().as_ref(), "before $x$ after $y$");
            let node::BlockNode::Paragraph(paragraph) = &state.parsed_content.document.blocks[0]
            else {
                panic!("expected paragraph");
            };
            assert_eq!(
                paragraph
                    .children
                    .iter()
                    .filter(|child| child.custom.is_some())
                    .count(),
                2
            );
        });
    }
}
