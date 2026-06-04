//! Top-level navigation state: which primary view the client is showing, plus
//! the per-view selection (active server + channel, open DM).

use uuid::Uuid;

/// The primary views reachable from the server rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum View {
    /// Servers and their channels (the default landing view).
    #[default]
    Servers,
    /// Direct-message conversations.
    DirectMessages,
    /// User settings.
    Settings,
}

impl View {
    /// Human-readable title shown in the sidebar header.
    pub fn title(self) -> &'static str {
        match self {
            View::Servers => "Concord",
            View::DirectMessages => "Direct Messages",
            View::Settings => "Settings",
        }
    }

    /// Short glyph used on the server-rail button.
    pub fn glyph(self) -> &'static str {
        match self {
            View::Servers => "C",
            View::DirectMessages => "DM",
            View::Settings => "⚙",
        }
    }
}

/// Tracks the active [`View`] and the user's selection within each view: the
/// server whose channels are shown, the channel open in the content pane, and
/// the DM conversation open in the DM view.
///
/// Selection is kept here (rather than on the individual views) so the rail,
/// sidebar, and content pane can all read a single source of truth when
/// deciding what to render and which row to highlight.
#[derive(Clone, Debug, Default)]
pub struct NavState {
    active: View,
    selected_server: Option<Uuid>,
    selected_channel: Option<Uuid>,
    selected_dm: Option<Uuid>,
}

impl NavState {
    /// Create state on the default view with nothing selected yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently active view.
    pub fn active(&self) -> View {
        self.active
    }

    /// Switch to `view`.
    pub fn activate(&mut self, view: View) {
        self.active = view;
    }

    /// Whether `view` is the active one.
    pub fn is_active(&self, view: View) -> bool {
        self.active == view
    }

    /// The server whose channels are currently shown, if any.
    pub fn selected_server(&self) -> Option<Uuid> {
        self.selected_server
    }

    /// Select `server`. Switching to a different server clears the channel
    /// selected in the previous one, since that channel is no longer in view.
    pub fn select_server(&mut self, server: Uuid) {
        if self.selected_server != Some(server) {
            self.selected_channel = None;
        }
        self.selected_server = Some(server);
    }

    /// The channel open in the content pane, if any.
    pub fn selected_channel(&self) -> Option<Uuid> {
        self.selected_channel
    }

    /// Open `channel` in the content pane.
    pub fn select_channel(&mut self, channel: Uuid) {
        self.selected_channel = Some(channel);
    }

    /// The DM conversation open in the DM view, if any.
    pub fn selected_dm(&self) -> Option<Uuid> {
        self.selected_dm
    }

    /// Open DM conversation `dm`.
    pub fn select_dm(&mut self, dm: Uuid) {
        self.selected_dm = Some(dm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_servers() {
        let nav = NavState::new();
        assert_eq!(nav.active(), View::Servers);
        assert!(nav.is_active(View::Servers));
    }

    #[test]
    fn activate_switches_view() {
        let mut nav = NavState::new();
        nav.activate(View::Settings);
        assert_eq!(nav.active(), View::Settings);
        assert!(nav.is_active(View::Settings));
        assert!(!nav.is_active(View::Servers));
    }

    #[test]
    fn selection_starts_empty() {
        let nav = NavState::new();
        assert_eq!(nav.selected_server(), None);
        assert_eq!(nav.selected_channel(), None);
        assert_eq!(nav.selected_dm(), None);
    }

    #[test]
    fn select_channel_within_same_server_is_kept() {
        let mut nav = NavState::new();
        let server = Uuid::new_v4();
        let channel = Uuid::new_v4();
        nav.select_server(server);
        nav.select_channel(channel);
        // Re-selecting the same server must not drop the open channel.
        nav.select_server(server);
        assert_eq!(nav.selected_server(), Some(server));
        assert_eq!(nav.selected_channel(), Some(channel));
    }

    #[test]
    fn switching_server_clears_selected_channel() {
        let mut nav = NavState::new();
        nav.select_server(Uuid::new_v4());
        nav.select_channel(Uuid::new_v4());

        let other = Uuid::new_v4();
        nav.select_server(other);
        assert_eq!(nav.selected_server(), Some(other));
        assert_eq!(nav.selected_channel(), None);
    }

    #[test]
    fn select_dm_is_independent_of_channel() {
        let mut nav = NavState::new();
        let dm = Uuid::new_v4();
        nav.select_dm(dm);
        assert_eq!(nav.selected_dm(), Some(dm));
        // Switching servers leaves the open DM untouched.
        nav.select_server(Uuid::new_v4());
        assert_eq!(nav.selected_dm(), Some(dm));
    }
}
