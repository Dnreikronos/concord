//! Shared async plumbing for the data-backed views.
//!
//! GPUI runs its own (non-tokio) executor, so the `reqwest`-backed REST calls
//! can't be awaited on it directly. They are spawned on a dedicated tokio
//! [`runtime`] and their results handed back over a oneshot channel that a GPUI
//! task awaits (the same bridge the auth screen uses). [`LoadState`] tracks the
//! lifecycle of each fetched resource so the views can render loading, empty,
//! and error states without juggling separate flags.

use std::sync::OnceLock;

/// Shared tokio runtime used to drive the REST calls off GPUI's executor.
/// Built once and reused so every fetch shares one connection pool.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Runtime::new().expect("failed to start tokio runtime for API requests")
    })
}

/// The lifecycle of a fetched resource.
#[derive(Default)]
pub enum LoadState<T> {
    /// Not requested yet.
    #[default]
    Idle,
    /// A request is in flight.
    Loading,
    /// Loaded successfully — the payload may still be empty.
    Loaded(T),
    /// The request failed, carrying a user-facing message.
    Failed(String),
}

impl<T> LoadState<T> {
    /// The loaded payload, if the last fetch succeeded.
    pub fn loaded(&self) -> Option<&T> {
        match self {
            Self::Loaded(value) => Some(value),
            _ => None,
        }
    }
}
