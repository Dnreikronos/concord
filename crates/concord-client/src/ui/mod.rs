//! GPUI desktop client: window setup, theme, navigation, and root layout.

pub mod nav;
pub use nav::{NavState, View};

// GPUI-backed pieces — only built with the `gui` feature.
#[cfg(feature = "gui")]
pub mod auth_view;
#[cfg(feature = "gui")]
pub mod root;
#[cfg(feature = "gui")]
pub mod theme;

#[cfg(feature = "gui")]
pub use auth_view::{AuthEvent, AuthView};
#[cfg(feature = "gui")]
pub use root::ConcordApp;
