use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use gpui::{
    AppContext as _, Bounds, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _, Pixels, Point,
    Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _, TestAppContext,
    VisualTestContext, Window, div, point, prelude::FluentBuilder as _, px, size,
};

use super::{
    MarkdownExtensions, MarkdownNode, SelectionFormat, TextView, TextViewState, TextViewStyle,
    node::BlockNode,
};

struct WindowedTextRoot {
    text: Entity<TextViewState>,
    windowed: bool,
    scrollable: bool,
    max_lines: Option<usize>,
    before: Pixels,
    width: Pixels,
    font_size: Pixels,
    scroll: Option<ScrollHandle>,
    selection_format: SelectionFormat,
    table_actions_height: Option<Pixels>,
    extensions: Option<MarkdownExtensions>,
}

impl Render for WindowedTextRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("windowed-text-root")
            .w(self.width)
            .text_size(self.font_size)
            .child(crate::TextSelectionLayer)
            .child(
                div()
                    .id("windowed-text-viewport")
                    .when(self.scrollable, |this| this.h(px(400.)))
                    .when_some(self.scroll.as_ref(), |this, scroll| {
                        this.h(px(400.)).overflow_y_scroll().track_scroll(scroll)
                    })
                    .child(div().h(self.before))
                    .child(
                        TextView::new(&self.text)
                            .style(TextViewStyle::default())
                            .selectable(true)
                            .scrollable(self.scrollable)
                            .when_some(self.max_lines, |this, max_lines| this.max_lines(max_lines))
                            .selection_format(self.selection_format)
                            .when_some(self.table_actions_height, |this, height| {
                                this.table_actions(move |_, _, _| div().h(height))
                            })
                            .when_some(self.extensions.as_ref(), |this, extensions| {
                                this.markdown_extensions(extensions.clone())
                            })
                            .windowed(self.windowed),
                    ),
            )
    }
}

