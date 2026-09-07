//! This crate's error type and `Result` alias.

/// This crate's `Result`, with its error type as the default.
///
/// The default parameter ties the two together: `Result<T>` means
/// `Result<T, PreviewError>` at every call site, so the error type is never
/// spelled out and never accidentally varies.
pub type Result<T, E = PreviewError> = std::result::Result<T, E>;

/// Failures the preview pane has to render.
///
/// Every variant carries the file it concerns, because an error that does not
/// name its subject cannot be shown to a person unedited — and the pane shows
/// these unedited.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error(
        "Ferret can't preview {name}: the file is {size} and this kind of preview stops at {budget}."
    )]
    OverBudget {
        name: String,
        size: u64,
        budget: usize,
    },

    #[error("Ferret can't preview {name}: {reason}.")]
    DecodeFailed { name: String, reason: &'static str },
}
