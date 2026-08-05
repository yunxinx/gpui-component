use futures::Stream as _;
use std::{pin::Pin, sync::Arc, task::Poll};

use gpui::{
    App, AppContext as _, Bounds, Context, FocusHandle, IntoElement, KeyBinding, ListState,
    ParentElement as _, Pixels, Point, Render, SharedString, Styled as _, Task, Window,
    prelude::FluentBuilder as _, px,
};
use rust_i18n::t;

use crate::{
    ElementExt,
    async_util::{Receiver, Sender, unbounded},
    input::{self, SelectAll},
    scroll::AutoScroll,
    text::{
        CodeBlockActionsFn, MarkdownExtensions, TextViewStyle,
        document::ParsedDocument,
        format,
        node::{self, NodeContext},
    },
    v_flex,
};

const CONTEXT: &'static str = "TextView";
// Keep coalescing bounded so sustained streams still render intermediate updates.
const MAX_COALESCED_UPDATES_PER_PARSE: usize = 64;

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
    /// Plain-text view.
    Plain,
    /// Markdown view
    Markdown,
    /// HTML view
    Html,
}

/// The state of a TextView.
pub struct TextViewState {
    pub(super) focus_handle: FocusHandle,
    pub(super) entity_id: gpui::EntityId,
    pub(super) list_state: ListState,
    estimated_block_height: Option<Pixels>,

    /// The bounds of the text view
    bounds: Bounds<Pixels>,

    pub(super) selectable: bool,
    pub(super) scrollable: bool,
    pub(super) text_view_style: TextViewStyle,
    pub(super) code_block_actions: Option<std::sync::Arc<CodeBlockActionsFn>>,
    pub(super) markdown_extensions: Arc<MarkdownExtensions>,

    pub(super) is_selecting: bool,
    multi_click_selection: Option<TextViewMultiClickSelection>,
    selected_text_override: Option<String>,
    select_all: bool,
    pub(super) auto_scroll: AutoScroll,

    pub(super) parsed_content: ParsedContent,
    /// Content format (plain text / Markdown / HTML), used to parse
    /// synchronously on the main thread for full-replace updates.
    format: TextViewFormat,
    text: String,
    revision: usize,
    parsed_error: Option<SharedString>,
    tx: Sender<UpdateOptions>,
    _parse_task: Task<()>,
    _receive_task: Task<()>,
}

