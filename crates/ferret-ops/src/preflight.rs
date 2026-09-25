use std::collections::{HashMap, HashSet};

use ferret_vfs::{ListingDetail, Vfs, VfsPath};
use futures::StreamExt as _;

use crate::plan::{BatchRenamePlan, NameKey, PlanIssue, RenameRow};
use crate::{BatchRenameError, Result};

pub struct ExistingNames {
    directories: HashMap<VfsPath, HashSet<NameKey>>,
}

pub async fn load_existing(fs: &dyn Vfs, sources: &[VfsPath]) -> Result<ExistingNames> {
    let mut parents = Vec::new();
    let mut seen = HashSet::new();
    for source in sources {
        if let Some(parent) = source.parent()
            && seen.insert(parent.clone())
        {
            parents.push(parent);
        }
    }

    let mut directories = HashMap::with_capacity(parents.len());
    for parent in parents {
        fs.invalidate(&parent);
        let mut names = HashSet::new();
        let mut stream = fs.read_dir(&parent, ListingDetail::NamesOnly);
        while let Some(item) = stream.next().await {
            match item {
                Ok(batch) => {
                    names.extend(
                        batch
                            .iter()
                            .map(|entry| NameKey::new(entry.name(), &parent)),
                    );
                }
                Err(source) => {
                    return Err(BatchRenameError::ReadDirectory {
                        path: parent,
                        source,
                    });
                }
            }
        }
        directories.insert(parent, names);
    }
    Ok(ExistingNames { directories })
}

pub fn validate_plan(plan: &mut BatchRenamePlan, existing: &ExistingNames) {
    let movable_targets = movable_targets(plan);
    for row in plan.rows_mut() {
        let issues = row_issues(row, &existing.directories, &movable_targets);
        row.issues_mut().extend(issues);
    }
    plan.recount();
}

pub(super) async fn preflight_issues(
    fs: &dyn Vfs,
    plan: &BatchRenamePlan,
) -> Result<Vec<PlanIssue>> {
    let mut issues = plan.issues().cloned().collect::<Vec<_>>();
    if !issues.is_empty() {
        return Ok(issues);
    }
    let existing = load_existing(fs, &plan.source_paths()).await?;
    let movable_targets = movable_targets(plan);
    issues.extend(
        plan.rows()
            .iter()
            .flat_map(|row| row_issues(row, &existing.directories, &movable_targets)),
    );
    Ok(issues)
}

fn movable_targets(plan: &BatchRenamePlan) -> HashSet<(VfsPath, NameKey)> {
    plan.rows()
        .iter()
        .filter(|row| row.changed() && row.readable())
        .filter_map(|row| {
            let parent = row.destination.parent()?;
            Some((parent, NameKey::new(&row.old_name, &row.source)))
        })
        .collect()
}

fn row_issues(
    row: &RenameRow,
    directories: &HashMap<VfsPath, HashSet<NameKey>>,
    movable_targets: &HashSet<(VfsPath, NameKey)>,
) -> Vec<PlanIssue> {
    let mut issues = Vec::new();
    if !row.readable() {
        return issues;
    }
    let Some(source_parent) = row.source.parent() else {
        return issues;
    };
    let source_key = NameKey::new(&row.old_name, &row.source);
    if !directories
        .get(&source_parent)
        .is_some_and(|names| names.contains(&source_key))
    {
        issues.push(PlanIssue::MissingSource {
            path: row.source.clone(),
        });
        return issues;
    }
    if !row.changed() {
        return issues;
    }
    let Some(target_parent) = row.destination.parent() else {
        return issues;
    };
    let target_key = NameKey::new(&row.new_name, &row.source);
    let exists = directories
        .get(&target_parent)
        .is_some_and(|names| names.contains(&target_key));
    if exists && !movable_targets.contains(&(target_parent, target_key)) {
        issues.push(PlanIssue::TargetExists {
            source: row.source.clone(),
            target: row.destination.clone(),
        });
    }
    issues
}
