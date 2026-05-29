//! Asset source for the desktop client.
//!
//! Overlays the client's own bundled icons on top of gpui-component's set.
//! gpui-component ships a GitHub mark but no Google one, so the Google icon is
//! bundled here and served by path; everything else falls through to the
//! component library's assets.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component_assets::Assets as ComponentAssets;

/// The Google "G" mark. gpui renders SVG icons as a single-color mask, so it
/// shows tinted (see the OAuth button), not in the four brand colors.
const GOOGLE_SVG: &[u8] = include_bytes!("../../assets/icons/google.svg");

/// Asset source serving the client's own icons, delegating the rest to
/// gpui-component.
pub struct ConcordAssets;

impl AssetSource for ConcordAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icons/google.svg" => Ok(Some(Cow::Borrowed(GOOGLE_SVG))),
            _ => ComponentAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        ComponentAssets.list(path)
    }
}
