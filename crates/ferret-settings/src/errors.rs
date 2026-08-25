use std::io;
use std::path::PathBuf;

/// This crate's `Result`, with its error type as the default.
pub type Result<T, E = SettingsError> = std::result::Result<T, E>;

/// Failures worth telling the user about.
///
/// Note what is *not* here: a malformed settings file. Settings never fail to
/// load — a section that will not parse falls back to defaults and reports a
/// [`crate::Warning`]. Refusing to start because a config file is wrong is a
/// worse outcome than starting with defaults and saying so.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("Ferret can't find a place to store its settings on this system.")]
    NoConfigDirectory,

    #[error("Couldn't create the settings folder at {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("Couldn't read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("Couldn't save settings to {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A recoverable problem found while loading.
///
/// Surfaced as a notification rather than swallowed: silently ignoring a setting
/// the user deliberately typed is how a config file stops being trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// Dotted path to the offending key, e.g. `browser.default_view`.
    pub key: String,
    /// What was wrong, phrased for a person.
    pub problem: String,
    /// What Ferret did instead.
    pub resolution: String,
}

impl Warning {
    pub fn new(
        key: impl Into<String>,
        problem: impl Into<String>,
        resolution: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            problem: problem.into(),
            resolution: resolution.into(),
        }
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} {}", self.key, self.problem, self.resolution)
    }
}
