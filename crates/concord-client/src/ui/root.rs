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
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDate, Utc};
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, Icon, IconName, Sizable};
use uuid::Uuid;

use concord_shared::protocol::{ClientMsg, ServerMsg, Token};
use concord_shared::types::{
    Channel, ChannelCategory, ChannelType, DmChannelInfo, DmConversation, MemberInfo,
    MessageAuthor, MessageWithAuthor, Server, UserStatus,
};
use concord_shared::validation::validate_message_content;

use crate::api;
use crate::auth;
use crate::state::{
    AuthState, ChatState, ConnectionState, ConnectionStatus, DmsState, PresenceState, ServersState,
};
use crate::ui::auth_view::{AuthEvent, AuthView};
use crate::ui::group_dm_dialog::{GroupDmDialog, GroupDmEvent};
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
/// Side length of a message author's avatar; also the width of the blank gutter
/// that keeps grouped (header-less) messages aligned under it.
const AVATAR_SIZE: f32 = 40.0;
/// Side length of a member panel avatar.
const MEMBER_AVATAR_SIZE: f32 = 32.0;
/// Side length of a DM conversation-list avatar.
const DM_AVATAR_SIZE: f32 = 36.0;
/// Diameter of the presence dot overlaid on a member panel avatar.
const MEMBER_STATUS_DOT: f32 = 10.0;
/// Width of the ring that sets the presence dot off from the avatar.
const MEMBER_STATUS_RING: f32 = 3.0;
/// While the user keeps typing, re-announce `StartTyping` no more often than
/// this. Comfortably inside the server's typing TTL so the indicator never
/// lapses mid-sentence, but far above a keystroke so we don't flood the socket.
const TYPING_REFRESH: Duration = Duration::from_secs(3);
/// How long the composer must sit idle after the last keystroke before we send
/// `StopTyping` to clear the indicator for everyone else.
const TYPING_IDLE: Duration = Duration::from_secs(3);

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
    /// Whether the right-hand member list panel is shown. Pure view state,
    /// toggled from the chat header; defaults to shown, Discord-style.
    show_members: bool,

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

    /// The message composer at the foot of the chat pane.
    composer: Entity<InputState>,
    /// Channel the composer's placeholder currently names, so it is only
    /// rewritten on an actual channel switch rather than every chat change.
    composer_channel: Option<Uuid>,
    /// The message currently being edited inline, if any. Its row swaps its text
    /// for [`Self::editor`]; folded into the row model so the list re-measures
    /// the taller editor on entry and the restored text on exit.
    editing: Option<Uuid>,
    /// The single inline editor reused across rows: only the message named by
    /// [`Self::editing`] renders it, so one entity serves every row.
    editor: Entity<InputState>,
    /// Channel we currently have an open `StartTyping` session in, if any — so
    /// we know which channel to `StopTyping`, and whether a refresh is due.
    typing_channel: Option<Uuid>,
    /// When we last announced `StartTyping`, to throttle the refreshes.
    last_typing_sent: Option<Instant>,
    /// Bumped on every keystroke. The idle timer only sends `StopTyping` if this
    /// still matches when it wakes, so a later keystroke cancels an earlier arm.
    typing_seq: u64,

    // Shared application state, handed to views as entity handles.
    auth_state: Entity<AuthState>,
    servers: Entity<ServersState>,
    chat: Entity<ChatState>,
    connection: Entity<ConnectionState>,
    presence: Entity<PresenceState>,
    dms: Entity<DmsState>,

    /// The open "New Group DM" dialog, overlaid on the main layout, or `None`
    /// when it is closed.
    group_dm_dialog: Option<Entity<GroupDmDialog>>,
    /// Subscription to the open dialog's events; dropped (cancelling it) when
    /// the dialog closes.
    _dialog_subscription: Option<Subscription>,

    /// The live connection handle: outgoing messages are sent through it, and
    /// holding it keeps the background task's command channel open (the task
    /// exits once every handle drops).
    ws_handle: Option<ConnectionHandle>,
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
        let presence = cx.new(|_| PresenceState::new());
        let dms = cx.new(|_| DmsState::new());

        // The composer: an auto-growing textarea where a plain Enter submits and
        // Shift+Enter inserts a newline (`submit_on_enter` flips that default).
        let composer = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Message")
                .auto_grow(1, 6)
                .submit_on_enter(true)
        });

        // The inline message editor, shared across rows. Like the composer, a
        // plain Enter saves and Shift+Enter inserts a newline.
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });

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
            cx.subscribe_in(&composer, window, Self::on_composer_event),
            cx.subscribe_in(&editor, window, Self::on_editor_event),
            cx.observe(&auth_state, |_, _, cx| cx.notify()),
            cx.observe(&servers, |_, _, cx| cx.notify()),
            cx.observe_in(&chat, window, |this, _, window, cx| {
                this.sync_messages(cx);
                this.refresh_composer_placeholder(window, cx);
                cx.notify();
            }),
            cx.observe(&connection, |_, _, cx| cx.notify()),
            cx.observe(&presence, |_, _, cx| cx.notify()),
            cx.observe(&dms, |_, _, cx| cx.notify()),
        ];

        Self {
            screen: Screen::Auth,
            auth,
            nav: NavState::new(),
            collapsed_categories: HashSet::new(),
            show_members: true,
            message_list,
            message_rows: Rc::new(Vec::new()),
            synced_channel: None,
            unseen_messages: false,
            composer,
            composer_channel: None,
            editing: None,
            editor,
            typing_channel: None,
            last_typing_sent: None,
            typing_seq: 0,
            auth_state,
            servers,
            chat,
            connection,
            presence,
            dms,
            group_dm_dialog: None,
            _dialog_subscription: None,
            ws_handle: None,
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
        self.ws_handle = Some(handle.clone());
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
    /// represent are handled; server/membership/DM events are folded in by
    /// later work.
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
                let author_id = *author_id;
                self.chat.update(cx, |c, cx| {
                    // The server echoes our own messages back; if this confirms
                    // one we already showed optimistically, reconcile it in
                    // place instead of appending a duplicate.
                    if !c.confirm_optimistic(
                        message.channel_id,
                        message.id,
                        author_id,
                        &message.content,
                        message.created_at,
                    ) {
                        c.push_message(message);
                    }
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
            // Initial presence of our peers, sent once per (re)connect; it is
            // authoritative, so it replaces whatever we held.
            ServerMsg::PresenceSnapshot { users } => {
                let users = users.clone();
                self.presence.update(cx, |p, cx| {
                    p.set_snapshot(users);
                    cx.notify();
                });
            }
            ServerMsg::UserStatusChanged { user_id, status } => {
                let (user_id, status) = (*user_id, *status);
                self.presence.update(cx, |p, cx| {
                    p.set_status(user_id, status);
                    cx.notify();
                });
            }
            ServerMsg::NewDirectMessage {
                id,
                dm_channel_id,
                author_id,
                content,
                created_at,
            } => {
                let me = self.auth_state.read(cx).user().map(|u| u.id);
                let from_me = matches!((*author_id, me), (Some(a), Some(m)) if a == m);
                // The wire message carries no author profile; resolve it against
                // the conversation's participants (or the signed-in user).
                let author = (*author_id).and_then(|aid| self.resolve_dm_author(*dm_channel_id, aid, cx));
                // Refresh the conversation list: bump the preview, reorder, and
                // flag unread unless this DM is open or the message is our own.
                self.dms.update(cx, |d, cx| {
                    d.apply_new_message(
                        *dm_channel_id,
                        *id,
                        author.clone(),
                        content.clone(),
                        *created_at,
                        from_me,
                    );
                    cx.notify();
                });
                // When the DM is the open chat, fold it into the message list the
                // same way a channel `NewMessage` is — reconciling our own echo.
                if self.chat.read(cx).active_channel() == Some(*dm_channel_id) {
                    let message = MessageWithAuthor {
                        id: *id,
                        channel_id: *dm_channel_id,
                        author,
                        content: content.clone(),
                        edited_at: None,
                        created_at: *created_at,
                    };
                    let author_id = *author_id;
                    self.chat.update(cx, |c, cx| {
                        if !c.confirm_optimistic(
                            message.channel_id,
                            message.id,
                            author_id,
                            &message.content,
                            message.created_at,
                        ) {
                            c.push_message(message);
                        }
                        cx.notify();
                    });
                }
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

    /// Author profile for a live DM message: the signed-in user, or one of the
    /// conversation's participants, else `None` (a deleted account, or a
    /// conversation we don't hold).
    fn resolve_dm_author(
        &self,
        dm_channel_id: Uuid,
        author_id: Uuid,
        cx: &Context<Self>,
    ) -> Option<MessageAuthor> {
        if let Some(user) = self.auth_state.read(cx).user() {
            if user.id == author_id {
                return Some(MessageAuthor {
                    id: user.id,
                    username: user.username.clone(),
                    avatar_url: user.avatar_url.clone(),
                });
            }
        }
        self.dms
            .read(cx)
            .conversation(dm_channel_id)
            .and_then(|conv| {
                conv.participants
                    .iter()
                    .find(|p| p.user_id == author_id)
                    .map(|p| MessageAuthor {
                        id: p.user_id,
                        username: p.username.clone(),
                        avatar_url: p.avatar_url.clone(),
                    })
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

    /// Show or hide the right-hand member list panel.
    fn toggle_members(&mut self, cx: &mut Context<Self>) {
        self.show_members = !self.show_members;
        cx.notify();
    }

    /// Start a direct message with `user_id`, clicked from the member panel.
    /// Opening a DM with a specific *user* needs the find-or-create endpoint,
    /// which lands in later work; existing conversations are reachable today
    /// from the DM view's list. For now this only records the intent.
    fn open_dm(&mut self, user_id: Uuid, _cx: &mut Context<Self>) {
        tracing::debug!(%user_id, "start-DM clicked; opening a DM with a user lands in later work");
    }

    /// Fetch the DM conversation list once, the first time the user opens the DM
    /// view. Already-loaded or in-flight lists are left alone; live
    /// `NewDirectMessage`s keep the list current afterwards.
    fn ensure_dms_loaded(&mut self, cx: &mut Context<Self>) {
        let dms = self.dms.read(cx);
        if dms.is_loaded() || dms.is_loading() {
            return;
        }
        self.load_dms(cx);
    }

    /// Load `GET /api/dms` into the DM state. A failure clears the spinner and is
    /// logged; the next view entry will not retry (the list is marked loaded only
    /// on success), but a reconnect's live messages still flow in.
    fn load_dms(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        self.dms.update(cx, |d, cx| {
            d.set_loading(true);
            cx.notify();
        });

        let base = auth::api_base_url();
        let (tx, rx) = tokio::sync::oneshot::channel();
        api::runtime().spawn(async move {
            let _ = tx.send(api::list_dms(&base, &token).await);
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.dms.update(cx, |d, cx| {
                    match outcome {
                        Ok(Ok(list)) => d.set_conversations(list),
                        Ok(Err(err)) => {
                            tracing::warn!(error = %err, "failed to load DM conversations");
                            d.set_loading(false);
                        }
                        Err(_canceled) => d.set_loading(false),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Open an existing DM conversation from the list: select it (clearing its
    /// unread dot), tell the server it has been read, and open its channel in the
    /// chat pane so its history loads — DM channels share the message endpoint
    /// and `ChatState` with server channels.
    fn open_dm_conversation(&mut self, dm_channel_id: Uuid, cx: &mut Context<Self>) {
        self.dms.update(cx, |d, cx| {
            d.set_active(dm_channel_id);
            cx.notify();
        });
        self.mark_dm_read_remote(dm_channel_id, cx);
        self.open_channel(dm_channel_id, cx);
    }

    /// Open the "New Group DM" dialog over the main layout, subscribing to its
    /// events. A dialog already open is left in place. Needs a signed-in session
    /// (its requests carry the token); without one it is a no-op.
    fn open_group_dm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.group_dm_dialog.is_some() {
            return;
        }
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        let dialog = cx.new(|cx| GroupDmDialog::new(token, window, cx));
        self._dialog_subscription = Some(cx.subscribe(&dialog, Self::on_group_dm_event));
        self.group_dm_dialog = Some(dialog);
        cx.notify();
    }

    /// Tear down the open dialog (and its subscription).
    fn close_group_dm_dialog(&mut self, cx: &mut Context<Self>) {
        self.group_dm_dialog = None;
        self._dialog_subscription = None;
        cx.notify();
    }

    /// React to the group-DM dialog: on creation, fold the new channel into the
    /// DM list, close the dialog, and open the conversation; on dismissal, just
    /// close it.
    fn on_group_dm_event(
        &mut self,
        _dialog: Entity<GroupDmDialog>,
        event: &GroupDmEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            GroupDmEvent::Created(info) => {
                let conversation = conversation_from_info(info.clone());
                let id = conversation.id;
                self.dms.update(cx, |d, cx| {
                    d.upsert_conversation(conversation);
                    cx.notify();
                });
                // The dialog opens from the DM view, but make the switch explicit
                // so the new conversation lands on screen wherever it fired from.
                self.nav.activate(View::DirectMessages);
                self.close_group_dm_dialog(cx);
                self.open_dm_conversation(id, cx);
            }
            GroupDmEvent::Dismissed => self.close_group_dm_dialog(cx),
        }
    }

    /// Tell the server a DM has been read. Fire-and-forget on the shared runtime:
    /// the local unread dot is already cleared, so a failure only means the flag
    /// reappears on the next fetch.
    fn mark_dm_read_remote(&self, dm_channel_id: Uuid, cx: &Context<Self>) {
        let Some(token) = self.auth_state.read(cx).access_token().map(str::to_owned) else {
            return;
        };
        let base = auth::api_base_url();
        api::runtime().spawn(async move {
            if let Err(err) = api::mark_dm_read(&base, &token, dm_channel_id).await {
                tracing::warn!(error = %err, "failed to mark DM read");
            }
        });
    }

    /// Whether the open chat is a DM (as opposed to a server channel). True when
    /// a DM conversation is selected and it is the channel `ChatState` holds, so
    /// it drives send routing, the title, and the suppressed member toggle alike.
    fn active_is_dm(&self, cx: &Context<Self>) -> bool {
        let active_dm = self.dms.read(cx).active();
        active_dm.is_some() && active_dm == self.chat.read(cx).active_channel()
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
        let new_rows = build_message_rows(self.chat.read(cx).messages(), today, self.editing);

        if active != self.synced_channel {
            // A channel switch abandons any in-progress edit; its row is gone.
            self.editing = None;
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

    // -- Composer ---------------------------------------------------------

    /// Handle composer input events: a plain Enter sends the message, Shift+Enter
    /// has already inserted a newline, and any change drives the typing indicator.
    fn on_composer_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.on_composer_change(cx),
            // A plain Enter submits; Shift+Enter has already inserted a newline.
            InputEvent::PressEnter { shift, .. } if !shift => self.send_message(window, cx),
            _ => {}
        }
    }

    // -- Message actions --------------------------------------------------

    /// React to the inline editor: a plain Enter saves the edit; losing focus
    /// (clicking away) cancels it. Shift+Enter falls through to insert a newline.
    fn on_editor_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { shift, .. } if !shift => self.save_edit(window, cx),
            InputEvent::Blur => self.cancel_edit(cx),
            _ => {}
        }
    }

    /// Open the inline editor on `message_id`, seeding it with the current text
    /// and focusing it. Re-syncs the rows so the editor's row re-measures.
    fn start_editing(&mut self, message_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = self
            .chat
            .read(cx)
            .messages()
            .iter()
            .find(|m| m.id == message_id)
            .map(|m| m.content.clone())
        else {
            return;
        };
        self.editing = Some(message_id);
        self.editor.update(cx, |editor, cx| {
            editor.set_value(content, window, cx);
            editor.focus(window, cx);
        });
        self.sync_messages(cx);
        cx.notify();
    }

    /// Commit the inline edit: validate, echo the change locally, and send it.
    /// An unchanged or blank/over-long value just closes the editor without a
    /// round-trip — the server would reject the latter and ignore the former.
    fn save_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(message_id) = self.editing else {
            return;
        };
        let content = self.editor.read(cx).value().trim().to_string();
        let unchanged = self
            .chat
            .read(cx)
            .messages()
            .iter()
            .find(|m| m.id == message_id)
            .is_some_and(|m| m.content == content);
        if unchanged || validate_message_content(&content).is_err() {
            self.cancel_edit(cx);
            return;
        }

        self.editing = None;
        // Echo the edit locally so it lands at once; the server's `MessageEdited`
        // reconciles the authoritative `edited_at` when it arrives.
        self.chat.update(cx, |c, cx| {
            c.edit_message(message_id, content.clone(), Utc::now());
            cx.notify();
        });
        self.send_ws(ClientMsg::EditMessage { message_id, content });
        self.sync_messages(cx);
        cx.notify();
    }

    /// Close the inline editor without saving, restoring the row's text.
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.sync_messages(cx);
            cx.notify();
        }
    }

    /// Delete `message_id`. The server re-checks the author/admin rule and
    /// broadcasts `MessageDeleted`, which removes the row, so this only sends.
    fn delete_message_action(&mut self, message_id: Uuid, cx: &mut Context<Self>) {
        // Close the inline editor first if it was open on this message, so its
        // row doesn't linger as an editor between the send and the server echo.
        if self.editing == Some(message_id) {
            self.cancel_edit(cx);
        }
        self.send_ws(ClientMsg::DeleteMessage { message_id });
    }

    /// Whether the signed-in user may moderate the active server — its owner or
    /// an admin member. Mirrors the server's delete rule so the trash affordance
    /// only shows where a delete would actually be allowed.
    fn is_active_server_admin(&self, cx: &Context<Self>) -> bool {
        let Some(me) = self.auth_state.read(cx).user().map(|u| u.id) else {
            return false;
        };
        let servers = self.servers.read(cx);
        let Some(server) = servers.active_server_info() else {
            return false;
        };
        server.owner_id == me
            || servers
                .members_for(server.id)
                .iter()
                .any(|m| m.user_id == me && m.role.eq_ignore_ascii_case("admin"))
    }

    /// Send the composer's contents to the active channel: show the message
    /// optimistically, hand it to the socket, clear the box, and end the typing
    /// session. Blank or over-long input is ignored.
    fn send_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel_id) = self.chat.read(cx).active_channel() else {
            return;
        };
        let content = self.composer.read(cx).value().trim().to_string();
        // Mirror the server's validation so blank or over-long input never
        // leaves the machine — nor shows optimistically.
        if validate_message_content(&content).is_err() {
            return;
        }

        // Optimistic echo: show it now under a client-generated id, which the
        // server's `NewMessage` reconciles away once it arrives.
        let optimistic = MessageWithAuthor {
            id: Uuid::new_v4(),
            channel_id,
            author: self.local_author(cx),
            content: content.clone(),
            edited_at: None,
            created_at: Utc::now(),
        };
        self.chat.update(cx, |c, cx| {
            c.push_optimistic(optimistic);
            cx.notify();
        });
        // A DM channel rides a different wire message than a server channel,
        // though `ChatState` keys both by the same channel id.
        if self.active_is_dm(cx) {
            self.send_ws(ClientMsg::SendDirectMessage {
                dm_channel_id: channel_id,
                content,
            });
        } else {
            self.send_ws(ClientMsg::SendMessage { channel_id, content });
        }

        self.composer.update(cx, |input, cx| input.set_value("", window, cx));
        self.stop_typing();
    }

    /// React to a keystroke in the composer: announce / refresh `StartTyping`
    /// while there is text, and (re)arm the idle timer that later sends
    /// `StopTyping`. An empty box stops typing right away.
    fn on_composer_change(&mut self, cx: &mut Context<Self>) {
        let Some(channel_id) = self.chat.read(cx).active_channel() else {
            return;
        };
        // DMs carry no typing indicators yet — the server's typing fan-out is a
        // server-channel concern — so don't announce typing on a DM channel.
        if self.active_is_dm(cx) {
            return;
        }
        if self.composer.read(cx).value().trim().is_empty() {
            self.stop_typing();
            return;
        }

        // Announce on the first keystroke and refresh periodically, but not on
        // every character; a change of channel also forces a fresh announce.
        let now = Instant::now();
        let refresh_due = self.typing_channel != Some(channel_id)
            || self
                .last_typing_sent
                .is_none_or(|t| now.duration_since(t) >= TYPING_REFRESH);
        if refresh_due {
            if self.typing_channel.is_some_and(|c| c != channel_id) {
                self.stop_typing();
            }
            self.send_ws(ClientMsg::StartTyping { channel_id });
            self.typing_channel = Some(channel_id);
            self.last_typing_sent = Some(now);
        }

        // (Re)arm the idle timer; only the latest keystroke's arm survives, since
        // each bumps the sequence the timer checks before firing.
        self.typing_seq = self.typing_seq.wrapping_add(1);
        let seq = self.typing_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPING_IDLE).await;
            let _ = this.update(cx, |this, _cx| {
                if this.typing_seq == seq {
                    this.stop_typing();
                }
            });
        })
        .detach();
    }

    /// End the current typing session, telling the server to clear our
    /// indicator. Bumps the sequence so any armed idle timer becomes a no-op.
    fn stop_typing(&mut self) {
        self.last_typing_sent = None;
        self.typing_seq = self.typing_seq.wrapping_add(1);
        if let Some(channel_id) = self.typing_channel.take() {
            self.send_ws(ClientMsg::StopTyping { channel_id });
        }
    }

    /// Fire-and-forget an outgoing `ClientMsg` over the socket. The send runs on
    /// the shared tokio runtime; with no live handle the message is dropped.
    fn send_ws(&self, msg: ClientMsg) {
        let Some(handle) = self.ws_handle.clone() else {
            return;
        };
        api::runtime().spawn(async move {
            if let Err(err) = handle.send(msg).await {
                tracing::warn!(error = %err, "failed to send ws message");
            }
        });
    }

    /// The signed-in user as a [`MessageAuthor`], for stamping optimistic
    /// messages before the server's echo resolves the real author.
    fn local_author(&self, cx: &Context<Self>) -> Option<MessageAuthor> {
        self.auth_state.read(cx).user().map(|u| MessageAuthor {
            id: u.id,
            username: u.username.clone(),
            avatar_url: u.avatar_url.clone(),
        })
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
            if view == View::DirectMessages {
                // Load the conversation list on first visit, and re-open the
                // selected DM so the chat pane shows it rather than whatever
                // server channel was opened while we were away.
                this.ensure_dms_loaded(cx);
                if let Some(active) = this.dms.read(cx).active() {
                    this.open_channel(active, cx);
                }
            }
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
            View::DirectMessages => self.dm_list(cx).into_any_element(),
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
                    .justify_between()
                    .border_b_1()
                    .border_color(color::border())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_color(color::text())
                            .text_size(px(font::LG))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(header),
                    )
                    // A "new group DM" affordance rides the DM view's header.
                    .children((view == View::DirectMessages).then(|| self.new_group_dm_button(cx))),
            )
            .child(body)
    }

    /// The "New Group DM" button in the DM sidebar header: a "+" that opens the
    /// group-creation dialog.
    fn new_group_dm_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("new-group-dm")
            .flex()
            .items_center()
            .justify_center()
            .size(px(28.0))
            .flex_shrink_0()
            .rounded(px(space::XS))
            .text_color(color::text_muted())
            .cursor_pointer()
            .hover(|s| s.bg(color::hover()).text_color(color::text()))
            .child(Icon::new(IconName::Plus).with_size(px(18.0)))
            .tooltip(|window, cx| Tooltip::new("New Group DM").build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.open_group_dm_dialog(window, cx)))
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

    /// The DM sidebar: the user's conversations, newest activity first, each a
    /// clickable row. Loading and empty states replace the whole list.
    fn dm_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.dms.read(cx).active();
        let me = self.auth_state.read(cx).user().map(|u| u.id);
        let now = Utc::now();
        // Build owned rows first so the `dms` borrow drops before the per-row
        // `cx.listener`s reborrow `cx`.
        let (loading, loaded, rows) = {
            let dms = self.dms.read(cx);
            let rows: Vec<DmRow> = dms
                .conversations()
                .iter()
                .map(|c| DmRow::from_conversation(c, me, now))
                .collect();
            (dms.is_loading(), dms.is_loaded(), rows)
        };

        let mut list = v_flex()
            .id("dm-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p(px(space::SM))
            .gap(px(space::XS));

        if rows.is_empty() {
            let notice = if loading || !loaded {
                "Loading…"
            } else {
                "No conversations yet."
            };
            return list.child(Self::muted_row(notice));
        }
        for row in rows {
            let selected = active == Some(row.id);
            list = list.child(self.dm_row(row, selected, cx));
        }
        list
    }

    /// One DM conversation row: an avatar, the display name with a relative
    /// timestamp, and a one-line message preview, plus an unread dot. An unread
    /// row reads brighter and bolder; clicking it opens the conversation.
    fn dm_row(&self, row: DmRow, selected: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = row.id;
        let name_color = if row.unread || selected {
            color::text()
        } else {
            color::text_muted()
        };
        let name_weight = if row.unread {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        };

        let mut container = h_flex()
            .id(SharedString::from(id.to_string()))
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .gap(px(space::SM))
            .items_center()
            .rounded(px(space::XS));
        if selected {
            container = container.bg(color::active());
        }
        container
            .hover(|s| s.bg(color::hover()))
            .cursor_pointer()
            .child(dm_avatar(&row.avatar))
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(2.0))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .gap(px(space::SM))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(font::MD))
                                    .text_color(name_color)
                                    .font_weight(name_weight)
                                    .child(row.name),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(font::SM))
                                    .text_color(color::text_faint())
                                    .child(row.timestamp),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(px(font::SM))
                            .text_color(color::text_muted())
                            .child(row.preview),
                    ),
            )
            .children(row.unread.then(|| {
                div()
                    .flex_shrink_0()
                    .size(px(8.0))
                    .rounded_full()
                    .bg(color::accent())
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.open_dm_conversation(id, cx)))
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
                if self.dms.read(cx).active().is_some() {
                    self.chat_pane(cx).into_any_element()
                } else {
                    Self::placeholder_pane(
                        "Direct Messages",
                        "Select a conversation to start chatting.",
                    )
                    .into_any_element()
                }
            }
            View::Settings => {
                Self::placeholder_pane("Settings", "Settings live here once the views land.")
                    .into_any_element()
            }
        }
    }

    /// The chat header's title for the open conversation: the DM's display name
    /// when a DM is open, otherwise the active server channel named
    /// Discord-style ("# general"), or a prompt when nothing is open.
    fn chat_title(&self, cx: &Context<Self>) -> SharedString {
        if let Some(name) = self.active_dm_name(cx) {
            return name;
        }
        self.chat
            .read(cx)
            .active_channel()
            .and_then(|id| {
                self.servers
                    .read(cx)
                    .active_channels()
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| SharedString::from(format!("# {}", c.name)))
            })
            .unwrap_or_else(|| "Select a channel".into())
    }

    /// The display name of the open DM conversation — the other person for a
    /// 1:1, the group name (or participant names) for a group — or `None` when
    /// no DM is the open chat.
    fn active_dm_name(&self, cx: &Context<Self>) -> Option<SharedString> {
        if !self.active_is_dm(cx) {
            return None;
        }
        let dms = self.dms.read(cx);
        let conv = dms.conversation(dms.active()?)?;
        let me = self.auth_state.read(cx).user().map(|u| u.id);
        Some(dm_display_name(conv, me))
    }

    /// The chat pane: header, the virtualized message list, and the composer
    /// (with its typing indicator) pinned at the foot.
    fn chat_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_channel = self.chat.read(cx).active_channel();
        let is_dm = self.active_is_dm(cx);
        let title = self.chat_title(cx);

        let (loading, empty) = {
            let chat = self.chat.read(cx);
            (chat.is_loading(), chat.is_empty())
        };
        let status = self.connection.read(cx).status();

        let body: AnyElement = if active_channel.is_none() {
            Self::message_notice("Pick a channel from the sidebar.").into_any_element()
        } else if empty && loading {
            Self::message_notice("Loading messages…").into_any_element()
        } else if empty {
            Self::message_notice("No messages yet — say hello!").into_any_element()
        } else {
            self.message_list_area(cx).into_any_element()
        };

        // The composer only makes sense once a channel is open.
        let composer = active_channel.map(|_| self.chat_composer(cx).into_any_element());

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
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(space::MD))
                            .child(Self::connection_indicator(status))
                            // The member toggle is a server-channel concern; a DM
                            // has no member list to reveal.
                            .children((!is_dm).then(|| self.member_toggle(cx))),
                    ),
            )
            .child(body)
            .children(composer)
    }

    /// The composer at the foot of the chat pane: the "<user> is typing…" line
    /// (when others are typing) stacked above a Discord-style input bar — a
    /// rounded, raised surface holding a leading add button and the borderless
    /// input.
    fn chat_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .flex_shrink_0()
            .px(px(space::LG))
            .pb(px(space::LG))
            .gap(px(space::XS))
            .children(self.typing_line(cx))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(space::MD))
                    .px(px(space::LG))
                    .py(px(space::MD))
                    .rounded(px(space::SM))
                    .bg(color::elevated())
                    .child(Self::composer_add_button(cx))
                    .child(Input::new(&self.composer).appearance(false).flex_1()),
            )
    }

    /// The leading "+" in the composer — Discord's attachment affordance. The
    /// upload flow lands in later work, so this is a styled stub for now.
    fn composer_add_button(cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("composer-add")
            .size(px(24.0))
            .flex_shrink_0()
            .rounded_full()
            .bg(color::text_muted())
            .text_color(color::elevated())
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(color::text()))
            .child(Icon::new(IconName::Plus).with_size(px(16.0)))
            .on_click(cx.listener(|_, _, _, _| {
                tracing::debug!("composer add clicked; attachments land in later work")
            }))
    }

    /// Keep the composer's placeholder in step with the active channel, naming it
    /// Discord-style ("Message #general"). Rewritten only on an actual switch, so
    /// routine chat updates (new messages, typing) don't churn the input.
    fn refresh_composer_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.chat.read(cx).active_channel();
        if active == self.composer_channel {
            return;
        }
        self.composer_channel = active;
        let placeholder: SharedString = if self.active_is_dm(cx) {
            self.active_dm_name(cx)
                .map(|name| format!("Message {name}"))
                .unwrap_or_else(|| "Message".to_string())
                .into()
        } else {
            match active {
                Some(id) => {
                    let servers = self.servers.read(cx);
                    servers
                        .active_channels()
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| format!("Message #{}", c.name))
                        .unwrap_or_else(|| "Message".to_string())
                        .into()
                }
                None => "Message".into(),
            }
        };
        self.composer
            .update(cx, |input, cx| input.set_placeholder(placeholder, window, cx));
    }

    /// The "<user> is typing…" line shown just above the composer, naming a
    /// couple of typists and summarising any others. `None` when nobody is.
    fn typing_line(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let mut typists: Vec<Uuid> = self.chat.read(cx).typing_users().collect();
        if typists.is_empty() {
            return None;
        }
        // The set has no inherent order; sort so names don't shuffle each frame.
        typists.sort();
        Some(
            div()
                .text_color(color::text_muted())
                .text_size(px(font::SM))
                .child(self.typing_label(&typists, cx)),
        )
    }

    /// Phrase a set of typing users into a sentence, naming up to two and
    /// summarising the rest.
    fn typing_label(&self, typists: &[Uuid], cx: &Context<Self>) -> SharedString {
        match typists {
            [only] => format!("{} is typing…", self.username_for(*only, cx)).into(),
            [a, b] => format!(
                "{} and {} are typing…",
                self.username_for(*a, cx),
                self.username_for(*b, cx)
            )
            .into(),
            _ => "Several people are typing…".into(),
        }
    }

    /// Resolve a user's display name from the active server's members, falling
    /// back to a neutral label when they aren't loaded.
    fn username_for(&self, user_id: Uuid, cx: &Context<Self>) -> String {
        let servers = self.servers.read(cx);
        servers
            .active_server()
            .and_then(|server| {
                servers
                    .members_for(server)
                    .iter()
                    .find(|m| m.user_id == user_id)
                    .map(|m| m.username.clone())
            })
            .unwrap_or_else(|| "Someone".to_string())
    }

    /// The scrollable list of messages, overlaid with the "new messages" jump
    /// button while the user is scrolled up past freshly arrived messages.
    fn message_list_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.message_rows.clone();
        // Everything a row needs to render its hover actions and inline editor.
        // Permissions are resolved here, per render, rather than baked into the
        // rows, so they track membership/auth loading without rebuilding rows.
        let ctx = RowRender {
            view: cx.weak_entity(),
            me: self.auth_state.read(cx).user().map(|u| u.id),
            is_admin: self.is_active_server_admin(cx),
            editor: self.editor.clone(),
        };
        let list = list(self.message_list.clone(), move |ix, _window, _cx| {
            rows.get(ix)
                .map(|row| render_message_row(row, &ctx))
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

    // -- Member panel -----------------------------------------------------

    /// The chat header's member-list toggle: a two-person icon that shows or
    /// hides the right-hand panel. Discord-clean — no filled hover box, the icon
    /// just brightens from muted to full on hover. Idle muted in both states so
    /// the brighten reads the same whether the panel is open or closed; the open
    /// state is conveyed by the panel's presence and the tooltip, not the icon.
    fn member_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.show_members;
        let tooltip: SharedString =
            if shown { "Hide Member List" } else { "Show Member List" }.into();
        div()
            .id("member-toggle")
            .size(px(32.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(color::text_muted())
            .cursor_pointer()
            .hover(|s| s.text_color(color::text()))
            .child(Icon::empty().path("icons/users.svg").with_size(px(20.0)))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_members(cx)))
    }

    /// The right-hand panel: the active server's members grouped by role (owner,
    /// admin, member), online first and offline dimmed within each group. Each
    /// row carries an avatar, a presence dot, and the username; clicking it opens
    /// a direct message with that member.
    fn member_panel(&self, server_id: Uuid, cx: &mut Context<Self>) -> impl IntoElement {
        // Group + order off owned data so the `servers`/`presence` borrows drop
        // before the per-row `cx.listener`s reborrow `cx`.
        let (loading, groups) = {
            let servers = self.servers.read(cx);
            let presence = self.presence.read(cx);
            (
                servers.is_loading(),
                group_members(servers.members_for(server_id), presence),
            )
        };

        let mut list = v_flex()
            .id("member-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .p(px(space::SM))
            .gap(px(space::XS));

        if groups.is_empty() {
            let notice = if loading { "Loading…" } else { "No members." };
            list = list.child(Self::muted_row(notice));
        } else {
            for (role, members) in groups {
                list = list.child(Self::member_group_header(role, members.len()));
                for member in members {
                    list = list.child(self.member_row(member, cx));
                }
            }
        }

        v_flex()
            .w(px(space::MEMBER_PANEL))
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
                    .child("Members"),
            )
            .child(list)
    }

    /// A role section header: the uppercased role and its member count, muted.
    fn member_group_header(role: String, count: usize) -> impl IntoElement {
        div()
            .w_full()
            .mt(px(space::SM))
            .px(px(space::XS))
            .py(px(space::XS))
            .text_color(color::text_muted())
            .text_size(px(font::SM))
            .font_weight(FontWeight::SEMIBOLD)
            .child(SharedString::from(format!("{role} — {count}")))
    }

    /// One member row: an avatar with a presence dot, then the username. Offline
    /// members are dimmed; clicking the row opens a DM with that member.
    fn member_row(&self, member: PanelMember, cx: &mut Context<Self>) -> impl IntoElement {
        let user_id = member.user_id;
        let offline = member.status == UserStatus::Offline;
        let tint = author_tint(Some(user_id));
        let initial = author_initial(&member.username);
        let username = SharedString::from(member.username);
        let tip = SharedString::from(format!("Message @{username}"));

        let mut row = h_flex()
            .id(SharedString::from(user_id.to_string()))
            .w_full()
            .px(px(space::SM))
            .py(px(space::XS))
            .gap(px(space::SM))
            .items_center()
            .rounded(px(space::XS))
            .text_color(color::text());
        // Offline members read as present-but-away: the whole row dims, dot and
        // all, Discord-style.
        if offline {
            row = row.opacity(0.45);
        }
        row.hover(|s| s.bg(color::hover()))
            .cursor_pointer()
            .child(member_avatar(initial, tint, member.status))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(font::MD))
                    .child(username),
            )
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| this.open_dm(user_id, cx)))
    }

    /// The main layout: server rail · sidebar · content, plus the member list
    /// panel on the right when it is toggled on and a server is in view.
    fn main_layout(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Members belong to a server, so the panel only rides alongside the
        // servers view, and only once a server is selected.
        let active_server = self.servers.read(cx).active_server();
        let members_panel = match (self.show_members, self.nav.is_active(View::Servers), active_server)
        {
            (true, true, Some(server_id)) => Some(self.member_panel(server_id, cx)),
            _ => None,
        };

        h_flex()
            .size_full()
            .bg(color::chat())
            .text_color(color::text())
            .font_family(font::FAMILY)
            .child(self.server_rail(cx))
            .child(self.channel_sidebar(cx))
            .child(self.content(cx))
            .children(members_panel)
    }
}

