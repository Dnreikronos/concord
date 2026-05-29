pub mod ws;

/// Authentication REST client. The wire types and OAuth URL builder are always
/// available (and unit-tested); the HTTP calls themselves are `gui`-only, so
/// the module is compiled for the desktop client and for tests.
#[cfg(any(feature = "gui", test))]
pub mod auth;

/// Desktop client UI. Navigation state is always available; the GPUI-backed
/// views (theme, root layout) compile only with the `gui` feature.
pub mod ui;

/// Client application state (auth, servers, chat, connection). Pure data +
/// logic, mirrored into GPUI entities by the desktop client; compiled for the
/// desktop client and for tests, like [`auth`].
#[cfg(any(feature = "gui", test))]
pub mod state;

/// REST client for loading the data the UI renders (servers, channels,
/// members, message history). `gui`-only — like [`auth`], a user of `reqwest`.
#[cfg(feature = "gui")]
pub mod api;
