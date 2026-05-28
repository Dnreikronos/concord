use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::UserStatus;

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
    },
    MessageEdited {
        message_id: Uuid,
        content: String,
    },
    MessageDeleted {
        message_id: Uuid,
    },

    // Typing
    UserTyping {
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
