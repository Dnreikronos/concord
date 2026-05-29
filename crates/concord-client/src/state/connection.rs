//! WebSocket connection status, mirrored from the background connection task.
//!
//! The [`crate::ws`] task owns the actual socket and reports its progress as
//! `WsEvent`s; the root view translates those into the setters below so views
//! can render a connection indicator by observing this entity. Kept free of any
//! `WsEvent` dependency so it stays a plain, unit-tested value.

/// Where the WebSocket link currently stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// No live socket — before the first connect, or after a clean close.
    #[default]
    Disconnected,
    /// The first connect attempt is in flight.
    Connecting,
    /// Authenticated and ready to send and receive.
    Connected,
    /// The link dropped and the background task is retrying with backoff.
    Reconnecting,
}

impl ConnectionStatus {
    /// A short, human-readable label for a status indicator.
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::Connected => "Connected",
            Self::Reconnecting => "Reconnecting…",
        }
    }
}

/// Tracks the WebSocket connection's status and last failure.
#[derive(Clone, Debug, Default)]
pub struct ConnectionState {
    status: ConnectionStatus,
    /// Attempt number from the most recent `Reconnecting` report.
    attempt: u32,
    /// Reason for the most recent disconnect, if the server or task gave one.
    last_error: Option<String>,
}

impl ConnectionState {
    /// Create state in the [`ConnectionStatus::Disconnected`] state.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current status.
    pub fn status(&self) -> ConnectionStatus {
        self.status
    }

    /// Whether the link is up and authenticated.
    pub fn is_connected(&self) -> bool {
        self.status == ConnectionStatus::Connected
    }

    /// The retry count reported by the last [`ConnectionStatus::Reconnecting`].
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The reason for the most recent disconnect, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Move to [`ConnectionStatus::Connecting`] for the first connect attempt.
    pub fn connecting(&mut self) {
        self.status = ConnectionStatus::Connecting;
        self.last_error = None;
    }

    /// Move to [`ConnectionStatus::Connected`], clearing any retry bookkeeping.
    pub fn connected(&mut self) {
        self.status = ConnectionStatus::Connected;
        self.attempt = 0;
        self.last_error = None;
    }

    /// Move to [`ConnectionStatus::Reconnecting`], recording the attempt number.
    pub fn reconnecting(&mut self, attempt: u32) {
        self.status = ConnectionStatus::Reconnecting;
        self.attempt = attempt;
    }

    /// Move to [`ConnectionStatus::Disconnected`], optionally noting why.
    pub fn disconnected(&mut self, reason: Option<String>) {
        self.status = ConnectionStatus::Disconnected;
        self.attempt = 0;
        if reason.is_some() {
            self.last_error = reason;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_disconnected() {
        let c = ConnectionState::new();
        assert_eq!(c.status(), ConnectionStatus::Disconnected);
        assert!(!c.is_connected());
        assert_eq!(c.attempt(), 0);
        assert!(c.last_error().is_none());
    }

    #[test]
    fn connect_clears_error_and_attempt() {
        let mut c = ConnectionState::new();
        c.reconnecting(3);
        c.disconnected(Some("boom".into()));
        c.connecting();
        assert_eq!(c.status(), ConnectionStatus::Connecting);
        assert!(c.last_error().is_none());

        c.connected();
        assert!(c.is_connected());
        assert_eq!(c.attempt(), 0);
        assert!(c.last_error().is_none());
    }

    #[test]
    fn reconnecting_records_attempt() {
        let mut c = ConnectionState::new();
        c.reconnecting(2);
        assert_eq!(c.status(), ConnectionStatus::Reconnecting);
        assert_eq!(c.attempt(), 2);
    }

    #[test]
    fn disconnect_keeps_prior_reason_when_none_given() {
        let mut c = ConnectionState::new();
        c.disconnected(Some("network down".into()));
        assert_eq!(c.last_error(), Some("network down"));
        // A reason-less disconnect (clean close) leaves the prior reason intact.
        c.disconnected(None);
        assert_eq!(c.last_error(), Some("network down"));
        assert_eq!(c.status(), ConnectionStatus::Disconnected);
    }
}