impl TextViewState {
    /// Create a plain-text TextViewState.
    pub fn plain(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Plain, text, true, cx)
    }

    /// Create a Markdown TextViewState.
    pub fn markdown(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Markdown, text, true, cx)
    }

    /// Create a Markdown state whose scrollable view measures only visible blocks initially.
    ///
    /// The retained [`ListState`] is created with lazy measurement and is never
    /// replaced, so callers may safely keep the handle returned by
    /// [`Self::scroll_state`]. This is intended for large documents shown in a
    /// definite-height [`TextView`](super::TextView).
    pub fn markdown_with_lazy_scroll_measurement(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Markdown, text, false, cx)
    }

    /// Create a HTML TextViewState.
    pub fn html(text: &str, cx: &mut Context<Self>) -> Self {
        Self::new(TextViewFormat::Html, text, true, cx)
    }

    /// Create a new TextViewState.
    fn new(
        format: TextViewFormat,
        text: &str,
        eager_scroll_measurement: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let entity_id = cx.entity_id();

        let (tx, rx) = unbounded::<UpdateOptions>();
        let (tx_result, rx_result) = unbounded::<ParsedUpdate>();
        let _receive_task = cx.spawn({
            async move |weak_self, cx| {
                while let Ok(parsed_update) = rx_result.recv().await {
                    _ = weak_self.update(cx, |state, cx| {
                        if parsed_update.revision != state.revision {
                            return;
                        }

                        match parsed_update.result {
                            Ok(content) => {
                                state.replace_parsed_content(content);
                                state.parsed_error = None;
                            }
                            Err(_) if state.parsed_error.is_none() => {
                                // Only append-driven batches publish worker
                                // results; full replacements parse synchronously.
                                // Keep the last successful document visible even
                                // when a coalesced replacement + append requires a
                                // full parse. The worker retains the authoritative
                                // source and retries it on the next append.
                                return;
                            }
                            Err(err) => {
                                state.parsed_error = Some(err);
                            }
                        }
                        // Don't interrupt an active drag-selection; the stored
                        // positions remain valid for append-only updates and will
                        // self-correct on the next mouse-move event.
                        if !state.is_selecting {
                            state.reset_selection();
                        }
                        cx.notify();
                    });
                }
            }
        });

        let _parse_task = cx.background_spawn(UpdateFuture::new(format, rx, tx_result));

        let mut this = Self {
            focus_handle,
            entity_id,
            bounds: Bounds::default(),
            multi_click_selection: None,
            selected_text_override: None,
            select_all: false,
            selectable: false,
            scrollable: false,
            // Eager measurement keeps ordinary scrollbar thumbs exact. Large
            // documents opt into lazy measurement at construction time so the
            // retained handle is never replaced after being shared.
            list_state: if eager_scroll_measurement {
                ListState::new(0, gpui::ListAlignment::Top, px(1000.)).measure_all()
            } else {
                ListState::new(0, gpui::ListAlignment::Top, px(1000.))
            },
            estimated_block_height: None,
            text_view_style: TextViewStyle::default(),
            code_block_actions: None,
            markdown_extensions: Arc::default(),
            is_selecting: false,
            auto_scroll: AutoScroll::default(),
            parsed_content: Default::default(),
            format,
            parsed_error: None,
            text: text.to_string(),
            revision: 0,
            tx,
            _parse_task,
            _receive_task,
        };
        this.increment_update(&text, false, cx);
        this
    }

    /// Get the text content.
    pub(crate) fn source(&self) -> SharedString {
        self.parsed_content.document.source.clone()
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

    /// Set whether the text is selectable, default false.
    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    /// Set whether the text is selectable, default false.
    pub fn set_scrollable(&mut self, scrollable: bool, cx: &mut Context<Self>) {
        if !scrollable {
            self.reset_selection();
        }
        self.scrollable = scrollable;
        cx.notify();
    }

    /// Return the retained scroll state used by scrollable text views.
    ///
    /// Owners can use this to coordinate an enclosing scrollbar, follow-tail
    /// behavior, or a custom scroll animation without replacing the document.
    pub fn scroll_state(&self) -> ListState {
        self.list_state.clone()
    }

    fn replace_parsed_content(&mut self, content: ParsedContent) {
        self.sync_list_items(&content);
        self.parsed_content = content;
    }

    fn sync_list_items(&self, content: &ParsedContent) {
        let Some(estimated_height) = self.estimated_block_height else {
            return;
        };
        let old_blocks = &self.parsed_content.document.blocks;
        let new_blocks = &content.document.blocks;
        if self.list_state.item_count() != old_blocks.len() {
            return;
        }

        let prefix_len = old_blocks
            .iter()
            .zip(new_blocks)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix_len = old_blocks[prefix_len..]
            .iter()
            .rev()
            .zip(new_blocks[prefix_len..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let old_changed_len = old_blocks.len() - prefix_len - suffix_len;
        let new_changed_len = new_blocks.len() - prefix_len - suffix_len;
        let retained_changed_len = old_changed_len.min(new_changed_len);

        if old_blocks.len() != new_blocks.len() && suffix_len == 0 && prefix_len > 0 {
            // An unchanged boundary block gains or loses the paragraph gap
            // when an append or truncation changes which block is last.
            self.list_state.remeasure_items(prefix_len - 1..prefix_len);
        }
        if retained_changed_len > 0 {
            self.list_state
                .remeasure_items(prefix_len..prefix_len + retained_changed_len);
        }

        let removed_range = prefix_len + retained_changed_len
            ..prefix_len + retained_changed_len + (old_changed_len - retained_changed_len);
        let inserted_count = new_changed_len - retained_changed_len;
        if !removed_range.is_empty() || inserted_count > 0 {
            self.list_state.splice_with_uniform_height(
                removed_range,
                inserted_count,
                estimated_height,
            );
        }
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
    ///
    /// If an append cannot be parsed, the last successfully parsed document
    /// remains visible. A later append reparses the complete authoritative
    /// source, allowing temporarily incomplete streaming constructs to recover.
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

        self.markdown_extensions = markdown_extensions;
        if self.format == TextViewFormat::Markdown {
            let text = self.text.clone();
            self.increment_update(&text, false, cx);
        }
    }

    /// Return the selected text.
    pub fn selected_text(&self) -> String {
        if self.select_all {
            if self.format == TextViewFormat::Plain {
                return self.text.clone();
            }
            return self.parsed_content.document.text();
        }

        if let Some(text) = &self.selected_text_override {
            return text.clone();
        }

        self.parsed_content.document.selected_text()
    }

    pub(crate) fn preserves_copy_boundaries(&self) -> bool {
        self.format == TextViewFormat::Plain
    }

    fn increment_update(&mut self, text: &str, append: bool, cx: &mut Context<Self>) {
        self.revision += 1;
        let update_options = UpdateOptions {
            revision: self.revision,
            append,
            pending_text: text.to_string(),
            markdown_extensions: self.markdown_extensions.clone(),
            // Full replacements are applied synchronously below. The worker
            // still receives them so its incremental snapshot stays in sync,
            // but publishing the same revision a second time would clear a
            // selection made between the synchronous parse and worker poll.
            publish_result: append,
        };

        // Full-replace updates (initial content / `set_text`) parse
        // synchronously on the main thread so the first layout already has the
        // correct height. Otherwise parsing finishes later on a background task
        // and the first layout sees an empty `parsed_content` (~0 height); when
        // this `TextView` is an item inside an outer `list` with `measure_all`,
        // off-screen items get measured at that empty height and the total
        // content height keeps growing as items scroll into view; the scrollbar
        // thumb jitters. Streaming appends stay async to avoid re-parsing the
        // whole document on every chunk.
        if !append {
            match parse_content(self.format, ParsedContent::default(), &update_options) {
                Ok(content) => {
                    self.replace_parsed_content(content);
                    self.parsed_error = None;
                    if !self.is_selecting {
                        self.reset_selection();
                    }
                }
                Err(err) => {
                    self.parsed_error = Some(err);
                }
            }
            // Preserve the exact update order for the background parser. A
            // later append can now build on this replacement instead of the
            // worker's previous document snapshot.
            _ = self.tx.try_send(update_options);
            cx.notify();
            return;
        }

        _ = self.tx.try_send(update_options);
    }

    /// Save bounds and unselect if bounds changed.
    pub(super) fn update_bounds(&mut self, bounds: Bounds<Pixels>) {
        if self.bounds.size != bounds.size {
            self.reset_selection();
        }
        self.bounds = bounds;
    }

    pub(super) fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
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

    fn reset_selection(&mut self) {
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

    /// Clear the current text selection.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.reset_selection();
        cx.notify();
    }

    pub(super) fn scroll_offset(&self) -> Point<Pixels> {
        if self.scrollable {
            self.list_state.scroll_px_offset_for_scrollbar()
        } else {
            Point::default()
        }
    }

    /// Select all rendered text in this view.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.multi_click_selection = None;
        self.selected_text_override = None;
        self.select_all = true;
        self.is_selecting = false;
        self.auto_scroll.stop();
        cx.notify();
    }

    pub(crate) fn set_multi_click_selection(
        &mut self,
        pos: Point<Pixels>,
        kind: TextViewMultiClickKind,
        selected_text: String,
    ) {
        let scroll_offset = self.scroll_offset();
        let pos = pos - self.bounds.origin - scroll_offset;
        self.multi_click_selection = Some(TextViewMultiClickSelection { pos, kind });
        self.selected_text_override = Some(selected_text);
        self.select_all = false;
        self.is_selecting = false;
        self.auto_scroll.stop();
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
    pub(crate) fn selection_points(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<(Point<Pixels>, Point<Pixels>)> {
        if !self.selectable {
            return None;
        }
        let root = window.root::<crate::Root>().flatten()?;
        let selection = &root.read(cx).text_selection;
        if let Some(view_id) = selection.single_view() {
            if view_id != self.entity_id {
                return None;
            }
        }
        selection.resolved_points(cx)
    }

    pub(crate) fn has_selection(&self, window: &Window, cx: &App) -> bool {
        self.has_view_selection() || self.selection_points(window, cx).is_some()
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
        self.estimated_block_height = Some(
            window.line_height().max(px(1.))
                + self
                    .text_view_style
                    .paragraph_gap
                    .to_pixels(window.rem_size()),
        );
        let state = cx.entity();
        let document = self.parsed_content.document.clone();
        let mut node_cx = self.parsed_content.node_cx.clone();

        node_cx.code_block_actions = self.code_block_actions.clone();
        node_cx.markdown_extensions = self.markdown_extensions.clone();
        node_cx.style = self.text_view_style.clone();
        let parsed_error = self
            .parsed_error
            .as_ref()
            .map(|error| self.markdown_extensions.format_parse_error(error));

        v_flex()
            .size_full()
            .map(|this| match parsed_error {
                None => this.child(document.render_root(
                    if self.scrollable {
                        Some(self.list_state.clone())
                    } else {
                        None
                    },
                    &node_cx,
                    window,
                    cx,
                )),
                Some(err) => this.child(
                    v_flex()
                        .gap_1()
                        .child(t!("TextView.failed_to_parse").to_string())
                        .child(err),
                ),
            })
            .on_prepaint(move |bounds, window, cx| {
                let size_changed = state.read(cx).bounds().size != bounds.size;
                let id = state.entity_id();
                state.update(cx, |state, _| {
                    state.update_bounds(bounds);
                });
                if size_changed {
                    if let Some(root) = window.root::<crate::Root>().flatten() {
                        root.update(cx, |root, cx| {
                            root.clear_text_selection_for_resized_view(id, cx);
                        });
                    }
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
                markdown_extensions: options.markdown_extensions.clone(),
                publish_result: options.publish_result,
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
                    if options.publish_result {
                        _ = self.tx_result.try_send(ParsedUpdate {
                            revision: options.revision,
                            result: res,
                        });
                    }
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
    markdown_extensions: Arc<MarkdownExtensions>,
    publish_result: bool,
}

impl UpdateOptions {
    fn merge(&mut self, next: UpdateOptions) {
        if next.append {
            self.pending_text.push_str(&next.pending_text);
            self.revision = next.revision;
            self.publish_result |= next.publish_result;
        } else {
            *self = next;
        }
    }
}

struct ParsedUpdate {
    revision: usize,
    result: Result<ParsedContent, SharedString>,
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
            markdown_extensions: options.markdown_extensions.clone(),
            publish_result: options.publish_result,
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

    let mut source = String::new();
    if options.append {
        if let Some(last_block) = content.document.blocks.pop() {
            // The last block is reparsed below. Remove any definitions it
            // contributed so invalidated or renamed definitions cannot linger.
            if let Some(span) = last_block.span() {
                if !node_cx.retain_definitions_before(span.start) {
                    return parse_appended_full_replacement(format, &content, options);
                }
                node_cx.offset = span.start;
                let last_source = &content.document.source[span.start..];
                source.push_str(last_source);
                source.push_str(&options.pending_text);
            } else {
                source.push_str(&options.pending_text);
            }
        } else {
            source.push_str(&options.pending_text);
        }
    } else {
        source.push_str(&options.pending_text);
    }

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
        content.document.blocks.extend(new_document.blocks);
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
                publish_result: true,
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

    #[gpui::test]
    fn set_text_then_push_str_appends_to_replaced_content(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("old", cx)));
        cx.run_until_parked();

        state.update(cx, |state, cx| {
            state.set_text("replacement", cx);
            state.push_str(" text", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.text.as_str(), "replacement text");
            assert_eq!(state.source().as_str(), "replacement text");
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

    #[test]
    fn update_options_merge_keeps_latest_full_text() {
        let mut options = UpdateOptions {
            revision: 1,
            pending_text: "old".to_string(),
            append: true,
            markdown_extensions: Arc::default(),
            publish_result: true,
        };

        options.merge(UpdateOptions {
            revision: 2,
            pending_text: "new".to_string(),
            append: false,
            markdown_extensions: Arc::default(),
            publish_result: false,
        });
        options.merge(UpdateOptions {
            revision: 3,
            pending_text: " text".to_string(),
            append: true,
            markdown_extensions: Arc::default(),
            publish_result: true,
        });

        assert_eq!(options.revision, 3);
        assert_eq!(options.pending_text, "new text");
        assert!(!options.append);
        assert!(options.publish_result);
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
                markdown_extensions: Arc::default(),
                publish_result: true,
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
            publish_result: false,
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
            publish_result: false,
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
            publish_result: true,
        })
        .unwrap();
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut task_cx),
            Poll::Pending
        ));

        let parsed_update = rx_result.try_recv().expect("append parse result");
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
            state.push_str(" rose", cx);
        });
        cx.run_until_parked();

        state.read_with(cx, |state, _| {
            assert_eq!(state.source().as_str(), "$TSLA.US rose");
            let node::BlockNode::Custom(node) = &state.parsed_content.document.blocks[0] else {
                panic!("expected custom markdown node");
            };
            assert_eq!(node.name(), "ticker");
            assert_eq!(
                node.data::<String>().map(String::as_str),
                Some("TSLA.US rose")
            );
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
