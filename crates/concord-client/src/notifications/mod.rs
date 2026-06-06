//! Native desktop notifications for incoming messages.
//!
//! Shows a platform-native notification (sender + a short preview) when a
//! message arrives and reports a click back to the caller so it can focus the
//! window and jump to the conversation. The backend is different on every OS —
//! freedesktop (D-Bus) on Linux/BSD, `NSUserNotification` on macOS, WinRT toasts
//! on Windows — and, awkwardly, each one exposes click handling through a
//! different API, so each lives in its own `cfg`-gated submodule behind the one
//! [`Notifier`] facade here.
//!
//! Notifications are shown from short-lived worker threads: the Linux and macOS
//! backends block until the user acts on (or dismisses) the notification, so a
//! click can't be reported without a thread to wait on. A click is delivered
//! over an unbounded channel; the root view drains it on the GPUI executor and
//! navigates from there.

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Where clicking a notification should take the user. Carries everything the
/// root view needs to navigate without re-deriving it from a message id.
#[derive(Debug, Clone, Copy)]
pub enum NotifyTarget {
    /// A message in a server channel.
    Channel { server_id: Uuid, channel_id: Uuid },
    /// A direct message (one-to-one or group).
    Dm { dm_channel_id: Uuid },
}

/// One notification to display, handed to a backend on a worker thread.
struct Request {
    title: String,
    body: String,
    target: NotifyTarget,
}

/// Shows native desktop notifications and reports clicks on them.
///
/// Cloneable so it can be held cheaply and captured into closures; every clone
/// reports clicks to the same receiver returned by [`Notifier::spawn`].
#[derive(Clone)]
pub struct Notifier {
    clicks: UnboundedSender<NotifyTarget>,
}

impl Notifier {
    /// Create a notifier together with the receiver that clicked notifications
    /// are delivered on. The caller drives the receiver (on the UI executor) to
    /// focus the window and navigate. Named `spawn` rather than `new` to match
    /// [`crate::ws::ConnectionHandle::spawn`]: like it, this hands back a handle
    /// plus the channel its background work reports on.
    pub fn spawn() -> (Self, UnboundedReceiver<NotifyTarget>) {
        let (clicks, rx) = unbounded_channel();
        (Self { clicks }, rx)
    }

    /// Show a notification with `title` and `body`, navigating to `target` when
    /// the user clicks it. Best-effort: each notification runs on its own
    /// worker thread (the Linux and macOS backends block waiting for the click),
    /// and a backend failure is logged and dropped rather than surfaced.
    pub fn notify(&self, title: String, body: String, target: NotifyTarget) {
        let request = Request { title, body, target };
        let clicks = self.clicks.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("concord-notify".into())
            .spawn(move || deliver(request, clicks))
        {
            tracing::warn!(error = %err, "failed to spawn notification thread");
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
use linux::deliver;
#[cfg(target_os = "macos")]
use macos::deliver;
#[cfg(target_os = "windows")]
use windows::deliver;

/// Fallback for any target without a notification backend: silently drop the
/// request. The desktop client only ships on Linux/macOS/Windows, so this never
/// runs there; it just keeps the crate buildable elsewhere.
#[cfg(not(any(all(unix, not(target_os = "macos")), target_os = "macos", target_os = "windows")))]
fn deliver(_request: Request, _clicks: UnboundedSender<NotifyTarget>) {}
