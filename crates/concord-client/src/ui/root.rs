//! Root view and the three-column main layout (server rail · sidebar · content).
//!
//! Gates the main UI behind authentication, then drives it from real backend
//! data: on sign-in it fetches the user's servers and DM conversations, loads a
//! server's channels when one is selected, and loads a channel's (or DM's)
//! message history when a row is clicked. Every list renders explicit loading,
//! empty, and error states.
//!
//! The REST calls run on the shared tokio runtime (GPUI's executor isn't
//! tokio); see [`crate::ui::data`]. Fetches that can be outraced by a fast
//! click — channels and messages — guard their result against the current
//! selection so a slow response can't overwrite a newer one.

use gpui::*;
use gpui_component::{h_flex, v_flex};
use tokio::sync::oneshot;
use uuid::Uuid;

use concord_shared::types::{Channel, ChannelType, DmChannelInfo, MessageWithAuthor, Server};

use crate::api;
use crate::auth::{self, Session};
use crate::ui::auth_view::{AuthEvent, AuthView};
use crate::ui::data::{self, LoadState};
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
/// [`Session`], swaps to the main three-column layout, and kicks off the
/// initial data fetches.
pub struct ConcordApp {
    screen: Screen,
    auth: Entity<AuthView>,
    nav: NavState,
    session: Option<Session>,
    /// The signed-in user's servers (`GET /api/servers`).
    servers: LoadState<Vec<Server>>,
    /// Channels of the selected server (`GET /api/servers/{id}/channels`).
    channels: LoadState<Vec<Channel>>,
    /// History of the open channel or DM, oldest-first for rendering.
    messages: LoadState<Vec<MessageWithAuthor>>,
    /// The thread [`messages`] was last loaded for (`nav.active_thread()` at
    /// load time). A view switch can change the active thread without touching
    /// `messages`, so the pane is keyed on this to tell when its history has
    /// gone stale and must be reloaded.
    messages_thread: Option<Uuid>,
    /// The user's DM conversations (`GET /api/dms`).
    dms: LoadState<Vec<DmChannelInfo>>,
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
            servers: LoadState::Idle,
            channels: LoadState::Idle,
            messages: LoadState::Idle,
            messages_thread: None,
            dms: LoadState::Idle,
            _auth_subscription: auth_subscription,
        }
    }

    /// React to the auth view: store the session, reveal the main app, and
    /// fetch the data the main screen renders.
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
                self.load_servers(cx);
                self.load_dms(cx);
            }
        }
    }

    // -- data fetching ------------------------------------------------------

    /// The current access token, if signed in.
    fn current_token(&self) -> Option<String> {
        self.session.as_ref().map(|s| s.access_token.clone())
    }

    /// Drive an API future on the tokio runtime and apply its result back on
    /// GPUI's executor. Centralizes the oneshot bridge so each fetch only has
    /// to say what to request and how to store the answer.
    fn drive<T, Fut, Apply>(cx: &mut Context<Self>, fut: Fut, apply: Apply)
    where
        T: Send + 'static,
        Fut: std::future::Future<Output = Result<T, api::ApiError>> + Send + 'static,
        Apply: FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    {
        let (tx, rx) = oneshot::channel();
        data::runtime().spawn(async move {
            let _ = tx.send(fut.await);
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                let result = match outcome {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(_canceled) => Err("the request was cancelled".to_string()),
                };
                apply(this, result, cx);
            });
        })
        .detach();
    }

    /// Fetch the signed-in user's servers, auto-selecting the first one.
    fn load_servers(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.current_token() else {
            return;
        };
        let base = auth::api_base_url();
        self.servers = LoadState::Loading;
        cx.notify();
        Self::drive(
            cx,
            async move { api::list_servers(&base, &token).await },
            |this, result, cx| match result {
                Ok(servers) => this.servers_loaded(servers, cx),
                Err(err) => {
                    this.servers = LoadState::Failed(err);
                    cx.notify();
                }
            },
        );
    }

    fn servers_loaded(&mut self, servers: Vec<Server>, cx: &mut Context<Self>) {
        let first = servers.first().map(|s| s.id);
        self.servers = LoadState::Loaded(servers);
        match first {
            Some(id) if self.nav.selected_server().is_none() => self.select_server(id, cx),
            _ => cx.notify(),
        }
    }

    /// Activate the Servers view on `server_id` and load its channels.
    fn select_server(&mut self, server_id: Uuid, cx: &mut Context<Self>) {
        self.nav.activate(View::Servers);
        self.nav.select_server(server_id);
        self.load_channels(cx);
        // Returning to a server keeps its previously open channel selected, so
        // the pane may still hold the thread we navigated away from; reconcile
        // it. (When the channel was cleared, the channel fetch reloads it.)
        self.sync_messages(cx);
    }

    /// Fetch the selected server's channels, auto-opening the first text one.
    fn load_channels(&mut self, cx: &mut Context<Self>) {
        let Some(server_id) = self.nav.selected_server() else {
            return;
        };
        let Some(token) = self.current_token() else {
            return;
        };
        let base = auth::api_base_url();
        self.channels = LoadState::Loading;
        cx.notify();
        Self::drive(
            cx,
            async move { api::list_channels(&base, &token, server_id).await },
            move |this, result, cx| {
                // Ignore a response for a server we've since navigated away from.
                if this.nav.selected_server() != Some(server_id) {
                    return;
                }
                match result {
                    Ok(channels) => this.channels_loaded(channels, cx),
                    Err(err) => {
                        this.channels = LoadState::Failed(err);
                        cx.notify();
                    }
                }
            },
        );
    }

    fn channels_loaded(&mut self, channels: Vec<Channel>, cx: &mut Context<Self>) {
        let first_text = channels
            .iter()
            .find(|c| c.channel_type == ChannelType::Text)
            .map(|c| c.id);
        self.channels = LoadState::Loaded(channels);
        match first_text {
            Some(id) if self.nav.selected_channel().is_none() => self.select_channel(id, cx),
            _ => cx.notify(),
        }
    }

    /// Open `channel_id` in the content pane and load its history.
    fn select_channel(&mut self, channel_id: Uuid, cx: &mut Context<Self>) {
        self.nav.select_channel(channel_id);
        self.load_messages(channel_id, cx);
    }

    /// Open DM conversation `dm_id` and load its history. DM channels share the
    /// `/api/channels/{id}/messages` endpoint with server channels.
    fn select_dm(&mut self, dm_id: Uuid, cx: &mut Context<Self>) {
        self.nav.activate(View::DirectMessages);
        self.nav.select_dm(dm_id);
        self.load_messages(dm_id, cx);
    }

    /// Switch to the DM view, opening the first conversation if none is open.
    fn show_dms(&mut self, cx: &mut Context<Self>) {
        self.nav.activate(View::DirectMessages);
        if self.nav.selected_dm().is_none() {
            if let Some(first) = self.dms.loaded().and_then(|d| d.first()).map(|d| d.id) {
                self.select_dm(first, cx);
                return;
            }
        }
        // An already-open conversation isn't re-fetched by the arm above, so
        // the pane may still hold the thread we came from; reconcile it.
        self.sync_messages(cx);
    }

    /// Reconcile the message pane with the active thread after a view switch.
    /// A click on a channel or DM row loads its history directly, but switching
    /// views via the rail can change which thread is active without touching
    /// [`Self::messages`] — leaving the pane showing the thread we left. Reload
    /// the active thread's history when the pane has gone stale, clear it when
    /// no thread is active, and otherwise just redraw the freshly-shown view.
    fn sync_messages(&mut self, cx: &mut Context<Self>) {
        match message_sync(self.nav.active_thread(), self.messages_thread) {
            MessageSync::Keep => cx.notify(),
            MessageSync::Clear => {
                self.messages = LoadState::Idle;
                self.messages_thread = None;
                cx.notify();
            }
            MessageSync::Reload(thread) => self.load_messages(thread, cx),
        }
    }

    /// Fetch a channel's (or DM's) message history.
    fn load_messages(&mut self, channel_id: Uuid, cx: &mut Context<Self>) {
        let Some(token) = self.current_token() else {
            return;
        };
        let base = auth::api_base_url();
        self.messages = LoadState::Loading;
        self.messages_thread = Some(channel_id);
        cx.notify();
        Self::drive(
            cx,
            async move { api::list_messages(&base, &token, channel_id).await },
            move |this, result, cx| {
                // Drop a stale response if the user has since switched threads.
                if this.nav.active_thread() != Some(channel_id) {
                    return;
                }
                match result {
                    Ok(mut messages) => {
                        // The endpoint returns newest-first; render oldest-first
                        // so the latest message sits at the bottom.
                        messages.reverse();
                        this.messages = LoadState::Loaded(messages);
                    }
                    Err(err) => this.messages = LoadState::Failed(err),
                }
                cx.notify();
            },
        );
    }

    /// Fetch the user's DM conversations.
    fn load_dms(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.current_token() else {
            return;
        };
        let base = auth::api_base_url();
        self.dms = LoadState::Loading;
        cx.notify();
        Self::drive(
            cx,
            async move { api::list_dms(&base, &token).await },
            |this, result, cx| {
                match result {
                    Ok(dms) => this.dms = LoadState::Loaded(dms),
                    Err(err) => this.dms = LoadState::Failed(err),
                }
                cx.notify();
            },
        );
    }

    // -- server rail --------------------------------------------------------

    /// Leftmost rail: one icon per server, then fixed DMs and Settings buttons.
    fn server_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut servers: Vec<AnyElement> = Vec::new();
        match &self.servers {
            LoadState::Loaded(list) => {
                for server in list {
                    servers.push(self.server_button(server, cx));
                }
            }
            LoadState::Failed(_) => servers.push(Self::rail_dot("!", color::danger())),
            LoadState::Idle | LoadState::Loading => {
                servers.push(Self::rail_dot("…", color::text_muted()))
            }
        }

        v_flex()
            .w(px(space::SERVER_RAIL))
            .h_full()
            .flex_shrink_0()
            .bg(color::server_rail())
            .py(px(space::MD))
            .gap(px(space::SM))
            .items_center()
            .children(servers)
            .child(div().w(px(32.0)).h(px(2.0)).rounded(px(1.0)).bg(color::border()))
            .child(self.rail_button(View::DirectMessages, cx))
            .child(div().flex_1())
            .child(self.rail_button(View::Settings, cx))
    }

    /// A circular icon for one server. Clicking it shows that server.
    fn server_button(&self, server: &Server, cx: &mut Context<Self>) -> AnyElement {
        let id = server.id;
        let active = self.nav.is_active(View::Servers) && self.nav.selected_server() == Some(id);
        div()
            .id(SharedString::from(id.to_string()))
            .size(px(space::RAIL_BUTTON))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(space::LG))
            .text_size(px(font::MD))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(if active {
                color::accent()
            } else {
                color::sidebar()
            })
            .text_color(if active {
                color::interactive_active()
            } else {
                color::text()
            })
            .hover(|s| {
                s.bg(color::accent_hover())
                    .text_color(color::interactive_active())
            })
            .cursor_pointer()
            .child(server_initial(&server.name))
            .on_click(cx.listener(move |this, _, _, cx| this.select_server(id, cx)))
            .into_any_element()
    }

    /// A non-interactive rail placeholder (loading / error indicator).
    fn rail_dot(label: impl Into<SharedString>, fg: Hsla) -> AnyElement {
        div()
            .size(px(space::RAIL_BUTTON))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(space::LG))
            .bg(color::sidebar())
            .text_color(fg)
            .text_size(px(font::MD))
            .child(label.into())
            .into_any_element()
    }

    /// A fixed rail button (DMs / Settings). Clicking it activates `view`.
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
            .on_click(cx.listener(move |this, _, _, cx| match view {
                View::DirectMessages => this.show_dms(cx),
                _ => {
                    this.nav.activate(view);
                    cx.notify();
                }
            }))
    }

    // -- sidebar ------------------------------------------------------------

    /// Sidebar listing entries for the active view (channels, DMs, settings).
    fn channel_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.nav.active();
        let title: SharedString = match view {
            View::Servers => self.selected_server_name(),
            View::DirectMessages => "Direct Messages".into(),
            View::Settings => "Settings".into(),
        };
        let rows: Vec<AnyElement> = match view {
            View::Servers => self.channel_rows(cx),
            View::DirectMessages => self.dm_rows(cx),
            View::Settings => Self::settings_rows(),
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
                    .child(title),
            )
            .child(
                v_flex()
                    .flex_1()
                    .p(px(space::SM))
                    .gap(px(space::XS))
                    .children(rows),
            )
    }

    /// Name of the selected server, falling back to the app name.
    fn selected_server_name(&self) -> SharedString {
        self.nav
            .selected_server()
            .and_then(|id| {
                self.servers
                    .loaded()
                    .and_then(|list| list.iter().find(|s| s.id == id))
            })
            .map(|s| SharedString::from(s.name.clone()))
            .unwrap_or_else(|| "Concord".into())
    }

    /// Channel rows for the Servers view, with loading / empty / error states.
    fn channel_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.nav.selected_server().is_none() {
            return vec![match &self.servers {
                LoadState::Failed(e) => Self::muted_row(format!("Couldn't load servers: {e}")),
                LoadState::Loaded(_) => Self::muted_row("No servers yet"),
                LoadState::Idle | LoadState::Loading => Self::muted_row("Loading…"),
            }];
        }

        match &self.channels {
            LoadState::Loaded(channels) if channels.is_empty() => {
                vec![Self::muted_row("No channels yet")]
            }
            LoadState::Loaded(channels) => {
                let selected = self.nav.selected_channel();
                let mut rows = Vec::with_capacity(channels.len());
                for channel in channels {
                    let id = channel.id;
                    rows.push(self.sidebar_row(
                        SharedString::from(id.to_string()),
                        channel_label(channel),
                        selected == Some(id),
                        move |this, _, cx| this.select_channel(id, cx),
                        cx,
                    ));
                }
                rows
            }
            LoadState::Failed(e) => vec![Self::muted_row(format!("Couldn't load channels: {e}"))],
            LoadState::Idle | LoadState::Loading => vec![Self::muted_row("Loading channels…")],
        }
    }

    /// DM rows for the DirectMessages view, with loading / empty / error states.
    fn dm_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match &self.dms {
            LoadState::Loaded(dms) if dms.is_empty() => {
                vec![Self::muted_row("No conversations yet")]
            }
            LoadState::Loaded(dms) => {
                let selected = self.nav.selected_dm();
                let mut rows = Vec::with_capacity(dms.len());
                for dm in dms {
                    let id = dm.id;
                    rows.push(self.sidebar_row(
                        SharedString::from(id.to_string()),
                        self.dm_display_name(dm),
                        selected == Some(id),
                        move |this, _, cx| this.select_dm(id, cx),
                        cx,
                    ));
                }
                rows
            }
            LoadState::Failed(e) => {
                vec![Self::muted_row(format!("Couldn't load conversations: {e}"))]
            }
            LoadState::Idle | LoadState::Loading => vec![Self::muted_row("Loading conversations…")],
        }
    }

    /// Static settings rows (fixed, not yet wired to real views).
    fn settings_rows() -> Vec<AnyElement> {
        ["My Account", "Appearance", "Notifications"]
            .into_iter()
            .map(Self::muted_row)
            .collect()
    }

    /// A clickable, selectable sidebar row.
    fn sidebar_row(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .rounded(px(space::XS))
            .text_size(px(font::SM))
            .text_color(if selected {
                color::text()
            } else {
                color::text_muted()
            })
            .bg(if selected {
                color::active()
            } else {
                color::sidebar()
            })
            .hover(|s| s.bg(color::hover()).text_color(color::text()))
            .cursor_pointer()
            .child(label.into())
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
            .into_any_element()
    }

    /// A non-interactive, muted sidebar row used for placeholders and states.
    fn muted_row(label: impl Into<SharedString>) -> AnyElement {
        div()
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .text_size(px(font::SM))
            .text_color(color::text_muted())
            .child(label.into())
            .into_any_element()
    }

    /// Display name for a DM: its group name, or the other participants' names.
    fn dm_display_name(&self, dm: &DmChannelInfo) -> SharedString {
        if let Some(name) = dm.name.as_ref().filter(|n| !n.trim().is_empty()) {
            return SharedString::from(name.clone());
        }
        let me = self.session.as_ref().map(|s| s.user.id);
        let others: Vec<&str> = dm
            .participants
            .iter()
            .filter(|p| Some(p.user_id) != me)
            .map(|p| p.username.as_str())
            .collect();
        if others.is_empty() {
            "Direct Message".into()
        } else {
            others.join(", ").into()
        }
    }

    // -- content pane -------------------------------------------------------

    /// Main content pane: header plus message history for the active view.
    fn content(&self) -> impl IntoElement {
        let body = match self.nav.active() {
            View::Servers => self.channel_content(),
            View::DirectMessages => self.dm_content(),
            View::Settings => self.settings_content(),
        };
        v_flex().flex_1().h_full().bg(color::chat()).child(body)
    }

    /// The Servers view content: channel header and message history.
    fn channel_content(&self) -> AnyElement {
        let selected = self.nav.selected_channel();
        let title: SharedString = selected
            .and_then(|id| {
                self.channels
                    .loaded()
                    .and_then(|cs| cs.iter().find(|c| c.id == id))
            })
            .map(channel_label)
            .unwrap_or_else(|| "Concord".into());

        let body = if selected.is_none() {
            let hint: SharedString = if self.nav.selected_server().is_none() {
                match &self.servers {
                    LoadState::Failed(e) => format!("Couldn't load servers: {e}").into(),
                    LoadState::Loaded(_) => "You're not in any servers yet.".into(),
                    LoadState::Idle | LoadState::Loading => "Loading your servers…".into(),
                }
            } else {
                match &self.channels {
                    LoadState::Loaded(cs) if cs.is_empty() => {
                        "This server has no channels yet.".into()
                    }
                    LoadState::Failed(e) => format!("Couldn't load channels: {e}").into(),
                    LoadState::Idle | LoadState::Loading => "Loading channels…".into(),
                    LoadState::Loaded(_) => "Pick a channel to start reading.".into(),
                }
            };
            Self::centered_state("No channel open", hint)
        } else {
            self.message_pane()
        };

        v_flex()
            .flex_1()
            .h_full()
            .child(Self::header(title))
            .child(body)
            .into_any_element()
    }

    /// The DM view content: conversation header and message history.
    fn dm_content(&self) -> AnyElement {
        let selected = self.nav.selected_dm();
        let title: SharedString = selected
            .and_then(|id| self.dms.loaded().and_then(|ds| ds.iter().find(|d| d.id == id)))
            .map(|d| self.dm_display_name(d))
            .unwrap_or_else(|| "Direct Messages".into());

        let body = if selected.is_none() {
            let hint: SharedString = match &self.dms {
                LoadState::Loaded(ds) if ds.is_empty() => "You have no conversations yet.".into(),
                LoadState::Failed(e) => format!("Couldn't load conversations: {e}").into(),
                LoadState::Idle | LoadState::Loading => "Loading conversations…".into(),
                LoadState::Loaded(_) => "Pick a conversation to open it.".into(),
            };
            Self::centered_state("No conversation open", hint)
        } else {
            self.message_pane()
        };

        v_flex()
            .flex_1()
            .h_full()
            .child(Self::header(title))
            .child(body)
            .into_any_element()
    }

    /// The Settings view content: a placeholder plus the signed-in identity.
    fn settings_content(&self) -> AnyElement {
        let signed_in = self
            .session
            .as_ref()
            .map(|s| SharedString::from(format!("Signed in as {}", s.user.username)));

        v_flex()
            .flex_1()
            .h_full()
            .child(Self::header("Settings"))
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
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .text_color(color::text_muted())
                            .text_size(px(font::MD))
                            .child("Settings live here once the views land."),
                    )
                    .children(signed_in.map(|line| {
                        div()
                            .text_color(color::text_faint())
                            .text_size(px(font::SM))
                            .child(line)
                    })),
            )
            .into_any_element()
    }

    /// Render the message history (or its loading / empty / error state).
    fn message_pane(&self) -> AnyElement {
        match &self.messages {
            LoadState::Idle | LoadState::Loading => Self::centered_state("Loading messages…", ""),
            LoadState::Failed(e) => Self::centered_state("Couldn't load messages", e.clone()),
            LoadState::Loaded(messages) if messages.is_empty() => {
                Self::centered_state("No messages yet", "Be the first to say something.")
            }
            LoadState::Loaded(messages) => {
                let mut rows = Vec::with_capacity(messages.len());
                for message in messages {
                    rows.push(Self::message_row(message));
                }
                v_flex()
                    .id("message-list")
                    .flex_1()
                    .w_full()
                    .overflow_y_scroll()
                    .px(px(space::LG))
                    .py(px(space::MD))
                    .gap(px(space::MD))
                    .children(rows)
                    .into_any_element()
            }
        }
    }

    /// One message: author + timestamp header, then the content.
    fn message_row(message: &MessageWithAuthor) -> AnyElement {
        let author = message
            .author
            .as_ref()
            .map(|a| SharedString::from(a.username.clone()))
            .unwrap_or_else(|| "Unknown".into());
        let timestamp = SharedString::from(message.created_at.format("%b %d, %H:%M").to_string());

        v_flex()
            .w_full()
            .gap(px(space::XS))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(space::SM))
                    .child(
                        div()
                            .text_color(color::text())
                            .text_size(px(font::MD))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(author),
                    )
                    .child(
                        div()
                            .text_color(color::text_faint())
                            .text_size(px(font::SM))
                            .child(timestamp),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .text_color(color::text())
                    .text_size(px(font::MD))
                    .child(SharedString::from(message.content.clone())),
            )
            .into_any_element()
    }

    /// A centered title + hint, used for loading / empty / error states.
    fn centered_state(title: impl Into<SharedString>, hint: impl Into<SharedString>) -> AnyElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(space::SM))
            .child(
                div()
                    .text_color(color::text())
                    .text_size(px(font::LG))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.into()),
            )
            .child(
                div()
                    .text_color(color::text_muted())
                    .text_size(px(font::SM))
                    .child(hint.into()),
            )
            .into_any_element()
    }

    /// A content-pane header bar with `title`.
    fn header(title: impl Into<SharedString>) -> impl IntoElement {
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
            .child(title.into())
    }

    /// The main three-column layout (server rail · sidebar · content).
    fn main_layout(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(color::chat())
            .text_color(color::text())
            .font_family(font::FAMILY)
            .child(self.server_rail(cx))
            .child(self.channel_sidebar(cx))
            .child(self.content())
    }
}

