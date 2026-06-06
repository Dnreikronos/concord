//! Linux/BSD notification backend, over the freedesktop D-Bus protocol via
//! `notify-rust`. Showing the notification registers a `default` action — the
//! one a compliant server invokes when the notification body is clicked — and
//! then blocks the worker thread until the user clicks it or it is dismissed.

use notify_rust::Notification;
use tokio::sync::mpsc::UnboundedSender;

use super::{NotifyTarget, Request};

/// The freedesktop action id invoked when the notification body itself is
/// clicked (as opposed to a named action button).
const DEFAULT_ACTION: &str = "default";

pub(super) fn deliver(request: Request, clicks: UnboundedSender<NotifyTarget>) {
    let Request { title, body, target } = request;
    let handle = match Notification::new()
        .appname("Concord")
        .summary(&title)
        .body(&body)
        .action(DEFAULT_ACTION, "Open")
        .show()
    {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!(error = %err, "failed to show desktop notification");
            return;
        }
    };

    // Blocks until the body's `default` action fires or the notification is
    // closed/dismissed (which arrives as a different action and is ignored).
    handle.wait_for_action(|action| {
        if action == DEFAULT_ACTION {
            let _ = clicks.send(target);
        }
    });
}