impl Render for ConcordApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen {
            Screen::Auth => self.auth.clone().into_any_element(),
            Screen::Main => {
                // The group-DM dialog, when open, overlays the layout as an
                // absolutely-positioned, full-window modal that dims the app.
                div()
                    .relative()
                    .size_full()
                    .child(self.main_layout(cx))
                    .children(self.group_dm_dialog.clone())
                    .into_any_element()
            }
        }
    }
}

/// Project a freshly created DM channel into a conversation-list row: it has no
/// messages yet and is read by its creator. `member_count` counts every
/// participant, the caller included, matching the DM-list endpoint's shape.
fn conversation_from_info(info: DmChannelInfo) -> DmConversation {
    DmConversation {
        id: info.id,
        name: info.name,
        is_group: info.is_group,
        owner_id: info.owner_id,
        created_at: info.created_at,
        member_count: info.participants.len() as i64,
        participants: info.participants,
        last_message: None,
        unread: false,
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
/// rendered height (header, content, "edited" marker, the inline editor).
#[derive(Clone, PartialEq)]
enum MessageRow {
    DateSeparator {
        label: SharedString,
    },
    Message {
        id: Uuid,
        /// The author's user id, or `None` for a deleted account. Drives the
        /// edit/delete permission check at render time.
        author_id: Option<Uuid>,
        author: SharedString,
        timestamp: SharedString,
        content: SharedString,
        show_header: bool,
        /// A "last edited …" tooltip label when the message has been edited,
        /// otherwise `None` (no "(edited)" marker shown).
        edited: Option<SharedString>,
        /// True while this message is open in the inline editor.
        editing: bool,
        /// Palette index for the author's avatar and name colour.
        tint: u8,
    },
}

/// Everything a message row needs to render its interactive parts, resolved
/// once per list render and shared (by reference) into every row. Click
/// handlers reach the root view through [`Self::view`]; permissions are read
/// from [`Self::me`] and [`Self::is_admin`]; the editing row borrows the shared
/// [`Self::editor`].
struct RowRender {
    view: WeakEntity<ConcordApp>,
    /// The signed-in user's id, for the own-message edit/delete check.
    me: Option<Uuid>,
    /// Whether the viewer can moderate the active server (owner or admin).
    is_admin: bool,
    /// The shared inline editor, rendered only by the row being edited.
    editor: Entity<InputState>,
}

/// Flatten loaded messages (oldest first) into renderable rows: a date
/// separator before the first message of each calendar day, then one row per
/// message. Consecutive messages from the same author within
/// [`GROUP_GAP_MINUTES`] are "grouped" — only the first carries a header.
fn build_message_rows(
    messages: &[MessageWithAuthor],
    today: NaiveDate,
    editing: Option<Uuid>,
) -> Vec<MessageRow> {
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
            author_id,
            author: author.into(),
            timestamp: local.format("%H:%M").to_string().into(),
            content: m.content.clone().into(),
            show_header,
            edited: m.edited_at.map(edit_tooltip),
            editing: editing == Some(m.id),
            tint: author_tint(author_id),
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

/// The tooltip shown on an edited message's "(edited)" marker: the absolute
/// local time the edit landed.
fn edit_tooltip(edited_at: DateTime<Utc>) -> SharedString {
    edited_at
        .with_timezone(&Local)
        .format("Last edited %b %-d, %Y at %H:%M")
        .to_string()
        .into()
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

/// A small palette of bright accents for avatars and author names, picked by
/// author id so the same person always reads the same colour (we have no role
/// colours to key off, as Discord does).
const AVATAR_PALETTE: [u32; 8] = [
    0x5865f2, 0x3ba55d, 0xe67e22, 0xeb459e, 0xed4245, 0x1abc9c, 0xfaa61a, 0x9b59b6,
];

/// The accent for a given palette index.
fn avatar_color(tint: u8) -> Hsla {
    rgb(AVATAR_PALETTE[tint as usize % AVATAR_PALETTE.len()]).into()
}

/// A stable palette index for an author (or a default for an unknown sender).
fn author_tint(author_id: Option<Uuid>) -> u8 {
    author_id
        .map(|id| (id.as_u128() % AVATAR_PALETTE.len() as u128) as u8)
        .unwrap_or(0)
}

/// A member as the panel renders them: identity plus current presence.
#[derive(Clone, PartialEq)]
struct PanelMember {
    user_id: Uuid,
    username: String,
    status: UserStatus,
}

/// Sort rank for a role string: owner first, then admin, then member, then any
/// unrecognised role last.
fn role_rank(role: &str) -> u8 {
    match role.to_ascii_lowercase().as_str() {
        "owner" => 0,
        "admin" => 1,
        "member" => 2,
        _ => 3,
    }
}

/// Group members for the panel: bucketed by role (owner, admin, member, then any
/// other), the buckets in that order, and within each bucket online members
/// first then alphabetical (case-insensitive). Each entry is the uppercased role
/// label paired with its ordered members.
fn group_members(
    members: &[MemberInfo],
    presence: &PresenceState,
) -> Vec<(String, Vec<PanelMember>)> {
    let mut roles: Vec<&str> = members.iter().map(|m| m.role.as_str()).collect();
    roles.sort_by(|a, b| role_rank(a).cmp(&role_rank(b)).then_with(|| a.cmp(b)));
    roles.dedup();

    roles
        .into_iter()
        .map(|role| {
            let mut group: Vec<PanelMember> = members
                .iter()
                .filter(|m| m.role == role)
                .map(|m| PanelMember {
                    user_id: m.user_id,
                    username: m.username.clone(),
                    status: presence.status_for(m.user_id),
                })
                .collect();
            group.sort_by(|a, b| {
                let a_off = a.status == UserStatus::Offline;
                let b_off = b.status == UserStatus::Offline;
                a_off
                    .cmp(&b_off)
                    .then_with(|| a.username.to_lowercase().cmp(&b.username.to_lowercase()))
            });
            (role.to_uppercase(), group)
        })
        .collect()
}

/// The presence dot colour for a status: green online, amber idle, red
/// do-not-disturb, grey offline.
fn status_color(status: UserStatus) -> Hsla {
    match status {
        UserStatus::Online => color::online(),
        UserStatus::Idle => color::idle(),
        UserStatus::Dnd => color::danger(),
        UserStatus::Offline => color::text_faint(),
    }
}

/// A member avatar — a tinted circle with their initial — overlaid at its
/// bottom-right with a presence dot, ringed in the panel background so it reads
/// as separate from the avatar.
fn member_avatar(initial: SharedString, tint: u8, status: UserStatus) -> impl IntoElement {
    div()
        .relative()
        .size(px(MEMBER_AVATAR_SIZE))
        .flex_shrink_0()
        .child(
            div()
                .size(px(MEMBER_AVATAR_SIZE))
                .rounded_full()
                .bg(avatar_color(tint))
                .flex()
                .items_center()
                .justify_center()
                .text_color(color::interactive_active())
                .text_size(px(font::SM))
                .font_weight(FontWeight::SEMIBOLD)
                .child(initial),
        )
        .child(
            div()
                .absolute()
                .bottom(px(-1.0))
                .right(px(-1.0))
                .size(px(MEMBER_STATUS_DOT + 2.0 * MEMBER_STATUS_RING))
                .rounded_full()
                .bg(color::sidebar())
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size(px(MEMBER_STATUS_DOT))
                        .rounded_full()
                        .bg(status_color(status)),
                ),
        )
}

/// A DM conversation as the sidebar list renders it: identity resolved against
/// the signed-in user (a 1:1 names the *other* person), a one-line preview of
/// the last message, and a compact relative timestamp.
struct DmRow {
    id: Uuid,
    name: SharedString,
    avatar: DmAvatar,
    preview: SharedString,
    timestamp: SharedString,
    unread: bool,
}

/// The avatar a DM row shows: a single participant's initial for a 1:1, or a
/// generic group glyph for a group DM.
enum DmAvatar {
    Person { initial: SharedString, tint: u8 },
    Group,
}

impl DmRow {
    /// Project a conversation into its rendered row, resolving the display name
    /// and avatar against `me` (the signed-in user) and the last-message age
    /// against `now`.
    fn from_conversation(conv: &DmConversation, me: Option<Uuid>, now: DateTime<Utc>) -> Self {
        let avatar = if conv.is_group {
            DmAvatar::Group
        } else {
            // A 1:1 shows the other person; fall back to any participant if the
            // list somehow holds only ourselves.
            let person = conv
                .participants
                .iter()
                .find(|p| Some(p.user_id) != me)
                .or_else(|| conv.participants.first());
            match person {
                Some(p) => DmAvatar::Person {
                    initial: author_initial(&p.username),
                    tint: author_tint(Some(p.user_id)),
                },
                None => DmAvatar::Person {
                    initial: "?".into(),
                    tint: 0,
                },
            }
        };

        DmRow {
            id: conv.id,
            name: dm_display_name(conv, me),
            avatar,
            preview: dm_preview(conv),
            timestamp: conv
                .last_message
                .as_ref()
                .map(|m| relative_time(m.created_at, now).into())
                .unwrap_or_default(),
            unread: conv.unread,
        }
    }
}

/// The display name of a DM: an explicit name when set, else the other
/// participants' usernames joined (a group), else the single other person (a
/// 1:1). Falls back to a neutral label when no other participant is known.
fn dm_display_name(conv: &DmConversation, me: Option<Uuid>) -> SharedString {
    if let Some(name) = conv.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string().into();
    }
    let others: Vec<&str> = conv
        .participants
        .iter()
        .filter(|p| Some(p.user_id) != me)
        .map(|p| p.username.as_str())
        .collect();
    if others.is_empty() {
        // Either a self-only record, or a participant list that didn't resolve.
        return if conv.is_group { "Group DM" } else { "Direct Message" }.into();
    }
    if conv.is_group {
        others.join(", ").into()
    } else {
        others[0].to_string().into()
    }
}

/// A one-line preview of a conversation's last message: the message text with
/// newlines flattened, prefixed with the sender for a group DM so it's clear who
/// spoke. An empty conversation reads as "No messages yet".
fn dm_preview(conv: &DmConversation) -> SharedString {
    let Some(last) = conv.last_message.as_ref() else {
        return "No messages yet".into();
    };
    let text = last.content.replace('\n', " ");
    match (conv.is_group, last.author.as_ref()) {
        (true, Some(author)) => format!("{}: {}", author.username, text).into(),
        _ => text.into(),
    }
}

/// A compact, relative timestamp for a DM's last activity: "now" under a minute,
/// then "5m" / "3h" / "2d" up to a week, then an absolute "Mon D" date.
fn relative_time(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        "now".to_string()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 7 * 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        then.with_timezone(&Local).format("%b %-d").to_string()
    }
}

/// A DM row avatar: a tinted initial circle for a 1:1, or a muted group glyph.
fn dm_avatar(avatar: &DmAvatar) -> impl IntoElement {
    let base = div()
        .size(px(DM_AVATAR_SIZE))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center();
    match avatar {
        DmAvatar::Person { initial, tint } => base
            .bg(avatar_color(*tint))
            .text_color(color::interactive_active())
            .text_size(px(font::SM))
            .font_weight(FontWeight::SEMIBOLD)
            .child(initial.clone()),
        DmAvatar::Group => base
            .bg(color::elevated())
            .text_color(color::text_muted())
            .child(Icon::empty().path("icons/users.svg").with_size(px(18.0))),
    }
}

/// Whether `me` may edit a message authored by `author_id`: only its own author,
/// and only when both identities are known (no editing a deleted account's
/// messages, nor while signed out).
fn can_edit_message(author_id: Option<Uuid>, me: Option<Uuid>) -> bool {
    matches!((author_id, me), (Some(a), Some(m)) if a == m)
}

/// Whether `me` may delete a message: its author, or a server admin/owner.
/// Mirrors the server's rule so the affordance only shows where it would work.
fn can_delete_message(author_id: Option<Uuid>, me: Option<Uuid>, is_admin: bool) -> bool {
    is_admin || can_edit_message(author_id, me)
}

/// The uppercase initial shown on an avatar with no image.
fn author_initial(author: &str) -> SharedString {
    author
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_string())
        .into()
}

