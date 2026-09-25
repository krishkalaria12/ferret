//! Ferret's file-operation engine: the queue every bulk mutation goes through,
//! and the batch rename planner.
//!
//! ## The queue is the only path
//!
//! Every copy, move, delete and trash in Ferret is a [`queue::Job`] here (F-60).
//! Nothing else writes files in bulk, which is the point: progress, cancellation
//! and cleanup are the parts that are easy to get subtly wrong, and one place to
//! get them right is cheaper than four places to get them mostly right.
//!
//! ## It is GPUI-free, on purpose
//!
//! Nothing in this crate imports a UI framework, so a service crate can queue
//! work without depending on a view. Observation is a callback ([`queue::Queue::observe`])
//! rather than an entity, and the queue is held wherever the application holds
//! its long-lived state — a job has to outlive the tab that started it.

mod apply;
mod errors;
mod execute;
mod job;
mod measure;
mod plan;
mod preflight;
mod queue;
mod rules;

pub mod config;

pub use apply::apply;
pub use errors::{BatchRenameError, OpsError, RenameResult, Result};
pub use execute::{Handle, ProgressSink};
pub use job::{Job, JobId, JobKind, JobProgress, JobState};
pub use measure::Measurement;
pub use plan::{BatchRenamePlan, PlanIssue, RenameRow};
pub use preflight::ExistingNames;
pub use queue::{Event, Observer, Queue, Request};
pub use rules::{
    CaseChange, CaseStyle, ExtensionChange, FindReplace, MatchSyntax, NumberPlacement, Numbering,
    RenameRule, RuleTarget, TextAffix, validate_file_name,
};

use ferret_vfs::{Vfs, VfsPath};

/// Start a queue over a storage layer and the platform's trash.
///
/// The two are both here because a queue that could only do one of them would
/// make the caller reimplement the other: trashing through the `Vfs` is
/// refused by every backend on purpose (gate 5), so the platform is the only
/// way in.
pub fn queue(
    fs: std::sync::Arc<dyn Vfs>,
    platform: std::sync::Arc<dyn ferret_platform::Platform>,
) -> Queue {
    Queue::new(fs, platform)
}

pub fn plan_single(source: VfsPath, new_name: &str) -> Result<BatchRenamePlan, BatchRenameError> {
    plan::build_single(source, new_name)
}

pub async fn load_existing(
    fs: &dyn Vfs,
    sources: &[VfsPath],
) -> Result<ExistingNames, BatchRenameError> {
    preflight::load_existing(fs, sources).await
}

pub fn plan_with_existing(
    sources: &[VfsPath],
    rules: &[RenameRule],
    existing: &ExistingNames,
) -> Result<BatchRenamePlan, BatchRenameError> {
    let mut plan = plan::build_plan(sources, rules)?;
    preflight::validate_plan(&mut plan, existing);
    Ok(plan)
}

pub async fn preview(
    fs: &dyn Vfs,
    sources: &[VfsPath],
    rules: &[RenameRule],
) -> Result<BatchRenamePlan, BatchRenameError> {
    let existing = load_existing(fs, sources).await?;
    plan_with_existing(sources, rules, &existing)
}
