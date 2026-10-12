use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu},
    tooltip::Tooltip,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::ToggleHostPanel;
use crate::host::ConnectionState;
use crate::i18n::t;

type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// A dock tab title: a leading mark, the label and, for closable panels, a
/// close button that dispatches the panel's close action. A panel may also
/// give the title a context menu, which opens on a right click anywhere on it.
///
/// Closing goes through an action rather than `TabGroup::close_panel` on
/// purpose: the group refuses to close the last panel of a region, while the
/// workspace wants every tab closable (an empty center shows the recent
/// hosts instead) and removes the panel from the dock itself.
///
/// Double-clicking the title shows or hides the host sidebar, the way a
/// double click on an editor tab gives it the room elsewhere.
///
/// The context menu and the double click hang on the title rather than on
/// the dock's `Tab`, which the application does not render. The tab's own
/// horizontal padding is therefore outside them; clicks there only select.
#[derive(IntoElement)]
pub struct ClosableTabTitle {
    id: ElementId,
    leading: AnyElement,
    label: SharedString,
    close: Option<(ElementId, Box<dyn Action>)>,
    menu: Option<MenuBuilder>,
    modified: bool,
    connection_state: Option<ConnectionState>,
}

impl ClosableTabTitle {
    /// `id` identifies the tab and must be unique among the tabs of a bar:
    /// the context menu keeps its open state under it.
    pub fn new(
        id: impl Into<ElementId>,
        leading: impl IntoElement,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            leading: leading.into_any_element(),
            label: label.into(),
            close: None,
            menu: None,
            modified: false,
            connection_state: None,
        }
    }

    /// Show this tab's own connection state, never its host's aggregate state.
    /// Tabs that do not own a remote connection leave this unset.
    pub fn connection_state(mut self, state: ConnectionState) -> Self {
        self.connection_state = Some(state);
        self
    }

    /// Mark the tab as holding changes not saved yet: a dot after the label,
    /// as editors do.
    pub fn modified(mut self, modified: bool) -> Self {
        self.modified = modified;
        self
    }

    /// Show a close button that dispatches `action` on the focused path.
    pub fn closable(mut self, id: impl Into<ElementId>, action: Box<dyn Action>) -> Self {
        self.close = Some((id.into(), action));
        self
    }

    /// Open a menu built by `builder` on a right click. The builder runs when
    /// the menu opens, not while the tab renders.
    pub fn context_menu(
        mut self,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.menu = Some(Rc::new(builder));
        self
    }
}

impl RenderOnce for ClosableTabTitle {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        // The badge must not change the mark's layout box or baseline. Put
        // its center at the circular mark's lower-right corner: the two
        // circles remain separate without shifting the icon or title text.
        let leading = match self.connection_state {
            Some(state) => {
                let color = match state {
                    ConnectionState::Connected => cx.theme().success,
                    ConnectionState::Connecting => cx.theme().warning,
                    ConnectionState::Disconnected => cx.theme().danger,
                };
                let label = state.label();
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(self.leading)
                    .child(
                        div()
                            .id("connection-status")
                            .test_support()
                            .role(Role::Status)
                            .aria_label(label.clone())
                            .absolute()
                            // Half the size_1p5 diameter, in the same rem
                            // units, centers the dot on the mark's corner.
                            .right(-rems(0.1875))
                            .bottom(-rems(0.1875))
                            .size_1p5()
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(color)
                            .tooltip(move |window, cx| {
                                Tooltip::new(label.clone()).build(window, cx)
                            }),
                    )
                    .into_any_element()
            }
            None => self.leading,
        };
        let row = h_flex()
            .id("title")
            .gap_1()
            .child(leading)
            .child(self.label.clone())
            .when(self.modified, |row| {
                row.child(
                    div()
                        .id("modified")
                        .test_support()
                        .aria_label(t!("shared.tab.unsaved"))
                        .size_1p5()
                        .rounded_full()
                        .bg(cx.theme().muted_foreground),
                )
            })
            .when_some(self.close, |row, (id, action)| {
                row.child(
                    Button::new(id)
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(t!("common.close"))
                        .on_click(move |_, window, cx| {
                            // Closing must not also select or drag the tab.
                            cx.stop_propagation();
                            window.dispatch_action(action.boxed_clone(), cx);
                        }),
                )
            });
        div()
            .id(self.id)
            // The first click selects the tab as usual.
            .on_click(|event, window, cx| {
                if event.click_count() == 2 {
                    window.dispatch_action(Box::new(ToggleHostPanel), cx);
                }
            })
            .test_support()
            .aria_label(self.label)
            .child(match self.menu {
                Some(menu) => row
                    .context_menu(move |popup, window, cx| menu(popup, window, cx))
                    .into_any_element(),
                None => row.into_any_element(),
            })
    }
}
