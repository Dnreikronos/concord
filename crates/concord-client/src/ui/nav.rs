//! Top-level navigation state: which primary view the client is showing.

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

/// Tracks which [`View`] is currently active.
///
/// Deliberately minimal for the skeleton — it holds just enough state to drive
/// the layout. Per-view state (selected channel, open DM, etc.) will hang off
/// the individual views as they are built out.
#[derive(Clone, Debug, Default)]
pub struct NavState {
    active: View,
}

impl NavState {
    /// Create state on the default view.
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
}