/// `# name` for a text channel, a speaker glyph for a voice channel.
fn channel_label(channel: &Channel) -> SharedString {
    let prefix = match channel.channel_type {
        ChannelType::Text => "# ",
        ChannelType::Voice => "🔊 ",
    };
    SharedString::from(format!("{prefix}{}", channel.name))
}

/// What the message pane should do when the active thread (`active`) and the
/// thread its contents were loaded for (`loaded`) may have diverged after a
/// view switch. See [`ConcordApp::sync_messages`].
#[derive(Debug, PartialEq, Eq)]
enum MessageSync {
    /// The pane already shows the active thread; leave its contents untouched.
    Keep,
    /// No thread is active (an empty view); clear the pane.
    Clear,
    /// The pane belongs to another thread; (re)load this one's history.
    Reload(Uuid),
}

/// Decide how the message pane should react to the active thread, given the
/// thread its current contents were loaded for. Kept pure (no view, no window)
/// so the view-switch reload logic is unit-testable.
fn message_sync(active: Option<Uuid>, loaded: Option<Uuid>) -> MessageSync {
    match active {
        _ if active == loaded => MessageSync::Keep,
        Some(thread) => MessageSync::Reload(thread),
        None => MessageSync::Clear,
    }
}

/// First letter of a server name, used as its rail glyph.
fn server_initial(name: &str) -> SharedString {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
        .into()
}

