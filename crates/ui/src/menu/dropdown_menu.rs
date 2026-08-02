use std::rc::Rc;

use gpui::{
    Anchor, Context, DismissEvent, ElementId, Entity, Focusable, InteractiveElement, IntoElement,
    RenderOnce, SharedString, StyleRefinement, Styled, Window,
};

use crate::{Selectable, button::Button, menu::PopupMenu, popover::Popover};

/// A dropdown menu trait for buttons and other interactive elements
pub trait DropdownMenu: Styled + Selectable + InteractiveElement + IntoElement + 'static {
    /// Create a dropdown menu with the given items, anchored to the TopLeft corner
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        self.dropdown_menu_with_anchor(Anchor::TopLeft, f)
    }

    /// Create a dropdown menu with the given items, anchored to the given corner
    fn dropdown_menu_with_anchor(
        mut self,
        anchor: impl Into<Anchor>,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        let style = self.style().clone();
        let id = self.interactivity().element_id.clone();

        DropdownMenuPopover::new(id.unwrap_or(0.into()), anchor, self, f).trigger_style(style)
    }
}

impl DropdownMenu for Button {}

#[derive(IntoElement)]
pub struct DropdownMenuPopover<T: Selectable + IntoElement + 'static> {
    id: ElementId,
    style: StyleRefinement,
    anchor: Anchor,
    trigger: T,
    builder: Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>,
}

impl<T> DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn new(
        id: ElementId,
        anchor: impl Into<Anchor>,
        trigger: T,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        Self {
            id: SharedString::from(format!("dropdown-menu:{:?}", id)).into(),
            style: StyleRefinement::default(),
            anchor: anchor.into(),
            trigger,
            builder: Rc::new(builder),
        }
    }

    /// Set the anchor corner for the dropdown menu popover.
    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }

    /// Set the style refinement for the dropdown menu trigger.
    fn trigger_style(mut self, style: StyleRefinement) -> Self {
        self.style = style;
        self
    }
}

#[derive(Default)]
struct DropdownMenuState {
    menu: Option<Entity<PopupMenu>>,
}

