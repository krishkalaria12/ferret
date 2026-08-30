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

    /// Assets under `path`, from both halves.
    ///
    /// The prefix filter is not decoration: this returned *every* embedded
    /// Ferret asset whatever it was asked for, and only delegated the filter
    /// to the library half (A-36). Nothing depended on the answer being right,
    /// which is why it went unnoticed — and "nothing calls this correctly yet"
    /// is a poor thing to rely on.
    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        let mut out: Vec<gpui::SharedString> = FerretAssets::iter()
            .filter(|asset| asset.starts_with(path))
            .map(Into::into)
            .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_a_directory_does_not_return_the_whole_bundle() {
        let icons = Assets
            .list("icons/file/")
            .expect("the icon directory lists");
        assert!(!icons.is_empty(), "the bundled file icons must be found");
        assert!(
            icons.iter().all(|a| a.starts_with("icons/file/")),
            "list(path) must return assets under `path`, not everything"
        );

        // And a path with nothing under it returns nothing from our half.
        let none = Assets
            .list("icons/nothing-here/")
            .expect("an empty listing is not an error");
        assert!(
            !none.iter().any(|a| a.starts_with("icons/file/")),
            "an unrelated prefix must not drag in the icon set"
        );
    }
}
