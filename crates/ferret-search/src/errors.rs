use ferret_vfs::VfsPath;

pub type Result<T, E = SearchError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("Ferret can't search without a filename or content query.")]
    EmptyQuery,
    #[error("Ferret can't search without at least one folder.")]
    EmptyScope,
    #[error(
        "Ferret can't search for a filename longer than {max_bytes} bytes (got {actual_bytes})."
    )]
    PatternTooLong {
        max_bytes: usize,
        actual_bytes: usize,
    },
    #[error("Ferret can't run a search whose result limit is zero.")]
    ZeroResultLimit,
    #[error("Ferret can't use {mode} filename matching yet; choose substring matching.")]
    UnsupportedNameMode { mode: &'static str },
    #[error("Ferret can't search file contents yet; clear the content query and search filenames.")]
    UnsupportedContentSearch,
    #[error("Ferret couldn't compile the filename query {pattern:?}: {message}")]
    InvalidNamePattern { pattern: String, message: String },
    #[error("Ferret couldn't walk {path}: {message}")]
    Walk { path: VfsPath, message: String },
}
