//! Root view and three-column layout (server rail · sidebar · content).
//!
//! The root owns the shared application [state](crate::state) as GPUI entities —
//! [`AuthState`], [`ServersState`], [`ChatState`], and [`ConnectionState`] — and
//! drives the data flow between them:
//!
//! - It gates the UI behind authentication, starting on the [`AuthView`] and
//!   swapping to the main layout once a [`crate::auth::Session`] arrives.
//! - On login it stores the session, opens the WebSocket, and loads the initial
//!   data (servers, their channels, the active server's members and the active
//!   channel's history) over REST.
//! - It folds live `WsEvent`s into the connection and chat state.
//!
//! The layout reads those entities and re-renders by observing them, so it is a
//! first consumer of the "entity handles passed to views, views subscribe via
//! `cx.observe`" pattern that the per-feature views will follow.

use gpui::*;
use gpui_component::{h_flex, v_flex};
use uuid::Uuid;

use concord_shared::protocol::{ServerMsg, Token};
use concord_shared::types::{ChannelType, MessageAuthor, MessageWithAuthor, Server};

use crate::api;
use crate::auth;
use crate::state::{AuthState, ChatState, ConnectionState, ConnectionStatus, ServersState};
use crate::ui::auth_view::{AuthEvent, AuthView};
use crate::ui::nav::{NavState, View};
use crate::ui::theme::{color, font, space};
use crate::ws::{ConnectionHandle, WsEvent};

/// Event-channel capacity for the background WebSocket task.
const WS_EVENT_BUFFER: usize = 256;
/// Page size requested for channel history.
const MESSAGE_PAGE: i64 = 50;

/// Which top-level screen the app is showing.
enum Screen {
    /// The login / register card, shown until the user authenticates.
    Auth,
    /// The main three-column app, shown once a session exists.
    Main,
}

/// The application's root view: it gates the main UI behind authentication and
/// owns the shared application state.
pub struct ConcordApp {
    screen: Screen,
    auth: Entity<AuthView>,
    nav: NavState,

    // Shared application state, handed to views as entity handles.
    auth_state: Entity<AuthState>,
    servers: Entity<ServersState>,
    chat: Entity<ChatState>,
    connection: Entity<ConnectionState>,

    /// The live connection handle. Held so the background task's command
    /// channel stays open (it exits once every handle drops); later work sends
    /// outgoing messages through it.
    _ws_handle: Option<ConnectionHandle>,
    /// Auth-view and state observers; dropped together with the view.
    _subscriptions: Vec<Subscription>,
}

impl ConcordApp {
    /// Construct the root view, starting on the auth screen.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let auth = cx.new(|cx| AuthView::new(window, cx));

        let auth_state = cx.new(|_| AuthState::new());
        let servers = cx.new(|_| ServersState::new());
        let chat = cx.new(|_| ChatState::new());
        let connection = cx.new(|_| ConnectionState::new());

        // Re-render the layout whenever the auth view fires or any piece of
        // shared state changes.
        let subscriptions = vec![
            cx.subscribe(&auth, Self::on_auth_event),
            cx.observe(&auth_state, |_, _, cx| cx.notify()),
            cx.observe(&servers, |_, _, cx| cx.notify()),
            cx.observe(&chat, |_, _, cx| cx.notify()),
            cx.observe(&connection, |_, _, cx| cx.notify()),
        ];