impl<T> RenderOnce for DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn render(self, window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let builder = self.builder.clone();
        let menu_state =
            window.use_keyed_state(self.id.clone(), cx, |_, _| DropdownMenuState::default());

        Popover::new(SharedString::from(format!("popover:{}", self.id)))
            .appearance(false)
            .overlay_closable(false)
            .trigger(self.trigger)
            .trigger_style(self.style)
            .anchor(self.anchor)
            .content(move |_, window, cx| {
                // Here is special logic to only create the PopupMenu once and reuse it.
                // Because this `content` will called in every time render, so we need to store the menu
                // in state to avoid recreating at every render.
                //
                // And we also need to rebuild the menu when it is dismissed, to rebuild menu items
                // dynamically for support `dropdown_menu` method, so we listen for DismissEvent below.
                let menu = match menu_state.read(cx).menu.clone() {
                    Some(menu) => menu,
                    None => {
                        let builder = builder.clone();
                        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                            builder(menu, window, cx)
                        });
                        menu_state.update(cx, |state, _| {
                            state.menu = Some(menu.clone());
                        });
                        menu.focus_handle(cx).focus(window, cx);

                        // Listen for dismiss events from the PopupMenu to close the popover.
                        let popover_state = cx.entity();
                        window
                            .subscribe(&menu, cx, {
                                let menu_state = menu_state.clone();
                                move |_, _: &DismissEvent, window, cx| {
                                    popover_state.update(cx, |state, cx| {
                                        state.dismiss(window, cx);
                                    });
                                    menu_state.update(cx, |state, _| {
                                        state.menu = None;
                                    });
                                }
                            })
                            .detach();

                        menu.clone()
                    }
                };

                menu.clone()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Disableable as _, Root, menu::PopupMenuItem};
    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, KeyDownEvent, KeyUpEvent, Keystroke, Render,
        TestAppContext, VisualTestContext,
    };
    use std::{cell::Cell, rc::Rc};

    #[derive(Clone, Copy)]
    enum TriggerState {
        Enabled,
        Loading,
        Disabled,
    }

    struct DropdownKeyboardTest {
        state: TriggerState,
        button_clicks: Rc<Cell<usize>>,
        selections: Rc<Cell<usize>>,
    }

    impl Render for DropdownKeyboardTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let button_clicks = self.button_clicks.clone();
            let selections = self.selections.clone();
            Button::new("dropdown-trigger")
                .label("Open menu")
                .debug_selector(|| "dropdown-trigger".into())
                .loading(matches!(self.state, TriggerState::Loading))
                .disabled(matches!(self.state, TriggerState::Disabled))
                .on_click(move |_, _, _| button_clicks.set(button_clicks.get() + 1))
                .dropdown_menu(move |menu, _, _| {
                    let selections = selections.clone();
                    menu.item(PopupMenuItem::new("Select").on_click(move |_, _, _| {
                        selections.set(selections.get() + 1);
                    }))
                })
        }
    }

    fn setup(
        cx: &mut TestAppContext,
    ) -> (
        Entity<DropdownKeyboardTest>,
        &mut VisualTestContext,
        Rc<Cell<usize>>,
        Rc<Cell<usize>>,
    ) {
        cx.update(crate::init);
        let button_clicks = Rc::new(Cell::new(0));
        let selections = Rc::new(Cell::new(0));
        let content = cx.update(|cx| {
            cx.new(|_| DropdownKeyboardTest {
                state: TriggerState::Enabled,
                button_clicks: button_clicks.clone(),
                selections: selections.clone(),
            })
        });
        let (_, cx) = cx.add_window_view({
            let content = content.clone();
            move |window, cx| Root::new(content, window, cx)
        });
        (content, cx, button_clicks, selections)
    }

    fn draw_and_focus_trigger(cx: &mut VisualTestContext) -> FocusHandle {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.focus_next(cx);
            let focus = window
                .focused(cx)
                .expect("dropdown trigger should be focusable");
            window.draw(cx).clear(cx);
            focus
        })
    }

    fn activate_key(cx: &mut VisualTestContext, key: &str) {
        let keystroke = Keystroke::parse(key).expect("valid test keystroke");
        cx.simulate_event(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(KeyUpEvent { keystroke });
    }

    fn set_trigger_state(
        view: &Entity<DropdownKeyboardTest>,
        state: TriggerState,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.state = state;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
    }

    #[gpui::test]
    fn dropdown_keyboard_focus_and_dismiss_contract(cx: &mut TestAppContext) {
        let (_, cx, _, selections) = setup(cx);
        let trigger_focus = draw_and_focus_trigger(cx);

        activate_key(cx, "enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let menu_focus =
            cx.update(|window, cx| window.focused(cx).expect("open menu should receive focus"));
        assert_ne!(menu_focus, trigger_focus);

        cx.simulate_keystrokes("enter");
        assert_eq!(selections.get(), 0);
        cx.update(|window, cx| assert_eq!(window.focused(cx).as_ref(), Some(&menu_focus)));

        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| assert_eq!(window.focused(cx).as_ref(), Some(&trigger_focus)));

        activate_key(cx, "space");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("down enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));

        assert_eq!(selections.get(), 1);
        cx.update(|window, cx| assert_eq!(window.focused(cx).as_ref(), Some(&trigger_focus)));
    }

    #[gpui::test]
    fn inert_button_does_not_open_dropdown_or_click(cx: &mut TestAppContext) {
        let (view, cx, button_clicks, selections) = setup(cx);
        let trigger_focus = draw_and_focus_trigger(cx);

        set_trigger_state(&view, TriggerState::Loading, cx);
        let trigger_center = cx
            .debug_bounds("dropdown-trigger")
            .expect("trigger should be drawn")
            .center();
        cx.simulate_click(trigger_center, Default::default());
        cx.update(|window, cx| assert_eq!(window.focused(cx).as_ref(), Some(&trigger_focus)));

        activate_key(cx, "enter");
        cx.update(|window, cx| assert_eq!(window.focused(cx).as_ref(), Some(&trigger_focus)));
        assert_eq!(button_clicks.get(), 0);
        assert_eq!(selections.get(), 0);

        set_trigger_state(&view, TriggerState::Disabled, cx);
        cx.simulate_click(trigger_center, Default::default());
        activate_key(cx, "space");
        assert_eq!(button_clicks.get(), 0);
        assert_eq!(selections.get(), 0);
    }
}
