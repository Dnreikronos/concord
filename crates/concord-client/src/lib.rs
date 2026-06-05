pub mod ws;

/// Authentication REST client. The wire types and OAuth URL builder are always
/// available (and unit-tested); the HTTP calls themselves are `gui`-only, so
/// the module is compiled for the desktop client and for tests.
#[cfg(any(feature = "gui", test))]
pub mod auth;

/// Authenticated reads against the REST API (servers, channels, messages, DMs).
/// `gui`-only — these are `reqwest` callers like the auth HTTP round-trips.
#[cfg(feature = "gui")]
pub mod api;

/// Desktop client UI. Navigation state is always available; the GPUI-backed
/// views (theme, root layout) compile only with the `gui` feature.
pub mod ui;
