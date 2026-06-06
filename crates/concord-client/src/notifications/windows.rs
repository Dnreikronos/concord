//! Windows notification backend, over WinRT toasts via
//! `tauri-winrt-notification`. Unlike the Linux and macOS backends, showing a
//! toast returns immediately and the click arrives later through an
//! `on_activated` callback dispatched by the WinRT runtime on its own thread.
//!
//! The worker thread therefore just shows the toast and exits; the registered
//! handler and its captured sender outlive it because the process keeps running
//! (the UI is on the main thread) and the runtime retains the toast while it is
//! on screen. An unpackaged binary has no registered AppUserModelID, so the
//! toast is posted under the built-in PowerShell id.

use tauri_winrt_notification::Toast;
use tokio::sync::mpsc::UnboundedSender;

use super::{NotifyTarget, Request};

pub(super) fn deliver(request: Request, clicks: UnboundedSender<NotifyTarget>) {
    let Request { title, body, target } = request;
    let result = Toast::new(Toast::POWERSHELL_APP_ID)
        .title(&title)
        .text1(&body)
        .on_activated(move |_action| {
            let _ = clicks.send(target);
            Ok(())
        })
        .show();

    if let Err(err) = result {
        tracing::warn!(error = %err, "failed to show desktop notification");
    }
}
