//! A tab-owned, memory-only draft. It is a sibling of the terminal input
//! surface: typing, IME composition and Enter never reach the transport.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Textarea, TextareaState},
    v_flex,
};
use gpui_kit::*;

use super::{RemoteTerminalId, TerminalLifecycle, TerminalView};
use crate::app::{PasteTerminalDraft, ToggleTerminalCompose};
use crate::i18n::t;

pub(super) struct ComposePane {
    id: RemoteTerminalId,
    draft: Entity<TextareaState>,
    terminal: Entity<TerminalView>,
    _subscriptions: Vec<Subscription>,
}

impl ComposePane {
    pub(super) fn new(
        id: RemoteTerminalId,
        terminal: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(t!("terminal.compose.placeholder"))
        });
        let subscriptions = vec![
            cx.observe(&draft, |_, _, cx| cx.notify()),
            cx.observe(&terminal, |_, _, cx| cx.notify()),
        ];
        Self {
            id,
            draft,
            terminal,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn paste(&self, cx: &mut App) -> Result<(), SharedString> {
        let draft = self.draft.read(cx).value();
        self.terminal
            .update(cx, |terminal, cx| terminal.paste_draft(&draft, cx))
    }
}

impl Focusable for ComposePane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.draft.focus_handle(cx)
    }
}

impl Render for ComposePane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id;
        let running = self.terminal.read(cx).lifecycle(cx) == TerminalLifecycle::Running;
        let empty = self.draft.read(cx).value().trim().is_empty();
        v_flex()
            .id(("terminal-compose", id.0))
            .test_support()
            .occlude()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(div().flex_1().text_sm().child(t!("terminal.compose.title")))
                    .child(
                        Button::new(("paste-terminal-draft", id.0))
                            .small()
                            .label(t!("terminal.compose.paste"))
                            .disabled(!running || empty)
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(PasteTerminalDraft(id)), cx)
                            }),
                    )
                    .child(
                        Button::new(("hide-terminal-compose", id.0))
                            .small()
                            .ghost()
                            .label(t!("terminal.compose.hide"))
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(ToggleTerminalCompose(id)), cx)
                            }),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if running {
                        t!("terminal.compose.hint")
                    } else {
                        t!("terminal.compose.offline")
                    }),
            )
            .child(
                div()
                    .id(("terminal-compose-input", id.0))
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p_2()
                    .overflow_hidden()
                    .child(
                        Textarea::new(&self.draft)
                            .h(relative(1.))
                            .aria_label(t!("terminal.compose.title"))
                            .font_family(cx.theme().mono_font_family.clone()),
                    ),
            )
    }
}
