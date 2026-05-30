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

use std::collections::HashSet;
use std::rc::Rc;

use chrono::{DateTime, Local, NaiveDate, Utc};
use gpui::*;
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, Icon, IconName, Sizable};
use uuid::Uuid;

use concord_shared::protocol::{ServerMsg, Token};
use concord_shared::types::{
    Channel, ChannelCategory, ChannelType, MessageAuthor, MessageWithAuthor, Server,
};

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
/// How many rows from the top of the message list trigger loading an older
/// page. Kept well below a page so the fetch starts before the user reaches the
/// very top.
const LOAD_OLDER_THRESHOLD: usize = 8;
/// Pixels of off-screen content the message list measures above and below the
/// viewport, to soften pop-in while scrolling.
const MESSAGE_LIST_OVERDRAW: f32 = 300.0;
/// Minutes between two messages from the same author beyond which the later one
/// starts a fresh header instead of joining the previous group.
const GROUP_GAP_MINUTES: i64 = 7;

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
    /// Channel categories the user has collapsed in the sidebar, by id. Pure
    /// view state: it survives re-renders but is not persisted across sessions.
    collapsed_categories: HashSet<Uuid>,

    /// Virtualized list backing the chat pane. Bottom-aligned and tail-following
    /// like a chat log; its item set is kept in lockstep with
    /// [`Self::message_rows`] by [`Self::sync_messages`].
    message_list: ListState,
    /// The rows the list renders — date separators interleaved with messages —
    /// rebuilt whenever the chat state changes. Shared into the list's render
    /// closure as an `Rc`.
    message_rows: Rc<Vec<MessageRow>>,
    /// Channel whose history is currently mirrored into `message_list`, used to
    /// tell a channel switch (reset) from an in-place update (splice).
    synced_channel: Option<Uuid>,
    /// Set when messages arrive below the viewport while the user has scrolled
    /// up; surfaces the "new messages" jump button.
    unseen_messages: bool,

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

        // Bottom-aligned, tail-following list — a chat log. The scroll handler
        // drives scroll-back paging and dismisses the "new messages" hint once
        // the bottom is back in view.
        let message_list =
            ListState::new(0, ListAlignment::Bottom, px(MESSAGE_LIST_OVERDRAW));
        message_list.set_follow_mode(FollowMode::Tail);
        let weak = cx.weak_entity();
        message_list.set_scroll_handler(move |event, _window, cx| {
            let (start, end, count) =
                (event.visible_range.start, event.visible_range.end, event.count);
            let _ = weak.update(cx, |this, cx| this.on_message_scroll(start, end, count, cx));
        });

        // Re-render the layout whenever the auth view fires or any piece of
        // shared state changes; chat changes also reconcile the message list.
        let subscriptions = vec![
            cx.subscribe(&auth, Self::on_auth_event),
            cx.observe(&auth_state, |_, _, cx| cx.notify()),
            cx.observe(&servers, |_, _, cx| cx.notify()),
            cx.observe(&chat, |this, _, cx| {
                this.sync_messages(cx);
                cx.notify();
            }),
            cx.observe(&connection, |_, _, cx| cx.notify()),
        ];

        Self {
            screen: Screen::Auth,
            auth,
            nav: NavState::new(),
            collapsed_categories: HashSet::new(),
            message_list,
            message_rows: Rc::new(Vec::new()),
            synced_channel: None,
            unseen_messages: false,
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
            WsEvent::Connected { .. } => {
                // A reconnect (as opposed to the first connect) may have missed
                // live messages while the socket was down, so refetch the active
                // channel's newest page. The initial connect needs no refetch —
                // `load_initial_data` already loaded it. This replaces history
                // with the newest page, discarding any older pages the user had
                // scrolled back to; merging on reconnect is left to later work.
                let reconnected =
                    self.connection.read(cx).status() == ConnectionStatus::Reconnecting;
                self.connection.update(cx, |c, cx| {
                    c.connected();
                    cx.notify();
                });
                if reconnected {
                    if let Some(channel_id) = self.chat.read(cx).active_channel() {
                        self.load_history(channel_id, cx);
                    }
                }
            }
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
            for (server_id, categories) in data.categories {
                s.set_categories(server_id, categories);
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

    /// Switch the active server from the rail: show the servers view, select
    /// `server_id`, and — when it is a fresh selection — load its members and
    /// open its first text channel (mirroring the initial load). Channels were
    /// already fetched for every server on login, so no channel fetch is needed.
    fn select_server(&mut self, server_id: Uuid, cx: &mut Context<Self>) {
        self.nav.activate(View::Servers);
        let already_active = self.servers.read(cx).active_server() == Some(server_id);
        self.servers.update(cx, |s, cx| {
            s.set_active(server_id);
            cx.notify();
        });
        if already_active {
            // Re-clicking the active server just re-reveals the servers view;
            // the servers.update above already notified our observer.
            return;
        }

        if self.servers.read(cx).members_for(server_id).is_empty() {
            self.load_members(server_id, cx);
        }
        let first_channel = self
            .servers
            .read(cx)
            .channels_for(server_id)
            .iter()
            .find(|c| c.channel_type == ChannelType::Text)
            .map(|c| c.id);
        if let Some(channel_id) = first_channel {
            self.open_channel(channel_id, cx);
        } else {
            // Voice-only server, or its channel fetch failed at login: clear
            // the chat so it stops showing the previous server's messages.
            self.chat.update(cx, |c, cx| {
                c.close_channel();
                cx.notify();
            });
        }
        cx.notify();
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

    /// Collapse or expand a sidebar channel category, toggling its membership in
    /// the collapsed set.
    fn toggle_category(&mut self, category_id: Uuid, cx: &mut Context<Self>) {
        if !self.collapsed_categories.insert(category_id) {
            self.collapsed_categories.remove(&category_id);
        }
        cx.notify();
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

    // -- Message list -----------------------------------------------------

    /// Reconcile the virtualized [`Self::message_list`] with the current chat
    /// state. A channel switch resets the list and re-arms tail-following; an
    /// in-place change (new message, older page, edit, delete) is applied as the
    /// minimal splice, so cached measurements and the scroll position survive.
    fn sync_messages(&mut self, cx: &mut Context<Self>) {
        let active = self.chat.read(cx).active_channel();
        let today = Local::now().date_naive();
        let new_rows = build_message_rows(self.chat.read(cx).messages(), today);

        if active != self.synced_channel {
            self.message_list.reset(new_rows.len());
            self.message_list.set_follow_mode(FollowMode::Tail);
            self.synced_channel = active;
            self.unseen_messages = false;
            self.message_rows = Rc::new(new_rows);
            return;
        }

        if let Some((range, count)) = diff_splice(&self.message_rows, &new_rows) {
            // A non-empty insert at the very end, with the viewport scrolled up,
            // is a freshly arrived message the user has not seen.
            let appended_at_tail = range.start == self.message_rows.len() && range.is_empty();
            if appended_at_tail && count > 0 && !self.message_list.is_following_tail() {
                self.unseen_messages = true;
            }
            self.message_list.splice(range, count);
        }
        self.message_rows = Rc::new(new_rows);
    }

    /// React to a user scroll of the message list: pull an older page when near
    /// the top, and clear the "new messages" hint once the bottom is back in
    /// view. Must not touch `message_list` — it runs while that element holds
    /// the list state mutably borrowed.
    fn on_message_scroll(
        &mut self,
        visible_start: usize,
        visible_end: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        let (has_more, loading, active) = {
            let chat = self.chat.read(cx);
            (chat.has_more(), chat.is_loading(), chat.active_channel())
        };
        if visible_start <= LOAD_OLDER_THRESHOLD && has_more && !loading {
            if let Some(channel_id) = active {
                self.load_older(channel_id, cx);
            }
        }
        if self.unseen_messages && visible_end >= count {
            self.unseen_messages = false;
            cx.notify();
        }
    }

    /// Fetch the page just older than the oldest loaded message and prepend it.
    /// Guards on `has_more`/`is_loading` so the frequent scroll handler can call
    /// it freely; the prepend splice preserves the visible scroll position.
    fn load_older(&mut self, channel_id: Uuid, cx: &mut Context<Self>) {
        let before = match self.chat.read(cx).oldest_cursor() {
            Some(before) => before,
            None => return,
        };
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        self.chat.update(cx, |c, cx| {
            c.set_loading(true);
            cx.notify();
        });

        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        api::runtime().spawn(async move {
            let result =
                api::list_messages(&base, &token, channel_id, Some(before), Some(MESSAGE_PAGE)).await;
            let _ = tx.send(result);
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.chat.update(cx, |c, cx| {
                    match outcome {
                        Ok(Ok(page)) => {
                            // A full page means older messages may still remain.
                            let has_more = page.len() as i64 == MESSAGE_PAGE;
                            c.prepend_older(channel_id, page, has_more);
                        }
                        Ok(Err(err)) => {
                            if c.active_channel() == Some(channel_id) {
                                c.set_loading(false);
                            }
                            tracing::warn!(error = %err, "failed to load older messages");
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

    /// Snap the list back to the newest message and re-arm tail-following,
    /// dismissing the "new messages" hint.
    fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.message_list.set_follow_mode(FollowMode::Tail);
        self.unseen_messages = false;
        cx.notify();
    }

    // -- Layout -----------------------------------------------------------

    /// Leftmost rail: a Discord-style column of server icons. A home / DM
    /// shortcut sits on top, the servers scroll in the middle, and the
    /// "add server" and settings buttons are pinned to the bottom.
    fn server_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on_servers = self.nav.is_active(View::Servers);
        let active_server = self.servers.read(cx).active_server();
        let servers: Vec<(Uuid, SharedString)> = self
            .servers
            .read(cx)
            .servers()
            .iter()
            .map(|s| (s.id, SharedString::from(s.name.clone())))
            .collect();

        // The server list fills the space between the fixed top and bottom
        // buttons and scrolls when it overflows.
        let mut list = v_flex()
            .id("server-list")
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_y_scroll()
            .py(px(space::SM))
            .gap(px(space::SM))
            .items_center();
        for (id, name) in servers {
            // Only the rail's own view marks a server active, so DMs / settings
            // don't leave a server highlighted.
            let selected = on_servers && active_server == Some(id);
            list = list.child(Self::server_button(id, name, selected, cx));
        }

        v_flex()
            .w(px(space::SERVER_RAIL))
            .h_full()
            .flex_shrink_0()
            .bg(color::server_rail())
            .py(px(space::MD))
            .gap(px(space::SM))
            .items_center()
            .child(Self::nav_button(
                View::DirectMessages,
                IconName::Inbox,
                "Direct Messages",
                self.nav.is_active(View::DirectMessages),
                cx,
            ))
            .child(Self::rail_divider())
            .child(list)
            .child(Self::add_server_button(cx))
            .child(Self::nav_button(
                View::Settings,
                IconName::Settings,
                "Settings",
                self.nav.is_active(View::Settings),
                cx,
            ))
    }

    /// The hairline rule separating the home shortcut from the server list.
    fn rail_divider() -> impl IntoElement {
        div()
            .w(px(space::RAIL_DIVIDER))
            .h(px(2.0))
            .flex_shrink_0()
            .rounded_full()
            .bg(color::border())
    }

    /// The white pill that marks the active rail item, hugging the rail's
    /// left edge. Inactive items reserve no height so the icons stay aligned.
    fn rail_pill(active: bool) -> impl IntoElement {
        div()
            .absolute()
            .left(px(0.0))
            .top(px((space::RAIL_BUTTON - space::RAIL_PILL_HEIGHT) / 2.0))
            .w(px(space::RAIL_PILL_WIDTH))
            .h(px(if active { space::RAIL_PILL_HEIGHT } else { 0.0 }))
            .rounded_full()
            .bg(color::interactive_active())
    }

    /// One rail slot: an active pill plus a round, clickable button holding
    /// `content`, with a hover tooltip. The button rounds into a squircle when
    /// active or hovered; `accent` swaps the idle look to the brand green used
    /// by the "add server" affordance.
    fn rail_item(
        id: impl Into<ElementId>,
        active: bool,
        accent: bool,
        tooltip: SharedString,
        content: AnyElement,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let (bg, fg) = if accent {
            (color::elevated(), color::online())
        } else if active {
            (color::accent(), color::interactive_active())
        } else {
            (color::elevated(), color::text())
        };
        div()
            .relative()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .child(Self::rail_pill(active))
            .child(
                div()
                    .id(id)
                    .size(px(space::RAIL_BUTTON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(if active {
                        space::LG
                    } else {
                        space::RAIL_BUTTON / 2.0
                    }))
                    .bg(bg)
                    .text_color(fg)
                    .hover(move |s| {
                        s.rounded(px(space::LG))
                            .bg(if accent { color::online() } else { color::accent() })
                            .text_color(color::interactive_active())
                    })
                    .cursor_pointer()
                    .child(content)
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx))),
            )
    }

    /// A rail button bound to a top-level [`View`] (the home / DM shortcut and
    /// settings). Clicking it activates `view`.
    fn nav_button(
        view: View,
        icon: IconName,
        tooltip: &'static str,
        active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = Icon::new(icon)
            .with_size(px(space::RAIL_ICON))
            .into_any_element();
        Self::rail_item(tooltip, active, false, tooltip.into(), content, cx, move |this, _, cx| {
            this.nav.activate(view);
            cx.notify();
        })
    }

    /// A server icon: the server's first initial (image icons land later).
    /// Clicking it switches to that server.
    fn server_button(
        id: Uuid,
        name: SharedString,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let initial: SharedString = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_else(|| "?".into())
            .into();
        let content = div()
            .text_size(px(font::LG))
            .font_weight(FontWeight::SEMIBOLD)
            .child(initial)
            .into_any_element();
        Self::rail_item(
            SharedString::from(id.to_string()),
            selected,
            false,
            name,
            content,
            cx,
            move |this, _, cx| this.select_server(id, cx),
        )
    }

    /// The "add a server" button pinned below the server list. Server creation
    /// is a separate piece of work; the affordance lives here so the rail is
    /// complete.
    fn add_server_button(cx: &mut Context<Self>) -> impl IntoElement {
        let content = Icon::new(IconName::Plus)
            .with_size(px(space::RAIL_ICON))
            .into_any_element();
        Self::rail_item(
            "add-server",
            false,
            true,
            "Add a Server".into(),
            content,
            cx,
            |_, _, _| tracing::debug!("add-server clicked; creation flow lands in later work"),
        )
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

    /// The active server's channels, grouped under collapsible category headers.
    /// Uncategorized channels render first (Discord-style, with no header), then
    /// each category in turn; loading and empty states replace the whole list.
    fn channel_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_channel = self.chat.read(cx).active_channel();
        // Collect owned data first so the `servers` borrow is dropped before the
        // per-row `cx.listener` calls reborrow `cx`.
        let (loading, channels, categories) = {
            let servers = self.servers.read(cx);
            let channels: Vec<(Uuid, String, ChannelType, Option<Uuid>)> = servers
                .active_channels()
                .iter()
                .map(|c| (c.id, c.name.clone(), c.channel_type, c.category_id))
                .collect();
            let categories: Vec<(Uuid, String)> = servers
                .active_categories()
                .iter()
                .map(|c| (c.id, c.name.clone()))
                .collect();
            (servers.is_loading(), channels, categories)
        };

        let mut list = v_flex()
            .id("channel-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p(px(space::SM))
            .gap(px(space::XS));

        if loading {
            return list.child(Self::muted_row("Loading…"));
        }
        if channels.is_empty() && categories.is_empty() {
            return list.child(Self::muted_row("No channels yet."));
        }

        // A channel is "ungrouped" when it has no category, or references one
        // that did not load; those render at the top with no header so a failed
        // category fetch never hides channels entirely.
        let known: HashSet<Uuid> = categories.iter().map(|(id, _)| *id).collect();
        for (id, name, channel_type, _) in channels
            .iter()
            .filter(|c| c.3.is_none_or(|cid| !known.contains(&cid)))
        {
            let selected = active_channel == Some(*id);
            list = list.child(self.channel_row(*id, name, *channel_type, selected, cx));
        }

        // Then each category, with its channels nested under a collapsible head.
        for (category_id, category_name) in &categories {
            let collapsed = self.collapsed_categories.contains(category_id);
            list = list.child(self.category_header(*category_id, category_name, collapsed, cx));
            if collapsed {
                continue;
            }
            for (id, name, channel_type, _) in channels.iter().filter(|c| c.3 == Some(*category_id))
            {
                let selected = active_channel == Some(*id);
                list = list.child(self.channel_row(*id, name, *channel_type, selected, cx));
            }
        }
        list
    }

    /// A clickable category header: a chevron (down when expanded, right when
    /// collapsed) beside the uppercased category name. Clicking toggles whether
    /// the category's channels are shown.
    fn category_header(
        &self,
        id: Uuid,
        name: &str,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let chevron = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        let label = SharedString::from(name.to_uppercase());
        h_flex()
            .id(SharedString::from(format!("category-{id}")))
            .w_full()
            .mt(px(space::SM))
            .px(px(space::XS))
            .py(px(space::XS))
            .gap(px(space::XS))
            .items_center()
            .text_color(color::text_muted())
            .text_size(px(font::SM))
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|s| s.text_color(color::text()))
            .cursor_pointer()
            .child(Icon::new(chevron).with_size(px(space::MD)))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_category(id, cx)))
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

    /// The chat pane: header, the virtualized message list, and a footer with
    /// typing and connection status.
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

        let (loading, empty) = {
            let chat = self.chat.read(cx);
            (chat.is_loading(), chat.is_empty())
        };
        let typing = self.chat.read(cx).typing_count();
        let status = self.connection.read(cx).status();
        let username = self
            .auth_state
            .read(cx)
            .user()
            .map(|u| SharedString::from(u.username.clone()));

        let body: AnyElement = if active_channel.is_none() {
            Self::message_notice("Pick a channel from the sidebar.").into_any_element()
        } else if empty && loading {
            Self::message_notice("Loading messages…").into_any_element()
        } else if empty {
            Self::message_notice("No messages yet — say hello!").into_any_element()
        } else {
            self.message_list_area(cx).into_any_element()
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

    /// The scrollable list of messages, overlaid with the "new messages" jump
    /// button while the user is scrolled up past freshly arrived messages.
    fn message_list_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.message_rows.clone();
        let list = list(self.message_list.clone(), move |ix, _window, _cx| {
            rows.get(ix)
                .map(render_message_row)
                .unwrap_or_else(|| div().into_any_element())
        })
        .flex_1()
        .py(px(space::SM));

        let mut area = v_flex().relative().flex_1().min_h(px(0.0)).child(list);
        if self.unseen_messages {
            area = area.child(self.jump_to_latest_button(cx));
        }
        area
    }

    /// The pill that drops the user back to the newest messages, shown floating
    /// above the footer when there are unseen messages below the viewport.
    fn jump_to_latest_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("jump-to-latest")
            .absolute()
            .bottom(px(space::SM))
            .right(px(space::LG))
            .px(px(space::MD))
            .py(px(space::XS))
            .rounded(px(space::LG))
            .bg(color::accent())
            .text_color(color::interactive_active())
            .text_size(px(font::SM))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.bg(color::accent_hover()))
            .child("New messages ↓")
            .on_click(cx.listener(|this, _, _, cx| this.jump_to_latest(cx)))
    }

    /// A muted, centered notice filling the message area (no channel, loading,
    /// or empty states).
    fn message_notice(text: impl Into<SharedString>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h(px(0.0))
            .items_center()
            .justify_center()
            .text_color(color::text_muted())
            .text_size(px(font::MD))
            .child(text.into())
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

/// Servers plus their channels and categories, fetched together on login.
struct InitialData {
    servers: Vec<Server>,
    channels: Vec<(Uuid, Vec<Channel>)>,
    categories: Vec<(Uuid, Vec<ChannelCategory>)>,
}

/// Load the server list and, for each server, its channels and categories. The
/// per-server fetches all run concurrently; a failed channel or category fetch
/// is logged and skipped rather than failing the whole load.
async fn load_servers_and_channels(base: &str, token: &str) -> Result<InitialData, api::ApiError> {
    let servers = api::list_servers(base, token).await?;
    let fetches = servers.iter().map(|server| {
        let id = server.id;
        async move {
            let (channels, categories) = futures_util::future::join(
                api::list_channels(base, token, id),
                api::list_categories(base, token, id),
            )
            .await;
            let channels = match channels {
                Ok(list) => Some((id, list)),
                Err(err) => {
                    tracing::warn!(server = %id, error = %err, "failed to load channels");
                    None
                }
            };
            let categories = match categories {
                Ok(list) => Some((id, list)),
                Err(err) => {
                    tracing::warn!(server = %id, error = %err, "failed to load categories");
                    None
                }
            };
            (channels, categories)
        }
    });
    let (channels, categories) = futures_util::future::join_all(fetches).await.into_iter().fold(
        (Vec::new(), Vec::new()),
        |(mut channels, mut categories), (channel, category)| {
            channels.extend(channel);
            categories.extend(category);
            (channels, categories)
        },
    );
    Ok(InitialData { servers, channels, categories })
}

/// One rendered row in the message list: a day separator, or a message that
/// carries an author/time header only when it opens a group. Equality drives the
/// splice diff, so it deliberately covers every field that affects a row's
/// rendered height (header, content, "edited" marker).
#[derive(Clone, PartialEq)]
enum MessageRow {
    DateSeparator {
        label: SharedString,
    },
    Message {
        id: Uuid,
        author: SharedString,
        timestamp: SharedString,
        content: SharedString,
        show_header: bool,
        edited: bool,
    },
}

/// Flatten loaded messages (oldest first) into renderable rows: a date
/// separator before the first message of each calendar day, then one row per
/// message. Consecutive messages from the same author within
/// [`GROUP_GAP_MINUTES`] are "grouped" — only the first carries a header.
fn build_message_rows(messages: &[MessageWithAuthor], today: NaiveDate) -> Vec<MessageRow> {
    let yesterday = today.pred_opt();
    let mut rows = Vec::with_capacity(messages.len());
    let mut prev_date: Option<NaiveDate> = None;
    let mut prev_author: Option<Uuid> = None;
    let mut prev_at: Option<DateTime<Utc>> = None;

    for m in messages {
        let local = m.created_at.with_timezone(&Local);
        let date = local.date_naive();
        let new_day = prev_date != Some(date);
        if new_day {
            rows.push(MessageRow::DateSeparator {
                label: date_label(date, today, yesterday).into(),
            });
        }

        let author_id = m.author.as_ref().map(|a| a.id);
        let gap = prev_at.is_none_or(|p| (m.created_at - p).num_minutes() >= GROUP_GAP_MINUTES);
        let show_header = new_day || author_id != prev_author || gap;

        let author = m
            .author
            .as_ref()
            .map(|a| a.username.clone())
            .unwrap_or_else(|| "unknown".into());
        rows.push(MessageRow::Message {
            id: m.id,
            author: author.into(),
            timestamp: local.format("%H:%M").to_string().into(),
            content: m.content.clone().into(),
            show_header,
            edited: m.edited_at.is_some(),
        });

        prev_date = Some(date);
        prev_author = author_id;
        prev_at = Some(m.created_at);
    }
    rows
}

/// A human label for a day separator: "Today" / "Yesterday" for the obvious
/// cases, otherwise an absolute date like "May 30, 2026".
fn date_label(date: NaiveDate, today: NaiveDate, yesterday: Option<NaiveDate>) -> String {
    if date == today {
        "Today".to_string()
    } else if Some(date) == yesterday {
        "Yesterday".to_string()
    } else {
        date.format("%B %-d, %Y").to_string()
    }
}

/// The minimal splice turning `old` into `new`: the range of `old` to replace
/// and how many `new` rows replace it, or `None` when they are identical.
/// Messages only grow at the head (older pages) or tail (live messages), with
/// the occasional in-place edit or delete, so a common-prefix / common-suffix
/// diff captures every case in a single splice.
fn diff_splice(old: &[MessageRow], new: &[MessageRow]) -> Option<(std::ops::Range<usize>, usize)> {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let range = prefix..(old.len() - suffix);
    let count = new.len() - suffix - prefix;
    if range.is_empty() && count == 0 {
        None
    } else {
        Some((range, count))
    }
}

/// Render a single list row.
fn render_message_row(row: &MessageRow) -> AnyElement {
    match row {
        MessageRow::DateSeparator { label } => render_date_separator(label.clone()),
        MessageRow::Message {
            author,
            timestamp,
            content,
            show_header,
            edited,
            ..
        } => render_message(
            author.clone(),
            timestamp.clone(),
            content.clone(),
            *show_header,
            *edited,
        ),
    }
}

/// A day separator: the label centered between two hairline rules.
fn render_date_separator(label: SharedString) -> AnyElement {
    let rule = || div().flex_1().h(px(1.0)).bg(color::border());
    h_flex()
        .w_full()
        .px(px(space::MD))
        .py(px(space::SM))
        .items_center()
        .gap(px(space::SM))
        .child(rule())
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(font::SM))
                .text_color(color::text_muted())
                .font_weight(FontWeight::SEMIBOLD)
                .child(label),
        )
        .child(rule())
        .into_any_element()
}

/// A message row: an author/time header for group openers, then the content
/// (with a trailing "(edited)" marker when applicable).
fn render_message(
    author: SharedString,
    timestamp: SharedString,
    content: SharedString,
    show_header: bool,
    edited: bool,
) -> AnyElement {
    let mut content_line = h_flex().w_full().items_baseline().gap(px(space::SM)).child(
        div()
            .text_color(color::text())
            .text_size(px(font::MD))
            .child(content),
    );
    if edited {
        content_line = content_line.child(
            div()
                .flex_shrink_0()
                .text_size(px(font::SM))
                .text_color(color::text_faint())
                .child("(edited)"),
        );
    }

    let mut col = v_flex().w_full().px(px(space::MD)).gap(px(space::XS));
    col = if show_header {
        col.pt(px(space::SM))
    } else {
        col.pt(px(2.0))
    };
    if show_header {
        col = col.child(
            h_flex()
                .items_baseline()
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
        );
    }
    col.child(content_line).into_any_element()
}

#[cfg(test)]
mod tests {
    // Import only what the tests need rather than `use super::*`: the latter
    // re-globs `gpui::*` into this module, which blows the recursion limit when
    // the `#[test]` harness expands.
    use super::{build_message_rows, diff_splice, MessageRow};

    use chrono::{DateTime, Local, TimeZone, Utc};
    use concord_shared::types::{MessageAuthor, MessageWithAuthor};
    use uuid::Uuid;

    fn at(year: i32, month: u32, day: u32, hour: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, min, 0).unwrap()
    }

    fn msg(id: u8, author: Option<u8>, content: &str, created_at: DateTime<Utc>) -> MessageWithAuthor {
        let with_byte = |n: u8| {
            let mut bytes = [0u8; 16];
            bytes[15] = n;
            Uuid::from_bytes(bytes)
        };
        MessageWithAuthor {
            id: with_byte(id),
            channel_id: Uuid::nil(),
            author: author.map(|a| MessageAuthor {
                id: with_byte(a),
                username: "alice".into(),
                avatar_url: None,
            }),
            content: content.into(),
            edited_at: None,
            created_at,
        }
    }

    fn is_header(row: &MessageRow, expected: bool) -> bool {
        matches!(row, MessageRow::Message { show_header, .. } if *show_header == expected)
    }

    fn separators(rows: &[MessageRow]) -> usize {
        rows.iter()
            .filter(|r| matches!(r, MessageRow::DateSeparator { .. }))
            .count()
    }

    #[test]
    fn groups_consecutive_same_author_messages() {
        let t = at(2026, 5, 30, 12, 0);
        let rows = build_message_rows(
            &[msg(1, Some(1), "hi", t), msg(2, Some(1), "again", t)],
            Local::now().date_naive(),
        );
        // One separator, then a header opener and a grouped (header-less) reply.
        assert_eq!(rows.len(), 3);
        assert_eq!(separators(&rows), 1);
        assert!(is_header(&rows[1], true));
        assert!(is_header(&rows[2], false));
    }

    #[test]
    fn separates_messages_across_days() {
        let rows = build_message_rows(
            &[
                msg(1, Some(1), "old", at(2026, 5, 28, 12, 0)),
                msg(2, Some(1), "new", at(2026, 5, 30, 12, 0)),
            ],
            Local::now().date_naive(),
        );
        // A separator opens each day, and the second day's message gets a header.
        assert_eq!(separators(&rows), 2);
        assert!(is_header(&rows[3], true));
    }

    #[test]
    fn different_author_starts_a_new_group() {
        let t = at(2026, 5, 30, 12, 0);
        let rows = build_message_rows(
            &[msg(1, Some(1), "a", t), msg(2, Some(2), "b", t)],
            Local::now().date_naive(),
        );
        assert_eq!(rows.len(), 3);
        assert!(is_header(&rows[2], true));
    }

    #[test]
    fn long_gap_starts_a_new_group() {
        let rows = build_message_rows(
            &[
                msg(1, Some(1), "a", at(2026, 5, 30, 12, 0)),
                msg(2, Some(1), "b", at(2026, 5, 30, 12, 8)),
            ],
            Local::now().date_naive(),
        );
        assert!(is_header(&rows[2], true));
    }

    fn row_msg(id: u8, content: &str) -> MessageRow {
        let mut bytes = [0u8; 16];
        bytes[15] = id;
        MessageRow::Message {
            id: Uuid::from_bytes(bytes),
            author: "alice".into(),
            timestamp: "12:00".into(),
            content: content.into(),
            show_header: true,
            edited: false,
        }
    }

    #[test]
    fn diff_identical_is_none() {
        let rows = vec![row_msg(1, "a"), row_msg(2, "b")];
        assert_eq!(diff_splice(&rows, &rows), None);
    }

    #[test]
    fn diff_detects_tail_append() {
        let old = vec![row_msg(1, "a")];
        let new = vec![row_msg(1, "a"), row_msg(2, "b")];
        assert_eq!(diff_splice(&old, &new), Some((1..1, 1)));
    }

    #[test]
    fn diff_detects_head_prepend() {
        let old = vec![row_msg(2, "b")];
        let new = vec![row_msg(1, "a"), row_msg(2, "b")];
        assert_eq!(diff_splice(&old, &new), Some((0..0, 1)));
    }

    #[test]
    fn diff_detects_in_place_edit() {
        let old = vec![row_msg(1, "a"), row_msg(2, "b"), row_msg(3, "c")];
        let new = vec![row_msg(1, "a"), row_msg(2, "EDIT"), row_msg(3, "c")];
        assert_eq!(diff_splice(&old, &new), Some((1..2, 1)));
    }

    #[test]
    fn diff_detects_delete() {
        let old = vec![row_msg(1, "a"), row_msg(2, "b"), row_msg(3, "c")];
        let new = vec![row_msg(1, "a"), row_msg(3, "c")];
        assert_eq!(diff_splice(&old, &new), Some((1..2, 0)));
    }
}
