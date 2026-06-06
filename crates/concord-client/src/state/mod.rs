//! Client-side application state.
//!
//! Each piece is a plain data + logic struct that names no GPUI types, so it
//! unit-tests in the default build exactly like [`crate::ui::nav`]. The desktop
//! client wraps each one in a GPUI `Entity` (see [`crate::ui::root`]) and shares
//! the handles with its views, which re-render by observing the entity. Initial
//! data is loaded over the REST API ([`crate::api`]) on a successful login;
//! live updates arrive as `WsEvent`s and are folded in by the root view.
//!
//! The pieces mirror the concerns the UI tracks:
//! - [`AuthState`] — the current user, tokens, and login status.
//! - [`ServersState`] — the server list, active server, and per-server channels
//!   and members.
//! - [`ChatState`] — messages, pagination, and typing for the active channel.
//! - [`ConnectionState`] — the WebSocket link's status.
//! - [`PresenceState`] — the online status of the users we can see.
//! - [`DmsState`] — the DM conversation list and the open conversation.
//! - [`FriendsState`] — the friends list and pending friend requests.

pub mod auth;
pub mod chat;
pub mod connection;
pub mod dms;
pub mod friends;
pub mod presence;
pub mod servers;

pub use auth::AuthState;
pub use chat::ChatState;
pub use connection::{ConnectionState, ConnectionStatus};
pub use dms::DmsState;
pub use friends::FriendsState;
pub use presence::PresenceState;
pub use servers::ServersState;
