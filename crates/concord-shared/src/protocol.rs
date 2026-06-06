use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{UserStatus, UserSummary};

#[derive(Clone, Serialize, Deserialize)]
pub struct Token(String);

impl Token {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum ClientMsg {
    Authenticate { token: Token },

    // Messaging
    SendMessage { channel_id: Uuid, content: String },
    EditMessage { message_id: Uuid, content: String },
    DeleteMessage { message_id: Uuid },

    // Channels
    JoinChannel { channel_id: Uuid },
    LeaveChannel { channel_id: Uuid },
    StartTyping { channel_id: Uuid },
    StopTyping { channel_id: Uuid },

    // Servers
    CreateServer { name: String },
    JoinServer { server_id: Uuid },
    LeaveServer { server_id: Uuid },

    // DMs
    SendDirectMessage { dm_channel_id: Uuid, content: String },
    CreateDmChannel { user_ids: Vec<Uuid> },

    // Presence
    UpdateStatus { status: UserStatus },
}

/// A single peer's presence, used in `ServerMsg::PresenceSnapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPresence {
    pub user_id: Uuid,
    pub status: UserStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum ServerMsg {
    Authenticated { user_id: Uuid },
    Error { code: ErrorCode, message: String },

    // Messaging
    NewMessage {
        id: Uuid,
        channel_id: Uuid,
        author_id: Option<Uuid>,
        content: String,
        created_at: DateTime<Utc>,
    },
    MessageEdited {
        message_id: Uuid,
        content: String,
        edited_at: DateTime<Utc>,
    },
    MessageDeleted {
        message_id: Uuid,
    },

    // Typing
    //
    // Clients must self-expire a typing indicator a few seconds after the last
    // `TypingStarted` rather than wait for `TypingStopped`: the server emits the
    // stop on the happy path, but it can be lost if the originating instance
    // crashes or a network partition drops the cross-instance event.
    TypingStarted {
        channel_id: Uuid,
        user_id: Uuid,
    },
    TypingStopped {
        channel_id: Uuid,
        user_id: Uuid,
    },

    // Presence
    UserStatusChanged {
        user_id: Uuid,
        status: UserStatus,
    },
    /// Initial presence of the connecting user's relevant peers (members of
    /// shared servers), sent once right after authentication. Only non-offline
    /// peers are listed; anyone absent is implicitly offline.
    PresenceSnapshot {
        users: Vec<UserPresence>,
    },

    // Membership
    MemberJoined {
        server_id: Uuid,
        user_id: Uuid,
    },
    MemberLeft {
        server_id: Uuid,
        user_id: Uuid,
    },

    // DMs
    NewDirectMessage {
        id: Uuid,
        dm_channel_id: Uuid,
        author_id: Option<Uuid>,
        content: String,
        created_at: DateTime<Utc>,
    },
    DmChannelCreated {
        dm_channel_id: Uuid,
        user_ids: Vec<Uuid>,
    },

    // Server lifecycle
    ServerCreated {
        server_id: Uuid,
        name: String,
        owner_id: Uuid,
    },

    // Friends
    //
    // Pushed to the *other* party of a friend action so an online client updates
    // its lists without polling. Offline clients miss the push and reconcile
    // from `GET /api/friends` + `/api/friends/requests` on next load, so these
    // are best-effort, not a source of truth.
    /// A new incoming friend request: `from` is the sender, `request_id` the row
    /// to accept/reject.
    FriendRequestReceived {
        request_id: Uuid,
        from: UserSummary,
    },
    /// A request the recipient sent (or a reverse request) was accepted; `user`
    /// is the new friend.
    FriendRequestAccepted {
        user: UserSummary,
    },
    /// A pending request was rejected or cancelled by the other party; drop
    /// `request_id` from whichever pending list holds it.
    FriendRequestCanceled {
        request_id: Uuid,
    },
    /// The friendship with `user_id` was removed by them.
    FriendRemoved {
        user_id: Uuid,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    BadRequest,
    RateLimited,
    Internal,
}