fn paragraphs(count: usize, lines: usize) -> String {
    (0..count)
        .map(|ix| {
            (0..lines)
                .map(|line| format!("paragraph {ix}, line {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn setup<'a>(
    cx: &'a mut TestAppContext,
    source: &str,
) -> (Entity<WindowedTextRoot>, &'a mut VisualTestContext) {
    cx.update(crate::init);
    cx.add_window_view(|_, cx| WindowedTextRoot {
        text: cx.new(|cx| TextViewState::markdown(source, cx)),
        windowed: true,
        scrollable: false,
        max_lines: None,
        before: px(0.),
        width: px(400.),
        font_size: px(14.),
        scroll: None,
        selection_format: SelectionFormat::Plain,
        table_actions_height: None,
        extensions: None,
    })
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
}

fn state(root: &Entity<WindowedTextRoot>, cx: &VisualTestContext) -> Entity<TextViewState> {
    root.read_with(cx, |root, _| root.text.clone())
}

fn height(text: &Entity<TextViewState>, cx: &VisualTestContext) -> Pixels {
    text.read_with(cx, |text, _| text.bounds().size.height)
}

fn materialized(text: &TextViewState, ix: usize) -> bool {
    let BlockNode::Paragraph(paragraph) = &text.parsed_content.document.blocks[ix] else {
        panic!("fixture block {ix} must be a paragraph");
    };
    !paragraph
        .state
        .lock()
        .expect("paragraph state")
        .text
        .is_empty()
}

fn visible_text_bounds(text: &TextViewState) -> Vec<Bounds<Pixels>> {
    text.selection_adapter
        .registered_text_bounds()
        .iter()
        .filter(|bounds| bounds.bottom() > px(0.) && bounds.top() < px(400.))
        .copied()
        .collect()
}

fn assert_matches_natural_height(root: &Entity<WindowedTextRoot>, cx: &mut VisualTestContext) {
    let text = state(root, cx);
    let windowed = height(&text, cx);
    root.update(cx, |root, cx| {
        root.windowed = false;
        cx.notify();
    });
    draw(cx);
    let natural = height(&text, cx);
    assert!(
        (windowed - natural).abs() <= px(1.),
        "measured windowed height {windowed:?} differs from natural height {natural:?}"
    );
}

fn scroll_to_block(
    text: &Entity<TextViewState>,
    scroll: &ScrollHandle,
    ix: usize,
    cx: &mut VisualTestContext,
) {
    for _ in 0..6 {
        let offset = text.read_with(cx, |text, _| {
            (text.block_heights.sum_range(0..ix) - px(100.)).max(px(0.))
        });
        cx.update(|window, cx| {
            scroll.set_offset(point(px(0.), -offset));
            window.refresh();
            let _ = window.draw(cx);
        });
    }
    assert!(text.read_with(cx, |text, _| materialized(text, ix)));
}

fn endpoint(
    text: &Entity<TextViewState>,
    ix: usize,
    end: bool,
    cx: &VisualTestContext,
) -> Point<Pixels> {
    text.read_with(cx, |text, _| {
        let top = text.bounds().top() + text.block_heights.sum_range(0..ix);
        let bottom = text.bounds().top() + text.block_heights.sum_range(0..ix + 1);
        let lines = visible_text_bounds(text)
            .into_iter()
            .filter(|bounds| bounds.top() >= top - px(0.5) && bounds.top() < bottom - px(0.5))
            .collect::<Vec<_>>();
        let line = if end { lines.last() } else { lines.first() }
            .expect("visible text line for selection endpoint");
        point(
            if end {
                line.right() + px(1.)
            } else {
                line.left() + px(0.1)
            },
            line.center().y,
        )
    })
}

fn copy_selection(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    });
    cx.dispatch_action(crate::input::Copy);
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .expect("copied text")
}

#[gpui::test]
fn effective_windowed_mode_notifies_observers_and_settles(cx: &mut TestAppContext) {
    let (root, cx) = setup(cx, &paragraphs(4, 2));
    cx.simulate_resize(size(px(800.), px(400.)));
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    assert!(text.read_with(cx, |text, _| text.is_layout_complete()));
    root.update(cx, |root, cx| {
        root.windowed = false;
        cx.notify();
    });
    draw(cx);
    draw(cx);
    assert!(!text.read_with(cx, |text, _| text.is_windowed()));

    let observed = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        cx.observe(&text, {
            let observed = observed.clone();
            move |text, cx| {
                let mut modes = observed.borrow_mut();
                modes.push(text.read(cx).is_windowed());
                assert!(
                    modes.len() < 64,
                    "effective mode changes caused a notify loop"
                );
            }
        })
    });
    for (scrollable, max_lines, expected) in [
        (false, None, true),
        (true, None, false),
        (false, None, true),
        (false, Some(1), false),
        (false, None, true),
    ] {
        observed.borrow_mut().clear();
        root.update(cx, |root, cx| {
            root.windowed = true;
            root.scrollable = scrollable;
            root.max_lines = max_lines;
            cx.notify();
        });
        draw(cx);
        draw(cx);
        assert_eq!(text.read_with(cx, |text, _| text.is_windowed()), expected);
        assert!(
            !observed.borrow().is_empty(),
            "an effective mode change must notify observers"
        );
        assert!(observed.borrow().iter().all(|mode| *mode == expected));

        let settled = observed.borrow().len();
        for _ in 0..4 {
            root.update(cx, |_, cx| cx.notify());
            draw(cx);
        }
        assert_eq!(
            observed.borrow().len(),
            settled,
            "equivalent frames must not notify the text state"
        );
    }
}

#[gpui::test]
fn layout_completeness_changes_notify_observers_after_paint(cx: &mut TestAppContext) {
    let (root, cx) = setup(cx, &paragraphs(120, 2));
    cx.simulate_resize(size(px(800.), px(400.)));
    cx.run_until_parked();
    let text = state(&root, cx);
    text.read_with(cx, |text, _| {
        assert!(text.is_windowed());
        assert!(!text.is_layout_complete());
    });

    let observed = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        cx.observe(&text, {
            let observed = observed.clone();
            move |text, cx| {
                let mut states = observed.borrow_mut();
                states.push(text.read(cx).is_layout_complete());
                assert!(states.len() < 64, "layout notifications failed to settle");
            }
        })
    });

    cx.simulate_resize(size(px(800.), px(20000.)));
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().last().copied(),
        Some(true),
        "measuring all visible blocks must publish layout completion"
    );

    cx.simulate_resize(size(px(800.), px(400.)));
    cx.run_until_parked();
    observed.borrow_mut().clear();
    root.update(cx, |root, cx| {
        root.font_size *= 2.;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().last().copied(),
        Some(false),
        "reflow must publish that offscreen measurements are provisional"
    );

    observed.borrow_mut().clear();
    cx.simulate_resize(size(px(800.), px(20000.)));
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().last().copied(),
        Some(true),
        "remeasuring after reflow must publish completion again"
    );
}

