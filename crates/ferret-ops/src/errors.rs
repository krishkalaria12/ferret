use crate::PlanIssue;
use ferret_vfs::VfsError;

pub type Result<T, E = BatchRenameError> = std::result::Result<T, E>;

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
