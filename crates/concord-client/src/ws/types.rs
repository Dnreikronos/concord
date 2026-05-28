use concord_shared::protocol::{ClientMsg, ErrorCode, ServerMsg, Token};
use uuid::Uuid;

pub enum WsCommand {
    Connect { url: String, token: Token },
    Send(ClientMsg),
    Shutdown,
}

#[derive(Debug)]
pub enum WsEvent {
    Connected { user_id: Uuid },
    Message(ServerMsg),
    Disconnected { reason: String },
    Reconnecting { attempt: u32 },
    AuthFailed { code: ErrorCode, message: String },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    Disconnected,
    Connecting,
    Reconnecting,
}
