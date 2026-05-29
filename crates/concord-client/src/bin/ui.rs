//! Entry point for the Concord desktop client.
//!
//! Boots GPUI, opens the main window in dark mode, and mounts [`ConcordApp`]
//! as the root view. Run with: `cargo run -p concord-client --bin concord-ui
//! --features gui`.

use gpui::*;
use gpui_component::{Root, Theme, ThemeMode, TitleBar};

use concord_client::ui::{ConcordApp, ConcordAssets};

/// Initial window dimensions, in logical pixels.
const WINDOW_WIDTH: f32 = 1100.0;
const WINDOW_HEIGHT: f32 = 720.0;

fn main() {
    let app = gpui_platform::application().with_assets(ConcordAssets);

    app.run(move |cx| {
        gpui_component::init(cx);

        let window_options = WindowOptions {
            titlebar: Some(TitleBar::title_bar_options()),
            window_bounds: Some(WindowBounds::centered(
                size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
                cx,
            )),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.set_window_title("Concord");
                Theme::change(ThemeMode::Dark, Some(window), cx);

                let view = cx.new(|cx| ConcordApp::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open main window");
        })
        .detach();
    });
}