#[gpui::test]
fn synchronous_reparse_publishes_incomplete_layout_when_moved_offscreen(cx: &mut TestAppContext) {
    const COUNT: usize = 4;
    let source = paragraphs(COUNT, 2);
    let (root, cx) = setup(cx, &source);
    cx.simulate_resize(size(px(800.), px(400.)));
    cx.run_until_parked();
    let text = state(&root, cx);
    text.read_with(cx, |text, _| {
        assert!(text.is_windowed());
        assert!(text.is_layout_complete());
        assert_eq!(text.block_count(), COUNT);
    });

    let observed = Rc::new(RefCell::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        cx.observe(&text, {
            let observed = observed.clone();
            move |text, cx| {
                let text = text.read(cx);
                let mut snapshots = observed.borrow_mut();
                snapshots.push((text.is_layout_complete(), text.block_count()));
                assert!(
                    snapshots.len() < 64,
                    "parser configuration notifications failed to settle"
                );
            }
        })
    });
    let extensions = MarkdownExtensions::default().prepare_source(|source| source.to_string());
    root.update(cx, |root, cx| {
        root.before = px(1000.);
        root.extensions = Some(extensions);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        observed.borrow().last().copied(),
        Some((false, COUNT)),
        "synchronous reparse must publish invalidated measurements without a visible repaint"
    );
    text.read_with(cx, |text, _| {
        assert!(text.bounds().top() > px(400.));
        assert!(!text.is_layout_complete());
        assert_eq!(text.source().as_ref(), source);
    });

    let settled = observed.borrow().len();
    for _ in 0..4 {
        root.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }
    assert_eq!(
        observed.borrow().len(),
        settled,
        "an equivalent parser configuration must not notify again"
    );
}

#[gpui::test]
fn offscreen_document_retains_total_height(cx: &mut TestAppContext) {
    let (root, cx) = setup(cx, &paragraphs(400, 1));
    cx.simulate_resize(size(px(800.), px(400.)));
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    let initial = height(&text, cx);
    assert!(initial > px(1000.));

    root.update(cx, |root, cx| {
        root.before = px(1000.);
        cx.notify();
    });
    for _ in 0..6 {
        draw(cx);
        let offscreen = height(&text, cx);
        assert!(
            (offscreen - initial).abs() <= px(1.),
            "moving outside the viewport changed document height from {initial:?} to {offscreen:?}"
        );
    }
}

#[gpui::test]
fn zero_height_custom_block_can_grow_on_refresh(cx: &mut TestAppContext) {
    let requested_height = Arc::new(AtomicUsize::new(0));
    let extensions = MarkdownExtensions::default()
        .block_parser(|node, _| {
            matches!(node, markdown::mdast::Node::Paragraph(_))
                .then(|| MarkdownNode::new("growing-block", ()).text("dynamic"))
        })
        .block_renderer("growing-block", {
            let requested_height = requested_height.clone();
            move |_, _, _| {
                div()
                    .debug_selector(|| "growing-custom-block".into())
                    .h(px(requested_height.load(Ordering::Relaxed) as f32))
            }
        });
    let (root, cx) = setup(cx, "dynamic");
    cx.simulate_resize(size(px(800.), px(400.)));
    root.update(cx, |root, cx| {
        root.before = px(100.);
        root.extensions = Some(extensions);
        cx.notify();
    });
    cx.run_until_parked();
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    assert_eq!(height(&text, cx), px(0.));

    requested_height.store(160, Ordering::Relaxed);
    cx.update(|window, _| window.refresh());
    draw(cx);
    draw(cx);
    assert_eq!(height(&text, cx), px(160.));
    assert_eq!(
        cx.debug_bounds("growing-custom-block")
            .expect("grown custom block must paint")
            .size
            .height,
        px(160.)
    );
    assert_matches_natural_height(&root, cx);
}

