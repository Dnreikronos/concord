//! Discord-dark inspired theme constants: colors, spacing, and fonts.
//!
//! These are the source of truth for the client's look. The GPUI component
//! library carries its own [`gpui_component::Theme`]; we switch it to dark mode
//! at startup and otherwise style our own surfaces directly from the palette
//! below so the result stays consistent regardless of the component defaults.

use gpui::{Hsla, rgb};

/// Color palette. Functions rather than constants because building an [`Hsla`]
/// from a hex literal is not a `const` operation.
pub mod color {
    use super::*;

    /// Leftmost server rail — the darkest surface.
    pub fn server_rail() -> Hsla {
        rgb(0x1e1f22).into()
    }

    /// Channel / DM sidebar.
    pub fn sidebar() -> Hsla {
        rgb(0x2b2d31).into()
    }

    /// Main chat surface.
    pub fn chat() -> Hsla {
        rgb(0x313338).into()
    }

    /// Slightly raised surface (headers, inputs).
    pub fn elevated() -> Hsla {
        rgb(0x383a40).into()
    }

    /// Hover background for interactive rows.
    pub fn hover() -> Hsla {
        rgb(0x35373c).into()
    }

    /// Background for the selected row.
    pub fn active() -> Hsla {
        rgb(0x404249).into()
    }

    /// Hairline borders between surfaces.
    pub fn border() -> Hsla {
        rgb(0x26282c).into()
    }

    /// Primary readable text.
    pub fn text() -> Hsla {
        rgb(0xdbdee1).into()
    }

    /// Secondary text (channel names, captions).
    pub fn text_muted() -> Hsla {
        rgb(0x949ba4).into()
    }

    /// Lowest-emphasis text.
    pub fn text_faint() -> Hsla {
        rgb(0x80848e).into()
    }

    /// Text/icon color on an active or accented background.
    pub fn interactive_active() -> Hsla {
        rgb(0xffffff).into()
    }

    /// Brand "blurple" accent.
    pub fn accent() -> Hsla {
        rgb(0x5865f2).into()
    }

    /// Hovered accent.
    pub fn accent_hover() -> Hsla {
        rgb(0x4752c4).into()
    }

    /// Online / success.
    pub fn online() -> Hsla {
        rgb(0x23a55a).into()
    }

    /// Danger / destructive.
    pub fn danger() -> Hsla {
        rgb(0xf23f43).into()
    }
}

/// Spacing scale in logical pixels. Use with [`gpui::px`].
pub mod space {
    /// 4px.
    pub const XS: f32 = 4.0;
    /// 8px.
    pub const SM: f32 = 8.0;
    /// 12px.
    pub const MD: f32 = 12.0;
    /// 16px.
    pub const LG: f32 = 16.0;
    /// 24px.
    pub const XL: f32 = 24.0;

    /// Width of the server rail (leftmost column).
    pub const SERVER_RAIL: f32 = 72.0;
    /// Width of the channel / DM sidebar.
    pub const SIDEBAR: f32 = 240.0;
    /// Height of the top bar / channel header.
    pub const HEADER: f32 = 48.0;
    /// Side length of a circular rail button.
    pub const RAIL_BUTTON: f32 = 48.0;
}

/// Font settings in logical pixels.
pub mod font {
    /// Primary UI font. Discord ships "gg sans"; we fall back to a common
    /// system face until bundled fonts land.
    pub const FAMILY: &str = "Helvetica";
    /// Small text (captions, channel list).
    pub const SM: f32 = 13.0;
    /// Body text.
    pub const MD: f32 = 15.0;
    /// Section headers.
    pub const LG: f32 = 16.0;
    /// Large title.
    pub const TITLE: f32 = 20.0;
}
