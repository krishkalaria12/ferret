use crate::PlanIssue;
use ferret_vfs::{VfsError, VfsPath};

/// This crate's `Result`, with its error type as the default.
///
/// The default is [`OpsError`], because the queue is the crate's centre: every
/// signature reads `Result<T>` and the error is never spelled out. The
/// batch-rename half has a genuinely different failure vocabulary — a rule that
/// does not compile is not a filesystem problem — so it says so through
/// [`RenameResult`] rather than growing a variant for it.
pub type Result<T, E = OpsError> = std::result::Result<T, E>;

/// The batch rename's own result, for the parts of the crate that are about
/// renaming rather than about running an operation.
pub type RenameResult<T, E = BatchRenameError> = std::result::Result<T, E>;

/// A queued operation could not be started or could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    /// The backend under this path cannot do what the job needs.
    ///
    /// Asked for up front rather than discovered at the last step: a queue that
    /// accepts a job and then fails it is worse than one that says no, because
    /// the user has already watched it sit in the list.
    #[error("{path} cannot {capability}, so Ferret will not queue that operation there.")]
    Unsupported { path: VfsPath, capability: String },

    /// The two ends of a job live on different backends.
    ///
    /// Not a failure the queue can resolve: moving bytes between a server and
    /// local disk is a transfer, and the transfer path already exists with its
    /// own progress and its own cancel.
    #[error(
        "{from} and {to} are on different filesystems, so this has to be a transfer rather than a move."
    )]
    CrossBackend { from: VfsPath, to: VfsPath },

    /// One source failed. The job carries on with the rest.
    ///
    /// Collected rather than returned: a copy of 1,200 files where four fail is
    /// a copy that worked, and stopping at the first error would turn a
    /// recoverable problem into a failed job.
    #[error("{path}: {message}")]
    Source { path: VfsPath, message: String },

    /// A job with no sources. Enqueueing one would show a row that can never
    /// do anything.
    #[error("There are no files to operate on.")]
    NothingToDo,

    /// Nobody is listening, so the id means nothing.
    #[error("That operation is no longer in the queue.")]
    UnknownJob,

    /// The job is over, and the ask does not apply to it.
    #[error("That operation already {state}.")]
    AlreadySettled { state: &'static str },
}

impl OpsError {
    /// Whether this is "stop, and say so", rather than "skip this one".
    pub fn is_fatal(&self) -> bool {
        !matches!(self, OpsError::Source { .. })
    }

    /// A single source's failure, flattened for the job's failure list.
    pub fn describe(&self) -> String {
        self.to_string()
    }
}

impl From<VfsError> for OpsError {
    fn from(source: VfsError) -> Self {
        match &source {
            // The one case the queue itself has to act on rather than record:
            // the router refuses a cross-backend pair so that it becomes a
            // transfer, and swallowing that into a per-source failure list
            // would leave a job that silently copied nothing.
            VfsError::CrossBackend { from, to } => OpsError::CrossBackend {
                from: from.clone(),
                to: to.clone(),
            },
            _ => OpsError::Source {
                path: source
                    .path()
                    .cloned()
                    .unwrap_or_else(|| VfsPath::local("/")),
                message: source.to_string(),
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BatchRenameError {
    #[error("Fix rename rule {rule_number}: {message}", rule_number = rule_index + 1)]
    InvalidRule { rule_index: usize, message: String },

    #[error("Ferret can't read {path} while checking the rename plan.")]
    ReadDirectory {
        path: ferret_vfs::VfsPath,
        #[source]
        source: VfsError,
    },

    #[error("{path} cannot guarantee {capability}, so Ferret will not start a batch rename there.")]
    Unsupported {
        path: ferret_vfs::VfsPath,
        capability: &'static str,
    },

    #[error(
        "The rename plan has {issue_count} conflicts. First conflict: {first}. Fix the highlighted rows before applying it.",
        issue_count = issues.len(),
        first = issues.first().map(ToString::to_string).unwrap_or_else(|| "unknown".into())
    )]
    InvalidPlan { issues: Vec<PlanIssue> },

    #[error("The batch rename failed and every completed change was restored. {source}")]
    ApplyFailedRolledBack {
        #[source]
        source: VfsError,
    },

    #[error(
        "The batch rename failed and Ferret could not restore every path. Keep Ferret open and recover these paths: {summary}. {source}"
    )]
    RecoveryRequired {
        recovery_paths: Vec<ferret_vfs::VfsPath>,
        summary: String,
        #[source]
        source: VfsError,
    },
}

pub(crate) fn recovery_summary(paths: &[ferret_vfs::VfsPath]) -> String {
    let shown = paths
        .iter()
        .take(3)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > 3 {
        format!("{shown}, and {} more", paths.len() - 3)
    } else {
        shown
    }
}