        Self {
            screen: Screen::Auth,
            auth,
            nav: NavState::new(),
            auth_state,
            servers,
            chat,
            connection,
            _ws_handle: None,
            _subscriptions: subscriptions,
        }
    }

    /// React to the auth view: store the session, reveal the main app, connect
    /// the socket, and load the initial data.
    fn on_auth_event(&mut self, _auth: Entity<AuthView>, event: &AuthEvent, cx: &mut Context<Self>) {
        match event {
            AuthEvent::Authenticated(session) => {
                let session = session.clone();
                self.auth_state.update(cx, |auth, cx| {
                    auth.sign_in(session);
                    cx.notify();
                });
                self.screen = Screen::Main;
                self.connect(cx);
                self.load_initial_data(cx);
                cx.notify();
            }
        }
    }

    // -- Networking -------------------------------------------------------

    /// Open the WebSocket and stream its events into the connection and chat
    /// state. The socket runs on the shared tokio runtime; events cross back to
    /// the GPUI executor over the handle's channel.
    fn connect(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };

        let rt = api::runtime();
        // `ConnectionHandle::spawn` calls `tokio::spawn`, so it must run inside
        // the runtime's context.
        let (handle, mut events) = {
            let _guard = rt.enter();
            ConnectionHandle::spawn(WS_EVENT_BUFFER)
        };
        self._ws_handle = Some(handle.clone());
        self.connection.update(cx, |c, cx| {
            c.connecting();
            cx.notify();
        });

        let url = api::ws_url();
        let token = Token::new(token);
        rt.spawn(async move {
            if let Err(err) = handle.connect(url, token).await {
                tracing::error!(error = %err, "failed to send ws connect command");
            }
        });

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                match this.update(cx, |this, cx| this.on_ws_event(&event, cx)) {
                    Ok(true) | Err(_) => break, // socket closed, or root view gone
                    Ok(false) => {}
                }
            }
        })
        .detach();
    }

    /// Fold one WebSocket event into the relevant state. Returns `true` when the
    /// socket has closed and the event loop should stop.
    fn on_ws_event(&mut self, event: &WsEvent, cx: &mut Context<Self>) -> bool {
        match event {
            WsEvent::Connected { .. } => self.connection.update(cx, |c, cx| {
                c.connected();
                cx.notify();
            }),
            WsEvent::Disconnected { reason } => self.connection.update(cx, |c, cx| {
                c.disconnected(Some(reason.clone()));
                cx.notify();
            }),
            WsEvent::Reconnecting { attempt } => self.connection.update(cx, |c, cx| {
                c.reconnecting(*attempt);
                cx.notify();
            }),
            WsEvent::AuthFailed { message, .. } => self.connection.update(cx, |c, cx| {
                c.disconnected(Some(message.clone()));
                cx.notify();
            }),
            WsEvent::Message(msg) => self.on_server_msg(msg, cx),
            WsEvent::Closed => {
                self.connection.update(cx, |c, cx| {
                    c.disconnected(None);
                    cx.notify();
                });
                return true;
            }
        }
        false
    }

    /// Apply a decoded server message. Only the events the loaded state can
    /// represent are handled; server/membership/presence/DM events are folded
    /// in by later work.
    fn on_server_msg(&mut self, msg: &ServerMsg, cx: &mut Context<Self>) {
        match msg {
            ServerMsg::NewMessage {
                id,
                channel_id,
                author_id,
                content,
                created_at,
            } => {
                if self.chat.read(cx).active_channel() != Some(*channel_id) {
                    return;
                }
                // The wire message carries no author profile, so resolve it
                // locally; the server-assigned `created_at` is taken as-is.
                let author = (*author_id).and_then(|id| self.resolve_author(id, cx));
                let message = MessageWithAuthor {
                    id: *id,
                    channel_id: *channel_id,
                    author,
                    content: content.clone(),
                    edited_at: None,
                    created_at: *created_at,
                };
                self.chat.update(cx, |c, cx| {
                    c.push_message(message);
                    cx.notify();
                });
            }
            ServerMsg::MessageEdited { message_id, content, edited_at } => {
                let (id, content, edited_at) = (*message_id, content.clone(), *edited_at);
                self.chat.update(cx, |c, cx| {
                    c.edit_message(id, content, edited_at);
                    cx.notify();
                });
            }
            ServerMsg::MessageDeleted { message_id } => {
                let id = *message_id;
                self.chat.update(cx, |c, cx| {
                    c.delete_message(id);
                    cx.notify();
                });
            }
            ServerMsg::TypingStarted { channel_id, user_id }
                if self.chat.read(cx).active_channel() == Some(*channel_id) =>
            {
                let user = *user_id;
                self.chat.update(cx, |c, cx| {
                    c.start_typing(user);
                    cx.notify();
                });
            }
            ServerMsg::TypingStopped { channel_id, user_id }
                if self.chat.read(cx).active_channel() == Some(*channel_id) =>
            {
                let user = *user_id;
                self.chat.update(cx, |c, cx| {
                    c.stop_typing(user);
                    cx.notify();
                });
            }
            _ => {}
        }
    }

    /// Best-effort author profile for a live message: the signed-in user, or a
    /// member of the active server, else `None`.
    fn resolve_author(&self, author_id: Uuid, cx: &mut Context<Self>) -> Option<MessageAuthor> {
        if let Some(user) = self.auth_state.read(cx).user() {
            if user.id == author_id {
                return Some(MessageAuthor {
                    id: user.id,
                    username: user.username.clone(),
                    avatar_url: user.avatar_url.clone(),
                });
            }
        }
        let servers = self.servers.read(cx);
        let active = servers.active_server()?;
        servers
            .members_for(active)
            .iter()
            .find(|m| m.user_id == author_id)
            .map(|m| MessageAuthor {
                id: m.user_id,
                username: m.username.clone(),
                avatar_url: m.avatar_url.clone(),
            })
    }

    // -- Initial data load ------------------------------------------------

    /// Load the server list and each server's channels, then open the active
    /// server's first text channel and load its members.
    fn load_initial_data(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        self.servers.update(cx, |s, cx| {
            s.set_loading(true);
            cx.notify();
        });

        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        api::runtime().spawn(async move {
            let _ = tx.send(load_servers_and_channels(&base, &token).await);
        });

        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(Ok(data)) => this.apply_initial_data(data, cx),
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, "failed to load initial data");
                    this.servers.update(cx, |s, cx| {
                        s.set_loading(false);
                        cx.notify();
                    });
                }
                Err(_canceled) => this.servers.update(cx, |s, cx| {
                    s.set_loading(false);
                    cx.notify();
                }),
            });
        })
        .detach();
    }

    /// Store the loaded servers and channels, then kick off the active server's
    /// member and history loads.
    fn apply_initial_data(&mut self, data: InitialData, cx: &mut Context<Self>) {
        self.servers.update(cx, |s, cx| {
            s.set_loading(false);
            s.set_servers(data.servers);
            for (server_id, channels) in data.channels {
                s.set_channels(server_id, channels);
            }
            cx.notify();
        });

        let Some(active) = self.servers.read(cx).active_server() else {
            return;
        };
        self.load_members(active, cx);
        let first_channel = self
            .servers
            .read(cx)
            .channels_for(active)
            .iter()
            .find(|c| c.channel_type == ChannelType::Text)
            .map(|c| c.id);
        if let Some(channel_id) = first_channel {
            self.open_channel(channel_id, cx);
        }
    }

    /// Load the members of `server_id` into the servers state.
    fn load_members(&mut self, server_id: Uuid, cx: &mut Context<Self>) {
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        api::runtime().spawn(async move {
            let _ = tx.send(api::list_members(&base, &token, server_id).await);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(members)) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.servers.update(cx, |s, cx| {
                        s.set_members(server_id, members);
                        cx.notify();
                    });
                });
            }
        })
        .detach();
    }

    /// Make `channel_id` the active channel and load its first history page.
    /// Re-selecting the already-active channel is a no-op.
    fn open_channel(&mut self, channel_id: Uuid, cx: &mut Context<Self>) {
        if self.chat.read(cx).active_channel() == Some(channel_id) {
            return;
        }
        self.chat.update(cx, |c, cx| {
            c.open_channel(channel_id);
            c.set_loading(true);
            cx.notify();
        });
        self.load_history(channel_id, cx);
    }

    /// Fetch the newest page of history for `channel_id`.
    fn load_history(&mut self, channel_id: Uuid, cx: &mut Context<Self>) {
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        api::runtime().spawn(async move {
            let result = api::list_messages(&base, &token, channel_id, None, Some(MESSAGE_PAGE)).await;
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.chat.update(cx, |c, cx| {
                    match outcome {
                        Ok(Ok(page)) => {
                            // A full page means older messages may remain.
                            let has_more = page.len() as i64 == MESSAGE_PAGE;
                            c.set_history(channel_id, page, has_more);
                        }
                        Ok(Err(err)) => {
                            // Only clear the spinner if this response is still
                            // for the channel on screen — a late failure for a
                            // channel the user already left must not touch it.
                            if c.active_channel() == Some(channel_id) {
                                c.set_loading(false);
                            }
                            tracing::warn!(error = %err, "failed to load channel history");
                        }
                        Err(_canceled) => {
                            if c.active_channel() == Some(channel_id) {
                                c.set_loading(false);
                            }
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    // -- Layout -----------------------------------------------------------

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

    /// Sidebar: the active server's channels, or a placeholder for other views.
    fn channel_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.nav.active();
        let header: SharedString = match view {
            View::Servers => self
                .servers
                .read(cx)
                .active_server_info()
                .map(|s| SharedString::from(s.name.clone()))
                .unwrap_or_else(|| View::Servers.title().into()),
            other => other.title().into(),
        };

        let body: AnyElement = match view {
            View::Servers => self.channel_list(cx).into_any_element(),
            View::DirectMessages => {
                Self::placeholder_rows(&["Direct messages land in later work."]).into_any_element()
            }
            View::Settings => {
                Self::placeholder_rows(&["My Account", "Appearance", "Notifications"])
                    .into_any_element()
            }
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
                    .child(header),
            )
            .child(body)
    }

    /// The active server's channel rows, with loading and empty states.
    fn channel_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_channel = self.chat.read(cx).active_channel();
        // Collect owned data first so the `servers` borrow is dropped before the
        // per-row `cx.listener` calls reborrow `cx`.
        let (loading, channels): (bool, Vec<(Uuid, String, ChannelType)>) = {
            let servers = self.servers.read(cx);
            (
                servers.is_loading(),
                servers
                    .active_channels()
                    .iter()
                    .map(|c| (c.id, c.name.clone(), c.channel_type))
                    .collect(),
            )
        };

        let mut list = v_flex().flex_1().p(px(space::SM)).gap(px(space::XS));
        if loading {
            list = list.child(Self::muted_row("Loading…"));
        } else if channels.is_empty() {
            list = list.child(Self::muted_row("No channels yet."));
        } else {
            for (id, name, channel_type) in channels {
                let selected = active_channel == Some(id);
                list = list.child(self.channel_row(id, &name, channel_type, selected, cx));
            }
        }
        list
    }

    /// A single, clickable channel row; clicking opens the channel.
    fn channel_row(
        &self,
        id: Uuid,
        name: &str,
        channel_type: ChannelType,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let prefix = match channel_type {
            ChannelType::Text => "# ",
            ChannelType::Voice => "🔊 ",
        };
        let label = SharedString::from(format!("{prefix}{name}"));

        let mut row = div()
            .id(SharedString::from(id.to_string()))
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .rounded(px(space::XS))
            .text_size(px(font::SM));
        if selected {
            row = row.bg(color::active()).text_color(color::text());
        } else {
            row = row.text_color(color::text_muted());
        }
        row.hover(|s| s.bg(color::hover()).text_color(color::text()))
            .cursor_pointer()
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.open_channel(id, cx)))
    }

    /// Non-interactive, muted sidebar rows for placeholder / status text.
    fn placeholder_rows(rows: &[&'static str]) -> impl IntoElement {
        v_flex()
            .flex_1()
            .p(px(space::SM))
            .gap(px(space::XS))
            .children(rows.iter().map(|label| Self::muted_row(*label)))
    }

    /// A single muted, non-interactive row.
    fn muted_row(label: impl Into<SharedString>) -> impl IntoElement {
        div()
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .text_color(color::text_muted())
            .text_size(px(font::SM))
            .child(label.into())
    }

    /// Main content pane: the chat view for servers, placeholders otherwise.
    fn content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.nav.active() {
            View::Servers => self.chat_pane(cx).into_any_element(),
            View::DirectMessages => {
                Self::placeholder_pane("Direct Messages", "Select a conversation to start chatting.")
                    .into_any_element()
            }
            View::Settings => {
                Self::placeholder_pane("Settings", "Settings live here once the views land.")
                    .into_any_element()
            }
        }
    }

    /// The chat pane: header, message list, and a footer with typing and
    /// connection status.
    fn chat_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_channel = self.chat.read(cx).active_channel();
        let title: SharedString = active_channel
            .and_then(|id| {
                self.servers
                    .read(cx)
                    .active_channels()
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| SharedString::from(format!("# {}", c.name)))
            })
            .unwrap_or_else(|| "Select a channel".into());

        let (loading, messages): (bool, Vec<(SharedString, SharedString)>) = {
            let chat = self.chat.read(cx);
            (
                chat.is_loading(),
                chat.messages()
                    .iter()
                    .map(|m| {
                        let author = m
                            .author
                            .as_ref()
                            .map(|a| a.username.clone())
                            .unwrap_or_else(|| "unknown".into());
                        (SharedString::from(author), SharedString::from(m.content.clone()))
                    })
                    .collect(),
            )
        };
        let typing = self.chat.read(cx).typing_count();
        let status = self.connection.read(cx).status();
        let username = self
            .auth_state
            .read(cx)
            .user()
            .map(|u| SharedString::from(u.username.clone()));

        let mut body = v_flex()
            .flex_1()
            .p(px(space::MD))
            .gap(px(space::SM))
            .overflow_hidden();
        if active_channel.is_none() {
            body = body.child(Self::muted_row("Pick a channel from the sidebar."));
        } else if loading {
            body = body.child(Self::muted_row("Loading messages…"));
        } else if messages.is_empty() {
            body = body.child(Self::muted_row("No messages yet — say hello!"));
        } else {
            for (author, content) in messages {
                body = body.child(Self::message_row(author, content));
            }
        }

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
                    .justify_between()
                    .border_b_1()
                    .border_color(color::border())
                    .child(
                        div()
                            .text_color(color::text())
                            .text_size(px(font::LG))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(Self::connection_indicator(status)),
            )
            .child(body)
            .child(Self::chat_footer(typing, username))
    }

    /// One message row: bold author then content.
    fn message_row(author: SharedString, content: SharedString) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap(px(space::XS))
            .child(
                div()
                    .text_color(color::text())
                    .text_size(px(font::SM))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(author),
            )
            .child(
                div()
                    .text_color(color::text())
                    .text_size(px(font::MD))
                    .child(content),
            )
    }

    /// A colored dot plus label reflecting the WebSocket status.
    fn connection_indicator(status: ConnectionStatus) -> impl IntoElement {
        let dot: Hsla = match status {
            ConnectionStatus::Connected => rgb(0x23a55a).into(),
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => rgb(0xf0b232).into(),
            ConnectionStatus::Disconnected => rgb(0xf23f43).into(),
        };
        h_flex()
            .items_center()
            .gap(px(space::XS))
            .child(div().size(px(8.0)).rounded_full().bg(dot))
            .child(
                div()
                    .text_color(color::text_muted())
                    .text_size(px(font::SM))
                    .child(status.label()),
            )
    }

    /// Footer line: a typing indicator (left) and the signed-in user (right).
    fn chat_footer(typing: usize, username: Option<SharedString>) -> impl IntoElement {
        let typing_label: Option<SharedString> = match typing {
            0 => None,
            1 => Some("Someone is typing…".into()),
            n => Some(format!("{n} people are typing…").into()),
        };
        h_flex()
            .h(px(space::HEADER))
            .w_full()
            .px(px(space::LG))
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(color::border())
            .child(
                div()
                    .text_color(color::text_muted())
                    .text_size(px(font::SM))
                    .children(typing_label),
            )
            .children(username.map(|name| {
                div()
                    .text_color(color::text_faint())
                    .text_size(px(font::SM))
                    .child(SharedString::from(format!("Signed in as {name}")))
            }))
    }

    /// A centered placeholder pane (header + title + body) for unbuilt views.
    fn placeholder_pane(title: &'static str, body: &'static str) -> impl IntoElement {
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
                    ),
            )
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
            .child(self.content(cx))
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

/// Servers plus their channels, fetched together on login.
struct InitialData {
    servers: Vec<Server>,
    channels: Vec<(Uuid, Vec<concord_shared::types::Channel>)>,
}

/// Load the server list and each server's channels. A failed per-server channel
/// fetch is logged and skipped rather than failing the whole load.
async fn load_servers_and_channels(base: &str, token: &str) -> Result<InitialData, api::ApiError> {
    let servers = api::list_servers(base, token).await?;
    let mut channels = Vec::with_capacity(servers.len());
    for server in &servers {
        match api::list_channels(base, token, server.id).await {
            Ok(list) => channels.push((server.id, list)),
            Err(err) => {
                tracing::warn!(server = %server.id, error = %err, "failed to load channels");
            }
        }
    }
    Ok(InitialData { servers, channels })
}