/// Render a single list row.
fn render_message_row(row: &MessageRow, ctx: &RowRender) -> AnyElement {
    match row {
        MessageRow::DateSeparator { label } => render_date_separator(label.clone()),
        MessageRow::Message {
            id,
            author_id,
            author,
            timestamp,
            content,
            show_header,
            edited,
            editing,
            tint,
        } => render_message(
            MessageProps {
                id: *id,
                author: author.clone(),
                initial: author_initial(author),
                timestamp: timestamp.clone(),
                content: content.clone(),
                show_header: *show_header,
                edited: edited.clone(),
                editing: *editing,
                tint: *tint,
                can_edit: can_edit_message(*author_id, ctx.me),
                can_delete: can_delete_message(*author_id, ctx.me, ctx.is_admin),
            },
            ctx,
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

/// Fields needed to render one message row, bundled to keep the call site
/// readable. Permissions (`can_edit` / `can_delete`) are resolved by the caller
/// from the row's author and the viewer.
struct MessageProps {
    id: Uuid,
    author: SharedString,
    initial: SharedString,
    timestamp: SharedString,
    content: SharedString,
    show_header: bool,
    edited: Option<SharedString>,
    editing: bool,
    tint: u8,
    can_edit: bool,
    can_delete: bool,
}

/// A message row, Discord-style: a left avatar gutter, then the content column,
/// with a floating edit/delete toolbar revealed on hover. Group openers carry an
/// avatar and an author/time header; grouped messages leave the gutter blank so
/// their text stays aligned under the opener. While `editing`, the content line
/// swaps for the shared inline editor. The whole row lifts on hover.
fn render_message(props: MessageProps, ctx: &RowRender) -> AnyElement {
    let MessageProps {
        id,
        author,
        initial,
        timestamp,
        content,
        show_header,
        edited,
        editing,
        tint,
        can_edit,
        can_delete,
    } = props;

    let gutter = if show_header {
        div()
            .size(px(AVATAR_SIZE))
            .flex_shrink_0()
            .rounded_full()
            .bg(avatar_color(tint))
            .flex()
            .items_center()
            .justify_center()
            .text_color(color::interactive_active())
            .text_size(px(font::MD))
            .font_weight(FontWeight::SEMIBOLD)
            .child(initial)
            .into_any_element()
    } else {
        // Grouped messages leave the avatar slot blank, but reveal their
        // send time there while the row is hovered (Discord-style).
        div()
            .w(px(AVATAR_SIZE))
            .flex_shrink_0()
            .flex()
            .justify_center()
            .pt(px(3.0))
            .child(
                div()
                    .opacity(0.0)
                    .group_hover("message", |s| s.opacity(1.0))
                    .text_size(px(11.0))
                    .text_color(color::text_faint())
                    .child(timestamp.clone()),
            )
            .into_any_element()
    };

    let mut content_col = v_flex().flex_1().min_w(px(0.0)).gap(px(2.0));
    if show_header {
        content_col = content_col.child(
            h_flex()
                .items_baseline()
                .gap(px(space::SM))
                .child(
                    div()
                        .text_color(avatar_color(tint))
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

    if editing {
        // The row hands its text to the shared inline editor.
        content_col = content_col.child(
            v_flex()
                .w_full()
                .gap(px(space::XS))
                .child(Input::new(&ctx.editor).w_full())
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(color::text_faint())
                        .child("enter to save • click away to cancel"),
                ),
        );
    } else {
        let mut content_line = h_flex().w_full().items_baseline().gap(px(space::SM)).child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(color::text())
                .text_size(px(font::MD))
                .child(content),
        );
        if let Some(tip) = edited {
            // The "(edited)" marker carries the exact edit time in its tooltip.
            content_line = content_line.child(
                div()
                    .id(SharedString::from(format!("edited-{id}")))
                    .flex_shrink_0()
                    .text_size(px(font::SM))
                    .text_color(color::text_faint())
                    .child("(edited)")
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            );
        }
        content_col = content_col.child(content_line);
    }

    // No hover toolbar while the row is itself the editor.
    let actions = if editing {
        None
    } else {
        message_actions(id, can_edit, can_delete, ctx)
    };

    div()
        .w_full()
        .relative()
        .group("message")
        .px(px(space::MD))
        .pt(px(if show_header { space::MD } else { 1.0 }))
        .pb(px(1.0))
        .hover(|s| s.bg(color::hover()))
        .child(
            h_flex()
                .w_full()
                .gap(px(space::MD))
                .items_start()
                .child(gutter)
                .child(content_col),
        )
        .children(actions)
        .into_any_element()
}

/// The floating hover toolbar at a row's top-right: an edit pencil (own
/// messages) and a delete trash (own messages, or any message for a server
/// admin). Revealed on row hover, Discord-style. `None` when the viewer can do
/// neither.
fn message_actions(
    id: Uuid,
    can_edit: bool,
    can_delete: bool,
    ctx: &RowRender,
) -> Option<AnyElement> {
    if !can_edit && !can_delete {
        return None;
    }

    let mut bar = h_flex()
        .absolute()
        .top(px(-space::SM))
        .right(px(space::MD))
        .gap(px(space::XS))
        .p(px(space::XS))
        .rounded(px(space::XS))
        .bg(color::elevated())
        .border_1()
        .border_color(color::border())
        .opacity(0.0)
        .group_hover("message", |s| s.opacity(1.0));

    if can_edit {
        let view = ctx.view.clone();
        bar = bar.child(
            message_action_button(
                SharedString::from(format!("edit-{id}")),
                Icon::empty()
                    .path("icons/pencil.svg")
                    .with_size(px(16.0))
                    .into_any_element(),
                "Edit".into(),
                false,
            )
            .on_click(move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.start_editing(id, window, cx));
            }),
        );
    }
    if can_delete {
        let view = ctx.view.clone();
        bar = bar.child(
            message_action_button(
                SharedString::from(format!("delete-{id}")),
                Icon::new(IconName::Delete)
                    .with_size(px(16.0))
                    .into_any_element(),
                "Delete".into(),
                true,
            )
            .on_click(move |_, _window, cx| {
                let _ = view.update(cx, |this, cx| this.delete_message_action(id, cx));
            }),
        );
    }
    Some(bar.into_any_element())
}

/// A single round-cornered icon button for the message hover toolbar, returned
/// as a stateful div so the caller can attach the action's `on_click`. `danger`
/// tints the icon red on hover (used by delete).
fn message_action_button(
    id: SharedString,
    icon: AnyElement,
    tooltip: SharedString,
    danger: bool,
) -> Stateful<Div> {
    let hover_fg = if danger { color::danger() } else { color::text() };
    div()
        .id(id)
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(space::XS))
        .text_color(color::text_muted())
        .cursor_pointer()
        .hover(move |s| s.bg(color::hover()).text_color(hover_fg))
        .child(icon)
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

#[cfg(test)]
mod tests {
    // Import only what the tests need rather than `use super::*`: the latter
    // re-globs `gpui::*` into this module, which blows the recursion limit when
    // the `#[test]` harness expands.
    use super::{
        build_message_rows, can_delete_message, can_edit_message, diff_splice, dm_display_name,
        dm_preview, group_members, relative_time, role_rank, MessageRow,
    };

    use chrono::{DateTime, Local, TimeZone, Utc};
    use concord_shared::types::{
        DmConversation, DmLastMessage, DmParticipant, MemberInfo, MessageAuthor, MessageWithAuthor,
        UserStatus,
    };
    use uuid::Uuid;

    use crate::state::PresenceState;

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
            None,
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
            None,
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
            None,
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
            None,
        );
        assert!(is_header(&rows[2], true));
    }

    fn dm_participant(n: u8, username: &str) -> DmParticipant {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        DmParticipant {
            user_id: Uuid::from_bytes(bytes),
            username: username.into(),
            avatar_url: None,
        }
    }

    fn dm_conv(
        is_group: bool,
        name: Option<&str>,
        participants: Vec<DmParticipant>,
        last: Option<DmLastMessage>,
    ) -> DmConversation {
        DmConversation {
            id: Uuid::nil(),
            name: name.map(str::to_string),
            is_group,
            owner_id: None,
            created_at: Utc::now(),
            member_count: participants.len() as i64,
            participants,
            last_message: last,
            unread: false,
        }
    }

    fn me_id() -> Uuid {
        dm_participant(1, "").user_id
    }

    #[test]
    fn dm_name_prefers_explicit_group_name() {
        let conv = dm_conv(
            true,
            Some("Lunch Crew"),
            vec![dm_participant(1, "me"), dm_participant(2, "bob")],
            None,
        );
        assert_eq!(dm_display_name(&conv, Some(me_id())).as_ref(), "Lunch Crew");
    }

    #[test]
    fn dm_name_one_on_one_is_the_other_person() {
        let conv = dm_conv(
            false,
            None,
            vec![dm_participant(1, "me"), dm_participant(2, "bob")],
            None,
        );
        assert_eq!(dm_display_name(&conv, Some(me_id())).as_ref(), "bob");
    }

    #[test]
    fn dm_name_group_joins_other_participants() {
        let conv = dm_conv(
            true,
            None,
            vec![
                dm_participant(1, "me"),
                dm_participant(2, "bob"),
                dm_participant(3, "cara"),
            ],
            None,
        );
        assert_eq!(dm_display_name(&conv, Some(me_id())).as_ref(), "bob, cara");
    }

    #[test]
    fn dm_name_falls_back_when_only_self() {
        let conv = dm_conv(false, None, vec![dm_participant(1, "me")], None);
        assert_eq!(
            dm_display_name(&conv, Some(me_id())).as_ref(),
            "Direct Message"
        );
    }

    #[test]
    fn dm_preview_empty_and_group_prefix() {
        let empty = dm_conv(false, None, vec![dm_participant(2, "bob")], None);
        assert_eq!(dm_preview(&empty).as_ref(), "No messages yet");

        let last = DmLastMessage {
            id: Uuid::nil(),
            author: Some(MessageAuthor {
                id: Uuid::nil(),
                username: "bob".into(),
                avatar_url: None,
            }),
            content: "hey\nthere".into(),
            created_at: Utc::now(),
        };
        let group = dm_conv(true, Some("Crew"), vec![dm_participant(2, "bob")], Some(last));
        // Group previews name the speaker; newlines flatten to one line.
        assert_eq!(dm_preview(&group).as_ref(), "bob: hey there");
    }

    #[test]
    fn relative_time_buckets() {
        let now = Utc.with_ymd_and_hms(2026, 5, 30, 12, 0, 0).unwrap();
        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(now - chrono::Duration::minutes(5), now), "5m");
        assert_eq!(relative_time(now - chrono::Duration::hours(3), now), "3h");
        assert_eq!(relative_time(now - chrono::Duration::days(2), now), "2d");
        // Older than a week falls back to an absolute date.
        let old = relative_time(now - chrono::Duration::days(10), now);
        assert!(old.contains("May"), "expected a month label, got {old}");
    }

    fn row_msg(id: u8, content: &str) -> MessageRow {
        let mut bytes = [0u8; 16];
        bytes[15] = id;
        MessageRow::Message {
            id: Uuid::from_bytes(bytes),
            author_id: None,
            author: "alice".into(),
            timestamp: "12:00".into(),
            content: content.into(),
            show_header: true,
            edited: None,
            editing: false,
            tint: 0,
        }
    }

    /// The `editing` flag and `edited` tooltip a row carries, when it is a
    /// message row (panics otherwise — tests pass a known index).
    fn edit_state(row: &MessageRow) -> (bool, bool) {
        match row {
            MessageRow::Message { editing, edited, .. } => (*editing, edited.is_some()),
            _ => panic!("expected a message row"),
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

    fn id_for(n: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        Uuid::from_bytes(bytes)
    }

    #[test]
    fn marks_only_the_editing_message() {
        let t = at(2026, 5, 30, 12, 0);
        let rows = build_message_rows(
            &[msg(1, Some(1), "a", t), msg(2, Some(1), "b", t)],
            Local::now().date_naive(),
            Some(id_for(2)),
        );
        // rows[0] is the day separator; the openers are the two messages.
        assert_eq!(edit_state(&rows[1]).0, false);
        assert_eq!(edit_state(&rows[2]).0, true);
    }

    #[test]
    fn surfaces_an_edited_marker_with_a_tooltip() {
        let t = at(2026, 5, 30, 12, 0);
        let mut edited = msg(1, Some(1), "a", t);
        edited.edited_at = Some(t);
        let rows = build_message_rows(
            &[edited, msg(2, Some(2), "b", t)],
            Local::now().date_naive(),
            None,
        );
        // The first message is edited; the second (different author) is not.
        assert_eq!(edit_state(&rows[1]).1, true);
        assert_eq!(edit_state(&rows[2]).1, false);
    }

    #[test]
    fn edit_is_allowed_only_for_the_author() {
        let me = id_for(1);
        let other = id_for(2);
        assert!(can_edit_message(Some(me), Some(me)));
        assert!(!can_edit_message(Some(other), Some(me)));
        // A deleted author or a signed-out viewer can never edit.
        assert!(!can_edit_message(None, Some(me)));
        assert!(!can_edit_message(Some(me), None));
    }

    #[test]
    fn delete_is_allowed_for_author_or_admin() {
        let me = id_for(1);
        let other = id_for(2);
        // The author can always delete their own message.
        assert!(can_delete_message(Some(me), Some(me), false));
        // An admin can delete anyone's, including a deleted author's.
        assert!(can_delete_message(Some(other), Some(me), true));
        assert!(can_delete_message(None, Some(me), true));
        // A non-admin cannot delete someone else's.
        assert!(!can_delete_message(Some(other), Some(me), false));
    }

    fn member(n: u8, name: &str, role: &str) -> MemberInfo {
        MemberInfo {
            user_id: id_for(n),
            username: name.into(),
            avatar_url: None,
            role: role.into(),
            joined_at: at(2026, 5, 30, 12, 0),
        }
    }

    #[test]
    fn role_rank_orders_owner_admin_member_then_other() {
        assert!(role_rank("owner") < role_rank("admin"));
        assert!(role_rank("admin") < role_rank("member"));
        assert!(role_rank("member") < role_rank("guest"));
        // Matching is case-insensitive.
        assert_eq!(role_rank("OWNER"), role_rank("owner"));
    }

    #[test]
    fn groups_members_by_role_in_order() {
        let members = vec![
            member(1, "carol", "member"),
            member(2, "dave", "owner"),
            member(3, "alice", "admin"),
        ];
        let groups = group_members(&members, &PresenceState::new());
        let labels: Vec<&str> = groups.iter().map(|(role, _)| role.as_str()).collect();
        assert_eq!(labels, vec!["OWNER", "ADMIN", "MEMBER"]);
    }

    #[test]
    fn online_members_sort_before_offline_then_alphabetically() {
        let members = vec![
            member(1, "zoe", "member"),
            member(2, "amy", "member"),
            member(3, "bob", "member"),
        ];
        let mut presence = PresenceState::new();
        // zoe and bob are present (idle still counts as online); amy is offline.
        presence.set_status(members[0].user_id, UserStatus::Online);
        presence.set_status(members[2].user_id, UserStatus::Idle);
        let groups = group_members(&members, &presence);
        // One MEMBER group: online first (bob, zoe alphabetical), then offline amy.
        let names: Vec<&str> = groups[0].1.iter().map(|m| m.username.as_str()).collect();
        assert_eq!(names, vec!["bob", "zoe", "amy"]);
    }
}
