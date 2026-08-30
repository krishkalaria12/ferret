use std::path::PathBuf;

pub type Result<T, E = TerminalError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("Ferret couldn't initialize the {backend} terminal engine: {message}")]
    InitializeEngine {
        backend: &'static str,
        message: String,
    },
    #[error("Ferret couldn't create a terminal for {directory}: {source}")]
    OpenPty {
        directory: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Ferret couldn't start {shell} in {directory}: {message}")]
    SpawnShell {
        shell: String,
        directory: PathBuf,
        message: String,
    },
    #[error("Ferret couldn't communicate with the terminal: {0}")]
    Io(#[from] std::io::Error),
}
