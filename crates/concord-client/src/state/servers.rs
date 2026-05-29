//! Servers the user belongs to, the active selection, and the channels and
//! members loaded for each.
//!
//! The server list and a per-server channel list are loaded over REST on a
//! successful login; members are loaded lazily for whichever server is active.
//! Channels are kept sorted by `(position, name)` so the sidebar renders them
//! in a stable order regardless of fetch order.

use std::collections::HashMap;

use uuid::Uuid;

use concord_shared::types::{Channel, MemberInfo, Server};

/// The server list plus per-server channels and members.
#[derive(Default)]
pub struct ServersState {
    servers: Vec<Server>,
    active: Option<Uuid>,
    channels: HashMap<Uuid, Vec<Channel>>,
    members: HashMap<Uuid, Vec<MemberInfo>>,
    /// True while the initial server/channel fetch is in flight.
    loading: bool,
}

impl ServersState {
    /// Create empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the initial load is in progress.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Mark the initial load as in flight or finished.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// The servers the user belongs to.
    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// Replace the server list, defaulting the active server to the first one
    /// when the previous selection is gone (or there was none).
    pub fn set_servers(&mut self, servers: Vec<Server>) {
        let active_still_present = self
            .active
            .is_some_and(|id| servers.iter().any(|s| s.id == id));
        if !active_still_present {
            self.active = servers.first().map(|s| s.id);
        }
        self.servers = servers;
    }

    /// Insert or update a single server, selecting it if it is the first one.
    pub fn upsert_server(&mut self, server: Server) {
        match self.servers.iter_mut().find(|s| s.id == server.id) {
            Some(existing) => *existing = server,
            None => {
                if self.servers.is_empty() {
                    self.active = Some(server.id);
                }
                self.servers.push(server);
            }
        }
    }

    /// The active server's id, if any.
    pub fn active_server(&self) -> Option<Uuid> {
        self.active
    }

    /// The active server's full record, if any.
    pub fn active_server_info(&self) -> Option<&Server> {
        let id = self.active?;
        self.servers.iter().find(|s| s.id == id)
    }

    /// Select `server_id` as active, if it is in the list.
    pub fn set_active(&mut self, server_id: Uuid) {
        if self.servers.iter().any(|s| s.id == server_id) {
            self.active = Some(server_id);
        }
    }

    /// Channels loaded for `server_id` (empty if none loaded yet).
    pub fn channels_for(&self, server_id: Uuid) -> &[Channel] {
        self.channels
            .get(&server_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Channels for the active server.
    pub fn active_channels(&self) -> &[Channel] {
        match self.active {
            Some(id) => self.channels_for(id),
            None => &[],
        }
    }

    /// Store the channels for `server_id`, sorted by `(position, name)`.
    pub fn set_channels(&mut self, server_id: Uuid, mut channels: Vec<Channel>) {
        channels.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
        self.channels.insert(server_id, channels);
    }

    /// Members loaded for `server_id` (empty if none loaded yet).
    pub fn members_for(&self, server_id: Uuid) -> &[MemberInfo] {
        self.members
            .get(&server_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Store the members for `server_id`.
    pub fn set_members(&mut self, server_id: Uuid, members: Vec<MemberInfo>) {
        self.members.insert(server_id, members);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use concord_shared::types::{ChannelType, Server};

    fn server(name: &str) -> Server {
        Server {
            id: Uuid::new_v4(),
            name: name.into(),
            icon_url: None,
            owner_id: Uuid::nil(),
            created_at: Utc::now(),
        }
    }

    fn channel(server_id: Uuid, name: &str, position: i32) -> Channel {
        Channel {
            id: Uuid::new_v4(),
            server_id,
            category_id: None,
            name: name.into(),
            topic: None,
            channel_type: ChannelType::Text,
            position,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn set_servers_defaults_active_to_first() {
        let mut s = ServersState::new();
        let a = server("alpha");
        let id = a.id;
        s.set_servers(vec![a, server("beta")]);
        assert_eq!(s.active_server(), Some(id));
        assert_eq!(s.active_server_info().map(|s| s.name.as_str()), Some("alpha"));
    }

    #[test]
    fn set_servers_keeps_valid_active_selection() {
        let mut s = ServersState::new();
        let a = server("alpha");
        let b = server("beta");
        let (a_id, b_id) = (a.id, b.id);
        s.set_servers(vec![a, b]);
        s.set_active(b_id);
        // Re-fetching the list must not reset a still-valid selection.
        s.set_servers(vec![server("gamma"), {
            let mut keep = server("beta");
            keep.id = b_id;
            keep
        }]);
        assert_eq!(s.active_server(), Some(b_id));
        let _ = a_id;
    }

    #[test]
    fn channels_are_sorted_by_position_then_name() {
        let mut s = ServersState::new();
        let srv = server("alpha");
        let id = srv.id;
        s.set_servers(vec![srv]);
        s.set_channels(
            id,
            vec![
                channel(id, "zeta", 1),
                channel(id, "alpha", 1),
                channel(id, "first", 0),
            ],
        );
        let names: Vec<_> = s.active_channels().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["first", "alpha", "zeta"]);
    }

    #[test]
    fn missing_channels_and_members_are_empty() {
        let s = ServersState::new();
        assert!(s.channels_for(Uuid::new_v4()).is_empty());
        assert!(s.members_for(Uuid::new_v4()).is_empty());
        assert!(s.active_channels().is_empty());
    }

    #[test]
    fn upsert_selects_first_then_updates_in_place() {
        let mut s = ServersState::new();
        let mut a = server("alpha");
        let id = a.id;
        s.upsert_server(a.clone());
        assert_eq!(s.active_server(), Some(id));
        a.name = "renamed".into();
        s.upsert_server(a);
        assert_eq!(s.servers().len(), 1);
        assert_eq!(s.active_server_info().map(|s| s.name.as_str()), Some("renamed"));
    }
}