impl Render for ConcordApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen {
            Screen::Auth => self.auth.clone().into_any_element(),
            Screen::Main => self.main_layout(cx).into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    // Import specifics, not `use super::*`: this module pulls in `gpui::*`, and
    // re-globbing it into a test mod blows the type-resolution recursion limit.
    use super::{message_sync, MessageSync};
    use uuid::Uuid;

    #[test]
    fn keeps_the_pane_when_the_thread_is_unchanged() {
        let thread = Uuid::new_v4();
        // Same thread loaded and active — switching views must not refetch.
        assert_eq!(message_sync(Some(thread), Some(thread)), MessageSync::Keep);
        // Both empty (e.g. an empty Servers view) is also a no-op.
        assert_eq!(message_sync(None, None), MessageSync::Keep);
    }

    #[test]
    fn reloads_when_the_active_thread_differs() {
        let loaded = Uuid::new_v4();
        let active = Uuid::new_v4();
        // The bug this guards: the pane holds another thread's history after a
        // rail switch, so the active thread must be (re)loaded.
        assert_eq!(message_sync(Some(active), Some(loaded)), MessageSync::Reload(active));
        // First load into an idle pane reloads too.
        assert_eq!(message_sync(Some(active), None), MessageSync::Reload(active));
    }

    #[test]
    fn clears_when_no_thread_is_active() {
        let loaded = Uuid::new_v4();
        // Switched to a view with nothing selected: drop the stale history.
        assert_eq!(message_sync(None, Some(loaded)), MessageSync::Clear);
    }
}