#[gpui::test]
fn single_scroll_jump_materializes_current_frame(cx: &mut TestAppContext) {
    let (root, cx) = setup(cx, &paragraphs(1000, 1));
    cx.simulate_resize(size(px(800.), px(400.)));
    let scroll = ScrollHandle::new();
    root.update(cx, |root, cx| {
        root.scroll = Some(scroll.clone());
        cx.notify();
    });
    draw(cx);
    draw(cx);
    let text = state(&root, cx);

    // Capture the first draw inside the update, before queued effects can
    // produce a corrective frame.
    cx.update(|window, cx| {
        scroll.set_offset(point(px(0.), px(-8000.)));
        window.refresh();
        let _ = window.draw(cx);
        let text = text.read(cx);
        let origin = text.bounds().origin.y;
        assert!(origin < px(-7000.), "fixture did not scroll: {origin:?}");
        let required = text
            .block_heights
            .block_ix_at_y(-origin + px(200.))
            .expect("block at viewport center");
        assert!(required > 100, "fixture must jump beyond initial overdraw");
        assert!(
            materialized(text, required),
            "first scroll frame omitted block {required} at the viewport center"
        );
        assert!(
            !visible_text_bounds(text).is_empty(),
            "first scroll frame has no visible text"
        );
    });
}

#[gpui::test]
fn inherited_font_change_invalidates_offscreen_measurements(cx: &mut TestAppContext) {
    const COUNT: usize = 120;
    let (root, cx) = setup(cx, &paragraphs(COUNT, 2));
    cx.simulate_resize(size(px(800.), px(20000.)));
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    assert_eq!(
        text.read_with(cx, |text, _| text.block_heights.measured_count()),
        COUNT
    );

    cx.simulate_resize(size(px(800.), px(400.)));
    root.update(cx, |root, cx| {
        root.font_size = px(28.);
        cx.notify();
    });
    draw(cx);
    draw(cx);
    let measured = text.read_with(cx, |text, _| text.block_heights.measured_count());
    assert!(
        measured > 0 && measured < COUNT,
        "font change must remeasure visible blocks and discard offscreen measurements, got {measured}"
    );

    cx.simulate_resize(size(px(800.), px(20000.)));
    draw(cx);
    draw(cx);
    assert_matches_natural_height(&root, cx);
}

#[gpui::test]
fn rem_change_invalidates_offscreen_measurements(cx: &mut TestAppContext) {
    const COUNT: usize = 120;
    let (root, cx) = setup(cx, &paragraphs(COUNT, 2));
    cx.simulate_resize(size(px(800.), px(20000.)));
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    assert_eq!(
        text.read_with(cx, |text, _| text.block_heights.measured_count()),
        COUNT
    );

    cx.simulate_resize(size(px(800.), px(400.)));
    cx.update(|window, _| {
        window.set_rem_size(px(32.));
        window.refresh();
    });
    draw(cx);
    draw(cx);
    let measured = text.read_with(cx, |text, _| text.block_heights.measured_count());
    assert!(
        measured > 0 && measured < COUNT,
        "rem change must remeasure visible blocks and discard offscreen measurements, got {measured}"
    );

    cx.simulate_resize(size(px(800.), px(30000.)));
    draw(cx);
    draw(cx);
    assert_matches_natural_height(&root, cx);
}

