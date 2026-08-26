//! The application's asset source.
//!
//! GPUI takes exactly one [`AssetSource`], so this one merges two sets:
//! Ferret's own SVGs and mapping tables (this crate's `assets/` directory)
//! first, falling back to `gpui-component-assets` for the stock icon set
//! behind `IconName`. Without the fallback every library component renders
//! blank; without the first half Ferret's own icons do.

use gpui::AssetSource;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
pub(crate) struct FerretAssets;

/// The merged asset source. Register with `.with_assets(Assets)` in the bin.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(data) = FerretAssets::get(path) {
            return Ok(Some(data.data));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut out = FerretAssets::iter().map(Into::into).collect::<Vec<_>>();
        out.extend(gpui_component_assets::Assets.list(path)?);
        Ok(out)
    }
}

/// The embedded icon-mapping table, as raw JSON text.
pub fn file_icons_json() -> String {
    // The table ships under `assets/icons/`, so that is its embed key.
    FerretAssets::get("icons/file-icons.json")
        .map(|f| String::from_utf8_lossy(&f.data).into_owned())
        .unwrap_or_else(|| "{}".to_string())
}
