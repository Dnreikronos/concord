//! macOS notification backend, over `NSUserNotification` via
//! `mac-notification-sys`. `send_notification` blocks the worker thread until
//! the user clicks, dismisses, or it times out, returning which happened.
//!
//! macOS posts notifications under a registered application bundle id. An
//! unbundled binary (e.g. `cargo run`) has none, so registration is best-effort
//! and a failure is logged rather than fatal — a properly bundled `.app` is what
//! makes these reliably appear.

use std::sync::Once;

use mac_notification_sys::{
    send_notification, set_application, MainButton, Notification, NotificationResponse,
};
use tokio::sync::mpsc::UnboundedSender;

use super::{NotifyTarget, Request};

/// Bundle identifier the notifications are posted under.
const BUNDLE_ID: &str = "com.concord.desktop";

pub(super) fn deliver(request: Request, clicks: UnboundedSender<NotifyTarget>) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(err) = set_application(BUNDLE_ID) {
            tracing::warn!(error = %err, "failed to register notification bundle id");
        }
    });

    let Request { title, body, target } = request;
    // A main action button makes `send_notification` wait for and report the
    // user's response rather than returning as soon as the banner is posted.
    let mut options = Notification::default();
    options.main_button(MainButton::SingleAction("Open"));

    match send_notification(&title, None, &body, Some(&options)) {
        Ok(NotificationResponse::Click | NotificationResponse::ActionButton(_)) => {
            let _ = clicks.send(target);
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "failed to show desktop notification"),
    }
}