#[gpui::test]
fn semantic_mono_size_change_invalidates_offscreen_code_measurements(cx: &mut TestAppContext) {
    const COUNT: usize = 120;
    let source = (0..COUNT)
        .map(|ix| format!("```text\nfn block_{ix}() {{\n    work();\n}}\n```"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let (root, cx) = setup(cx, &source);
    cx.simulate_resize(size(px(800.), px(40000.)));
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    let before = height(&text, cx);
    text.read_with(cx, |text, _| {
        assert_eq!(text.block_heights.measured_count(), COUNT);
        assert!(text.is_layout_complete());
    });

    cx.simulate_resize(size(px(800.), px(400.)));
    cx.update(|window, cx| {
        crate::Theme::global_mut(cx).tokens.typography.mono_md.size *= 2.;
        window.refresh();
    });
    draw(cx);
    draw(cx);
    text.read_with(cx, |text, _| {
        let measured = text.block_heights.measured_count();
        assert!(
            measured > 0 && measured < COUNT,
            "changing only semantic code size retained {measured} old measurements"
        );
        assert!(!text.is_layout_complete());
    });

    cx.simulate_resize(size(px(800.), px(50000.)));
    draw(cx);
    draw(cx);
    assert!(height(&text, cx) > before + px(1.));
    assert_matches_natural_height(&root, cx);
}

#[gpui::test]
fn table_actions_height_change_invalidates_offscreen_measurements_and_settles(
    cx: &mut TestAppContext,
) {
    const COUNT: usize = 120;
    let source = (0..COUNT)
        .map(|ix| format!("| Name | Value |\n| --- | --- |\n| row {ix} | value |"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let (root, cx) = setup(cx, &source);
    let text = state(&root, cx);
    let notifications = Rc::new(Cell::new(0));
    let _subscription = cx.update(|_, cx| {
        cx.observe(&text, {
            let notifications = notifications.clone();
            move |_, _| {
                let count = notifications.get() + 1;
                notifications.set(count);
                assert!(count < 64, "equivalent table actions caused a notify loop");
            }
        })
    });
    root.update(cx, |root, cx| {
        root.table_actions_height = Some(px(20.));
        cx.notify();
    });
    cx.simulate_resize(size(px(800.), px(40000.)));
    draw(cx);
    draw(cx);
    let before = height(&text, cx);
    text.read_with(cx, |text, _| {
        assert_eq!(text.block_heights.measured_count(), COUNT);
        assert!(text.is_layout_complete());
    });

    cx.simulate_resize(size(px(800.), px(400.)));
    root.update(cx, |root, cx| {
        root.table_actions_height = Some(px(100.));
        cx.notify();
    });
    draw(cx);
    draw(cx);
    text.read_with(cx, |text, _| {
        let measured = text.block_heights.measured_count();
        assert!(
            measured > 0 && measured < COUNT,
            "changing table actions retained {measured} old measurements"
        );
        assert!(!text.is_layout_complete());
    });

    for _ in 0..4 {
        root.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        draw(cx);
    }
    let settled = notifications.get();
    draw(cx);
    draw(cx);
    assert_eq!(
        notifications.get(),
        settled,
        "equivalent table actions must settle after a root render"
    );

    cx.simulate_resize(size(px(800.), px(40000.)));
    draw(cx);
    draw(cx);
    assert!(height(&text, cx) > before + px(1.));
    assert_matches_natural_height(&root, cx);
}

#[gpui::test]
fn reenable_after_replacement_discards_previous_document_measurements(cx: &mut TestAppContext) {
    const COUNT: usize = 120;
    let replacements = [
        paragraphs(COUNT, 1),
        vec![format!("replacement {}", "word ".repeat(60)); COUNT].join("\n\n"),
    ];
    for replacement in replacements {
        let (root, cx) = setup(cx, &paragraphs(COUNT, 3));
        cx.simulate_resize(size(px(800.), px(20000.)));
        draw(cx);
        draw(cx);
        let text = state(&root, cx);
        assert_eq!(
            text.read_with(cx, |text, _| text.block_heights.measured_count()),
            COUNT
        );

        root.update(cx, |root, cx| {
            root.windowed = false;
            cx.notify();
        });
        text.update(cx, |text, cx| text.set_text(&replacement, cx));
        cx.run_until_parked();
        cx.simulate_resize(size(px(800.), px(400.)));
        root.update(cx, |root, cx| {
            root.windowed = true;
            cx.notify();
        });
        draw(cx);
        draw(cx);
        let measured = text.read_with(cx, |text, _| text.block_heights.measured_count());
        assert!(
            measured < COUNT,
            "replacement reused every measurement from the previous document"
        );
        text.read_with(cx, |text, _| {
            assert_eq!(text.source().as_ref(), replacement);
            assert_eq!(text.block_count(), COUNT);
            assert_eq!(text.block_heights.len(), COUNT);
        });
    }
}

#[gpui::test]
fn reference_definition_append_invalidates_semantically_changed_blocks(cx: &mut TestAppContext) {
    const COUNT: usize = 120;
    let label = "long reference label ".repeat(12);
    let source = vec![format!("[shown][{label}]"); COUNT].join("\n\n");
    let (root, cx) = setup(cx, &source);
    cx.simulate_resize(size(px(800.), px(50000.)));
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    assert_eq!(
        text.read_with(cx, |text, _| text.block_heights.measured_count()),
        COUNT
    );
    let before = text.read_with(cx, |text, _| {
        let block = &text.parsed_content.document.blocks[100];
        (block.span(), block.text())
    });

    cx.simulate_resize(size(px(800.), px(400.)));
    text.update(cx, |text, cx| {
        text.push_str(&format!("\n\n[{label}]: https://example.test/\n"), cx);
    });
    cx.run_until_parked();
    draw(cx);
    draw(cx);
    text.read_with(cx, |text, _| {
        let block = &text.parsed_content.document.blocks[100];
        assert_eq!(before.0, block.span());
        assert_ne!(before.1, block.text());
        assert_eq!(block.text().trim(), "shown");
        assert!(
            text.block_heights.measured_count() < COUNT,
            "reference resolution changed block contents but retained every measurement"
        );
    });
}

#[gpui::test]
fn width_change_that_reflows_text_invalidates_offscreen_measurements(cx: &mut TestAppContext) {
    const COUNT: usize = 120;
    let (root, cx) = setup(cx, "probe");
    let text = state(&root, cx);
    root.update(cx, |root, cx| {
        root.windowed = false;
        cx.notify();
    });
    draw(cx);
    let one_line = height(&text, cx);
    let mut candidate = None;
    for words in 20..100 {
        let source = "word ".repeat(words);
        text.update(cx, |text, cx| text.set_text(&source, cx));
        root.update(cx, |root, cx| {
            root.width = px(671.);
            cx.notify();
        });
        draw(cx);
        let wide = height(&text, cx);
        root.update(cx, |root, cx| {
            root.width = px(608.);
            cx.notify();
        });
        draw(cx);
        let narrow = height(&text, cx);
        if wide > one_line + px(1.) && narrow > wide + px(1.) {
            candidate = Some(source);
            break;
        }
    }
    let source = candidate.expect("paragraph that reflows between 671px and 608px");
    text.update(cx, |text, cx| {
        text.set_text(&vec![source; COUNT].join("\n\n"), cx);
    });
    cx.run_until_parked();
    root.update(cx, |root, cx| {
        root.width = px(671.);
        root.windowed = true;
        cx.notify();
    });
    cx.simulate_resize(size(px(800.), px(20000.)));
    draw(cx);
    draw(cx);
    assert_eq!(
        text.read_with(cx, |text, _| text.block_heights.measured_count()),
        COUNT
    );

    cx.simulate_resize(size(px(800.), px(400.)));
    root.update(cx, |root, cx| {
        root.width = px(608.);
        cx.notify();
    });
    draw(cx);
    draw(cx);
    let measured = text.read_with(cx, |text, _| text.block_heights.measured_count());
    assert!(
        measured > 0 && measured < COUNT,
        "a width change with real reflow retained all {measured} old measurements"
    );

    cx.simulate_resize(size(px(800.), px(20000.)));
    draw(cx);
    draw(cx);
    assert_matches_natural_height(&root, cx);
}

fn drag_across_unmaterialized_blocks(cx: &mut TestAppContext, reverse: bool) {
    const FIRST: usize = 2;
    const MIDDLE: usize = 100;
    const LAST: usize = 200;
    let source = paragraphs(300, 2);
    let expected = source
        .split("\n\n")
        .skip(FIRST)
        .take(LAST - FIRST + 1)
        .collect::<Vec<_>>()
        .join("\n");
    let (root, cx) = setup(cx, &source);
    cx.simulate_resize(size(px(800.), px(400.)));
    let scroll = ScrollHandle::new();
    root.update(cx, |root, cx| {
        root.scroll = Some(scroll.clone());
        cx.notify();
    });
    let text = state(&root, cx);
    let (anchor_ix, cursor_ix) = if reverse {
        (LAST, FIRST)
    } else {
        (FIRST, LAST)
    };
    scroll_to_block(&text, &scroll, anchor_ix, cx);
    let anchor = endpoint(&text, anchor_ix, reverse, cx);
    cx.simulate_mouse_down(anchor, MouseButton::Left, Modifiers::default());
    scroll_to_block(&text, &scroll, cursor_ix, cx);
    let cursor = endpoint(&text, cursor_ix, !reverse, cx);
    cx.simulate_mouse_move(cursor, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(cursor, MouseButton::Left, Modifiers::default());
    draw(cx);

    assert!(
        !text.read_with(cx, |text, _| materialized(text, MIDDLE)),
        "the drag fixture must leave its middle block unmaterialized"
    );
    let copied = copy_selection(cx);
    assert_eq!(copied.lines().count(), expected.lines().count());
    for (ix, (actual, expected)) in copied.lines().zip(expected.lines()).enumerate() {
        assert_eq!(actual, expected, "copied line {ix}");
    }

    let before = height(&text, cx);
    scroll_to_block(&text, &scroll, MIDDLE, cx);
    draw(cx);
    assert!(
        (height(&text, cx) - before).abs() > px(1.),
        "visiting the middle must replace provisional heights with measurements"
    );
    assert_eq!(
        copy_selection(cx),
        copied,
        "a completed selection changed when intervening blocks were measured"
    );
}

#[gpui::test]
fn forward_drag_copies_unmaterialized_blocks_and_survives_convergence(cx: &mut TestAppContext) {
    drag_across_unmaterialized_blocks(cx, false);
}

#[gpui::test]
fn reverse_drag_copies_unmaterialized_blocks_and_survives_convergence(cx: &mut TestAppContext) {
    drag_across_unmaterialized_blocks(cx, true);
}

#[gpui::test]
fn reverse_drag_retains_partial_anchor_before_the_first_mouse_move(cx: &mut TestAppContext) {
    const UPPER: usize = 100;
    const LOWER: usize = 200;
    const PARTIAL: &str = "xxxxxxxxxx";
    let blocks = (0..300)
        .map(|ix| format!("{PARTIAL}{PARTIAL}\ncontinuation {ix}"))
        .collect::<Vec<_>>();
    let (root, cx) = setup(cx, &blocks.join("\n\n"));
    cx.simulate_resize(size(px(800.), px(400.)));
    let scroll = ScrollHandle::new();
    root.update(cx, |root, cx| {
        root.scroll = Some(scroll.clone());
        cx.notify();
    });
    let text = state(&root, cx);
    scroll_to_block(&text, &scroll, LOWER, cx);
    let first_line = endpoint(&text, LOWER, false, cx);
    let (anchor, prefix_before) = text.read_with(cx, |text, _| {
        assert!(!materialized(text, UPPER));
        let line = visible_text_bounds(text)
            .into_iter()
            .find(|bounds| (bounds.center().y - first_line.y).abs() < px(0.5))
            .expect("lower endpoint's first line");
        // Identical glyphs put the center at the same character boundary
        // across platform fonts, halfway through the paragraph's first line.
        (line.center(), text.block_heights.sum_range(0..LOWER))
    });
    cx.simulate_mouse_down(anchor, MouseButton::Left, Modifiers::default());

    scroll_to_block(&text, &scroll, UPPER, cx);
    text.read_with(cx, |text, _| {
        assert!(text.block_heights.sum_range(0..LOWER) > prefix_before + px(1.));
    });
    let cursor = endpoint(&text, UPPER, false, cx);
    cx.simulate_mouse_move(cursor, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(cursor, MouseButton::Left, Modifiers::default());

    scroll_to_block(&text, &scroll, LOWER, cx);
    text.read_with(cx, |text, _| {
        assert!(!materialized(text, (UPPER + LOWER) / 2));
        assert_eq!(
            text.parsed_content.document.blocks[LOWER]
                .selected_text(SelectionFormat::Plain)
                .trim_end_matches('\n'),
            PARTIAL,
            "the lower endpoint must paint only the original partial selection"
        );
    });
    let expected = format!("{}\n{PARTIAL}", blocks[UPPER..LOWER].join("\n"));
    let copied = copy_selection(cx);
    assert_eq!(copied.lines().count(), expected.lines().count());
    for (ix, (actual, expected)) in copied.lines().zip(expected.lines()).enumerate() {
        assert_eq!(actual, expected, "copied line {ix}");
    }
}

#[gpui::test]
fn double_click_selection_stays_on_its_block_after_prefix_convergence(cx: &mut TestAppContext) {
    const TARGET: usize = 200;
    const MIDDLE: usize = 100;
    let (root, cx) = setup(cx, &paragraphs(300, 2));
    cx.simulate_resize(size(px(800.), px(400.)));
    let scroll = ScrollHandle::new();
    root.update(cx, |root, cx| {
        root.scroll = Some(scroll.clone());
        cx.notify();
    });
    let text = state(&root, cx);
    scroll_to_block(&text, &scroll, TARGET, cx);
    let position = endpoint(&text, TARGET, false, cx);
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position,
        modifiers: Modifiers::default(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position,
        modifiers: Modifiers::default(),
        click_count: 2,
    });
    draw(cx);
    assert_eq!(copy_selection(cx), "paragraph");
    let highlighted = text.read_with(cx, |text, _| {
        assert!(!materialized(text, MIDDLE));
        text.parsed_content.document.blocks[TARGET].selected_text(SelectionFormat::Plain)
    });
    assert_eq!(highlighted.trim(), "paragraph");

    let before = height(&text, cx);
    scroll_to_block(&text, &scroll, MIDDLE, cx);
    assert!(height(&text, cx) > before + px(1.));
    scroll_to_block(&text, &scroll, TARGET, cx);
    assert_eq!(copy_selection(cx), "paragraph");
    text.read_with(cx, |text, _| {
        assert_eq!(
            text.parsed_content.document.blocks[TARGET].selected_text(SelectionFormat::Plain),
            highlighted,
            "the highlight moved away from the double-clicked block"
        );
    });
}

#[gpui::test]
fn select_all_copies_exact_source_with_unmaterialized_blocks(cx: &mut TestAppContext) {
    const COUNT: usize = 400;
    let source = (0..COUNT)
        .map(|ix| format!("paragraph {ix}: **bold** and _italic_\nsecond line"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let (root, cx) = setup(cx, &source);
    cx.simulate_resize(size(px(800.), px(400.)));
    root.update(cx, |root, cx| {
        root.selection_format = SelectionFormat::Source;
        cx.notify();
    });
    draw(cx);
    let text = state(&root, cx);
    text.read_with(cx, |text, _| {
        assert!(text.block_heights.measured_count() < COUNT);
        assert!(!materialized(text, COUNT / 2));
    });

    let focus_handle = text.read_with(cx, |text, _| text.focus_handle().clone());
    cx.update(|window, cx| focus_handle.focus(window, cx));
    text.update(cx, |text, cx| text.select_all(cx));
    assert_eq!(copy_selection(cx), source);
}

#[gpui::test]
fn streaming_append_preserves_prefix_heights_and_converges_to_natural_layout(
    cx: &mut TestAppContext,
) {
    const COUNT: usize = 1000;
    const BATCH: usize = 100;
    const INITIAL: usize = 10;
    let source = paragraphs(COUNT, 2);
    let blocks = source.split("\n\n").collect::<Vec<_>>();
    let (root, cx) = setup(cx, &blocks[..INITIAL].join("\n\n"));
    cx.simulate_resize(size(px(800.), px(400.)));
    let scroll = ScrollHandle::new();
    root.update(cx, |root, cx| {
        root.scroll = Some(scroll.clone());
        cx.notify();
    });
    draw(cx);
    draw(cx);
    let text = state(&root, cx);
    let prefix_height = text.read_with(cx, |text, _| {
        assert_eq!(text.block_heights.measured_count(), INITIAL);
        text.block_heights.sum_range(0..5)
    });
    let mut previous_height = height(&text, cx);
    let mut previous_thumb = scroll.bounds().size.height / previous_height;

    for first in (INITIAL..COUNT).step_by(BATCH) {
        let end = (first + BATCH).min(COUNT);
        text.update(cx, |text, cx| {
            text.push_str(&format!("\n\n{}", blocks[first..end].join("\n\n")), cx);
        });
        cx.run_until_parked();
        draw(cx);
        draw(cx);
        text.read_with(cx, |text, _| {
            assert_eq!(text.block_count(), end);
            assert_eq!(text.block_heights.len(), end);
            assert_eq!(text.block_heights.sum_range(0..5), prefix_height);
        });
        let current_height = height(&text, cx);
        let thumb = scroll.bounds().size.height / current_height;
        assert!(current_height > previous_height);
        assert!(thumb < previous_thumb);
        assert!(
            (scroll.max_offset().y + scroll.bounds().size.height - current_height).abs() <= px(1.),
            "outer scroll extent does not match streamed document height"
        );
        previous_height = current_height;
        previous_thumb = thumb;
    }

    text.read_with(cx, |text, _| {
        assert_eq!(text.source().as_ref(), source);
        assert!(text.block_heights.measured_count() < COUNT);
        assert!(!text.is_layout_complete());
    });
    for ix in (0..COUNT).step_by(20) {
        scroll_to_block(&text, &scroll, ix, cx);
    }
    scroll_to_block(&text, &scroll, COUNT - 1, cx);
    draw(cx);
    assert_eq!(
        text.read_with(cx, |text, _| text.block_heights.measured_count()),
        COUNT,
        "visiting the whole document must converge every block measurement"
    );
    assert!(text.read_with(cx, |text, _| text.is_layout_complete()));
    assert_matches_natural_height(&root, cx);
}
