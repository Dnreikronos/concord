//! Root view and basic three-column layout (server rail · sidebar · content).
//!
//! This is the structural skeleton: it lays out the panes, wires the server
//! rail to the navigation state, and renders placeholder content per view.
//! Real channel lists, message history, and inputs land in later work.

use gpui::*;
use gpui_component::{h_flex, v_flex};

use crate::auth::Session;
use crate::ui::auth_view::{AuthEvent, AuthView};
use crate::ui::nav::{NavState, View};
use crate::ui::theme::{color, font, space};

/// Which top-level screen the app is showing.
enum Screen {
    /// The login / register card, shown until the user authenticates.
    Auth,
    /// The main three-column app, shown once a session exists.
    Main,
}

/// The application's root view. It gates the main UI behind authentication:
/// it starts on the [`AuthView`] and, on a successful login, stores the
/// [`Session`] and swaps to the main three-column layout.
pub struct ConcordApp {
    screen: Screen,
    auth: Entity<AuthView>,
    nav: NavState,
    session: Option<Session>,
    _auth_subscription: Subscription,
}

impl ConcordApp {
    /// Construct the root view, starting on the auth screen.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let auth = cx.new(|cx| AuthView::new(window, cx));
        let auth_subscription = cx.subscribe(&auth, Self::on_auth_event);
        Self {
            screen: Screen::Auth,
            auth,
            nav: NavState::new(),
            session: None,
            _auth_subscription: auth_subscription,
        }
    }

    /// React to the auth view: store the session and reveal the main app.
    fn on_auth_event(
        &mut self,
        _auth: Entity<AuthView>,
        event: &AuthEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AuthEvent::Authenticated(session) => {
                self.session = Some(session.clone());
                self.screen = Screen::Main;
                cx.notify();
            }
        }
    }

    /// Leftmost rail: one circular button per top-level [`View`].
    fn server_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(space::SERVER_RAIL))
            .h_full()
            .flex_shrink_0()
            .bg(color::server_rail())
            .py(px(space::MD))
            .gap(px(space::SM))
            .items_center()
            .child(self.rail_button(View::Servers, cx))
            .child(self.rail_button(View::DirectMessages, cx))
            .child(div().flex_1())
            .child(self.rail_button(View::Settings, cx))
    }

    /// A single rail button. Clicking it activates `view`.
    fn rail_button(&self, view: View, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.nav.is_active(view);
        div()
            .id(view.glyph())
            .size(px(space::RAIL_BUTTON))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(space::LG))
            .text_size(px(font::SM))
            .bg(if active {
                color::accent()
            } else {
                color::sidebar()
            })
            .text_color(if active {
                color::interactive_active()
            } else {
                color::text_muted()
            })
            .hover(|s| {
                s.bg(color::accent_hover())
                    .text_color(color::interactive_active())
            })
            .cursor_pointer()
            .child(view.glyph())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.activate(view);
                cx.notify();
            }))
    }

    /// Sidebar listing entries for the active view (channels, DMs, etc.).
    fn channel_sidebar(&self) -> impl IntoElement {
        let view = self.nav.active();
        let rows: &[&'static str] = match view {
            View::Servers => &["# general", "# random", "# dev"],
            View::DirectMessages => &["Alice", "Bob", "Carol"],
            View::Settings => &["My Account", "Appearance", "Notifications"],
        };

        v_flex()
            .w(px(space::SIDEBAR))
            .h_full()
            .flex_shrink_0()
            .bg(color::sidebar())
            .child(
                h_flex()
                    .h(px(space::HEADER))
                    .w_full()
                    .px(px(space::MD))
                    .items_center()
                    .border_b_1()
                    .border_color(color::border())
                    .text_color(color::text())
                    .text_size(px(font::LG))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(view.title()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .p(px(space::SM))
                    .gap(px(space::XS))
                    .children(rows.iter().map(|label| Self::sidebar_row(label))),
            )
    }

    /// A single, hoverable sidebar row.
    fn sidebar_row(label: &'static str) -> impl IntoElement {
        div()
            .id(label)
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .rounded(px(space::XS))
            .text_color(color::text_muted())
            .text_size(px(font::SM))
            .hover(|s| s.bg(color::hover()).text_color(color::text()))
            .cursor_pointer()
            .child(label)
    }

    /// Main content pane: a header plus a placeholder body per view.
    fn content(&self) -> impl IntoElement {
        let (title, body): (&'static str, &'static str) = match self.nav.active() {
            View::Servers => (
                "# general",
                "Welcome to #general — this is the start of the channel.",
            ),
            View::DirectMessages => (
                "Direct Messages",
                "Select a conversation to start chatting.",
            ),
            View::Settings => ("Settings", "Settings live here once the views land."),
        };

        v_flex()
            .flex_1()
            .h_full()
            .bg(color::chat())
            .child(
                h_flex()
                    .h(px(space::HEADER))
                    .w_full()
                    .px(px(space::LG))
                    .items_center()
                    .border_b_1()
                    .border_color(color::border())
                    .text_color(color::text())
                    .text_size(px(font::LG))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(px(space::SM))
                    .child(
                        div()
                            .text_color(color::text())
                            .text_size(px(font::TITLE))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_color(color::text_muted())
                            .text_size(px(font::MD))
                            .child(body),
                    )
                    .children(self.session.as_ref().map(|session| {
                        div()
                            .text_color(color::text_faint())
                            .text_size(px(font::SM))
                            .child(SharedString::from(format!(
                                "Signed in as {}",
                                session.user.username
                            )))
                    })),
            )
    }
}

impl ConcordApp {
    /// The main three-column layout (server rail · sidebar · content).
    fn main_layout(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(color::chat())
            .text_color(color::text())
            .font_family(font::FAMILY)
            .child(self.server_rail(cx))
            .child(self.channel_sidebar())
            .child(self.content())
    }
}

impl Render for ConcordApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen {
            Screen::Auth => self.auth.clone().into_any_element(),
            Screen::Main => self.main_layout(cx).into_any_element(),
        }
    }
}
