mod apply;
mod errors;
mod plan;
mod preflight;
mod rules;

pub mod config;

pub use apply::apply;
pub use errors::{BatchRenameError, Result};
pub use plan::{BatchRenamePlan, PlanIssue, RenameRow};
pub use preflight::ExistingNames;
pub use rules::{
    CaseChange, CaseStyle, ExtensionChange, FindReplace, MatchSyntax, NumberPlacement, Numbering,
    RenameRule, RuleTarget, TextAffix, validate_file_name,
};

use ferret_vfs::{Vfs, VfsPath};

pub fn plan_single(source: VfsPath, new_name: &str) -> Result<BatchRenamePlan> {
    plan::build_single(source, new_name)
}

pub async fn load_existing(fs: &dyn Vfs, sources: &[VfsPath]) -> Result<ExistingNames> {
    preflight::load_existing(fs, sources).await
}

pub fn plan_with_existing(
    sources: &[VfsPath],
    rules: &[RenameRule],
    existing: &ExistingNames,
) -> Result<BatchRenamePlan> {
    let mut plan = plan::build_plan(sources, rules)?;
    preflight::validate_plan(&mut plan, existing);
    Ok(plan)
}

pub async fn preview(
    fs: &dyn Vfs,
    sources: &[VfsPath],
    rules: &[RenameRule],
) -> Result<BatchRenamePlan> {
    let existing = load_existing(fs, sources).await?;
    plan_with_existing(sources, rules, &existing)
}
