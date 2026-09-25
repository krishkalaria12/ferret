use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

#[cfg(any(target_os = "macos", windows))]
use caseless::default_case_fold_str;
use ferret_vfs::VfsPath;
#[cfg(target_os = "macos")]
use unicode_normalization::UnicodeNormalization as _;

use crate::rules::{CompiledRules, RenameRule, TransformError, validate_file_name};
use crate::{BatchRenameError, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchRenamePlan {
    rows: Vec<RenameRow>,
    changed: usize,
    issue_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameRow {
    pub source: VfsPath,
    pub destination: VfsPath,
    pub old_name: String,
    pub new_name: String,
    readable: bool,
    changed: bool,
    issues: Vec<PlanIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanIssue {
    UnreadableName {
        path: VfsPath,
    },
    InvalidName {
        path: VfsPath,
        reason: &'static str,
    },
    NumberOverflow {
        path: VfsPath,
    },
    DuplicateSource {
        path: VfsPath,
    },
    OverlappingSelection {
        ancestor: VfsPath,
        descendant: VfsPath,
    },
    DuplicateTarget {
        sources: Arc<[VfsPath]>,
        target: VfsPath,
    },
    UnchangedSourceInWay {
        target: VfsPath,
        blockers: Vec<VfsPath>,
    },
    MissingSource {
        path: VfsPath,
    },
    TargetExists {
        source: VfsPath,
        target: VfsPath,
    },
}

impl fmt::Display for PlanIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnreadableName { path } => write!(
                formatter,
                "{path} has a name that is not valid Unicode, so Ferret won't guess a replacement."
            ),
            Self::InvalidName { path, reason } => {
                write!(formatter, "The new name for {path} {reason}.")
            }
            Self::NumberOverflow { path } => {
                write!(formatter, "The sequence number for {path} is too large.")
            }
            Self::DuplicateSource { path } => {
                write!(formatter, "{path} appears more than once in the selection.")
            }
            Self::OverlappingSelection {
                ancestor,
                descendant,
            } => write!(
                formatter,
                "{ancestor} contains selected item {descendant}. Rename them in separate batches."
            ),
            Self::DuplicateTarget { sources, target } => write!(
                formatter,
                "{} would all become {target}.",
                join_paths(sources, 3)
            ),
            Self::UnchangedSourceInWay { target, blockers } => write!(
                formatter,
                "{} would move onto unchanged selected item {}{}.",
                target,
                join_paths(blockers, 3),
                if blockers.len() > 3 {
                    ", and others"
                } else {
                    ""
                }
            ),
            Self::MissingSource { path } => {
                write!(
                    formatter,
                    "{path} no longer exists or is no longer readable."
                )
            }
            Self::TargetExists { source, target } => {
                write!(
                    formatter,
                    "{source} cannot become {target} because it already exists."
                )
            }
        }
    }
}

impl std::error::Error for PlanIssue {}

impl BatchRenamePlan {
    pub(crate) fn from_rows(rows: Vec<RenameRow>) -> Self {
        let changed = rows.iter().filter(|row| row.changed).count();
        let issue_count = rows.iter().map(|row| row.issues.len()).sum();
        Self {
            rows,
            changed,
            issue_count,
        }
    }

    pub fn rows(&self) -> &[RenameRow] {
        &self.rows
    }

    pub(crate) fn rows_mut(&mut self) -> &mut [RenameRow] {
        &mut self.rows
    }

    pub fn changed_len(&self) -> usize {
        self.changed
    }

    pub fn can_apply(&self) -> bool {
        self.changed > 0 && self.issue_count == 0
    }

    pub fn has_issues(&self) -> bool {
        self.issue_count > 0
    }

    pub fn issue_count(&self) -> usize {
        self.issue_count
    }

    pub(crate) fn recount(&mut self) {
        self.changed = self.rows.iter().filter(|row| row.changed).count();
        self.issue_count = self.rows.iter().map(|row| row.issues.len()).sum();
    }

    pub fn issues(&self) -> impl Iterator<Item = &PlanIssue> {
        self.rows.iter().flat_map(|row| row.issues.iter())
    }

    pub fn into_issues(self) -> Vec<PlanIssue> {
        self.rows
            .into_iter()
            .flat_map(|row| row.issues)
            .collect::<Vec<_>>()
    }

    pub fn changed_paths(&self) -> Vec<VfsPath> {
        self.rows
            .iter()
            .filter(|row| row.changed)
            .map(|row| row.destination.clone())
            .collect()
    }

    pub fn source_paths(&self) -> Vec<VfsPath> {
        self.rows.iter().map(|row| row.source.clone()).collect()
    }

    pub fn changed_source_paths(&self) -> Vec<VfsPath> {
        self.rows
            .iter()
            .filter(|row| row.changed)
            .map(|row| row.source.clone())
            .collect()
    }

    pub fn destination_paths(&self) -> Vec<VfsPath> {
        self.rows
            .iter()
            .map(|row| row.destination.clone())
            .collect()
    }

    pub fn destination_for(&self, source: &VfsPath) -> Option<VfsPath> {
        self.rows
            .iter()
            .find(|row| &row.source == source)
            .map(|row| row.destination.clone())
    }

    pub fn renames(&self) -> impl Iterator<Item = (&VfsPath, &VfsPath)> {
        self.rows
            .iter()
            .filter(|row| row.changed)
            .map(|row| (&row.source, &row.destination))
    }
}

impl RenameRow {
    pub fn changed(&self) -> bool {
        self.changed
    }

    pub fn readable(&self) -> bool {
        self.readable
    }

    pub fn issues(&self) -> &[PlanIssue] {
        &self.issues
    }

    pub(crate) fn issues_mut(&mut self) -> &mut Vec<PlanIssue> {
        &mut self.issues
    }

    #[cfg(test)]
    pub(crate) fn for_test(source: VfsPath, old_name: String, new_name: String) -> Self {
        let destination = source.parent().unwrap().join(&new_name);
        Self {
            source,
            destination,
            old_name,
            new_name,
            readable: true,
            changed: true,
            issues: Vec::new(),
        }
    }
}

pub(super) fn build_single(source: VfsPath, new_name: &str) -> Result<BatchRenamePlan> {
    let old_name = source
        .file_name()
        .ok_or_else(|| BatchRenameError::InvalidRule {
            rule_index: 0,
            message: "The selected path has no readable file name.".into(),
        })?;
    let parent = source.parent().unwrap_or_else(|| source.clone());
    let changed = old_name != new_name;
    let destination = if changed {
        parent.join(new_name)
    } else {
        source.clone()
    };
    let mut issues = Vec::new();
    if changed && let Err(reason) = validate_file_name(new_name) {
        issues.push(PlanIssue::InvalidName {
            path: destination.clone(),
            reason,
        });
    }
    let mut rows = vec![RenameRow {
        source: source.clone(),
        destination,
        old_name: old_name.to_string(),
        new_name: new_name.to_string(),
        readable: true,
        changed,
        issues,
    }];
    validate_sources(&mut rows);
    validate_destinations(&mut rows);
    Ok(BatchRenamePlan::from_rows(rows))
}

pub(super) fn build_plan(sources: &[VfsPath], rules: &[RenameRule]) -> Result<BatchRenamePlan> {
    let compiled = CompiledRules::new(rules)?;
    let mut rows = Vec::with_capacity(sources.len());
    for source in sources {
        let Some(old_name) = source.file_name().map(str::to_string) else {
            rows.push(RenameRow {
                source: source.clone(),
                destination: source.clone(),
                old_name: source.to_string(),
                new_name: source.to_string(),
                readable: false,
                changed: false,
                issues: vec![PlanIssue::UnreadableName {
                    path: source.clone(),
                }],
            });
            continue;
        };
        let new_name = match compiled.transform(&old_name, rows.len()) {
            Ok(name) => name,
            Err(TransformError::NumberOverflow) => {
                rows.push(RenameRow {
                    source: source.clone(),
                    destination: source.clone(),
                    old_name: old_name.clone(),
                    new_name: old_name,
                    readable: true,
                    changed: false,
                    issues: vec![PlanIssue::NumberOverflow {
                        path: source.clone(),
                    }],
                });
                continue;
            }
        };
        let parent = source.parent().unwrap_or_else(|| source.clone());
        let changed = new_name != old_name;
        let destination = if changed {
            parent.join(&new_name)
        } else {
            source.clone()
        };
        let mut issues = Vec::new();
        if changed && let Err(reason) = validate_file_name(&new_name) {
            issues.push(PlanIssue::InvalidName {
                path: destination.clone(),
                reason,
            });
        }
        rows.push(RenameRow {
            source: source.clone(),
            destination,
            old_name,
            new_name,
            readable: true,
            changed,
            issues,
        });
    }

    validate_sources(&mut rows);
    validate_destinations(&mut rows);
    Ok(BatchRenamePlan::from_rows(rows))
}

fn validate_sources(rows: &mut [RenameRow]) {
    let mut first_by_source = HashMap::new();
    for index in 0..rows.len() {
        let source = rows[index].source.clone();
        if let Some(first) = first_by_source.insert(source.clone(), index) {
            let issue = PlanIssue::DuplicateSource {
                path: source.clone(),
            };
            rows[first].issues.push(issue.clone());
            rows[index].issues.push(issue);
        }
    }

    let selected: HashSet<VfsPath> = rows.iter().map(|row| row.source.clone()).collect();
    for index in 0..rows.len() {
        let Some(ancestor) =
            ancestors(&rows[index].source).find(|ancestor| selected.contains(ancestor))
        else {
            continue;
        };
        let ancestor_index = first_by_source.get(&ancestor).copied();
        let issue = PlanIssue::OverlappingSelection {
            ancestor,
            descendant: rows[index].source.clone(),
        };
        rows[index].issues.push(issue.clone());
        if let Some(ancestor_index) = ancestor_index {
            rows[ancestor_index].issues.push(issue);
        }
    }
}

fn validate_destinations(rows: &mut [RenameRow]) {
    let mut source_groups: HashMap<(VfsPath, NameKey), Vec<usize>> = HashMap::new();
    let mut destination_groups: HashMap<(VfsPath, NameKey), Vec<usize>> = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        if !row.readable {
            continue;
        }
        let Some(parent) = row.destination.parent() else {
            continue;
        };
        let source_key = (parent.clone(), NameKey::new(&row.old_name, &row.source));
        source_groups.entry(source_key).or_default().push(index);
        if row.changed {
            destination_groups
                .entry((parent, NameKey::new(&row.new_name, &row.source)))
                .or_default()
                .push(index);
        }
    }

    for indices in destination_groups.values() {
        if indices.len() < 2 {
            continue;
        }
        let Some(first) = indices.first() else {
            continue;
        };
        let issue = PlanIssue::DuplicateTarget {
            sources: indices
                .iter()
                .map(|index| rows[*index].source.clone())
                .collect::<Vec<_>>()
                .into(),
            target: rows[*first].destination.clone(),
        };
        for index in indices {
            rows[*index].issues.push(issue.clone());
        }
    }

    for index in 0..rows.len() {
        if !rows[index].changed || !rows[index].issues.is_empty() {
            continue;
        }
        let Some(parent) = rows[index].destination.parent() else {
            continue;
        };
        let key = (
            parent,
            NameKey::new(&rows[index].new_name, &rows[index].source),
        );
        let Some(candidates) = source_groups.get(&key) else {
            continue;
        };
        let blockers = candidates
            .iter()
            .filter(|candidate| !rows[**candidate].changed)
            .map(|candidate| rows[*candidate].source.clone())
            .collect::<Vec<_>>();
        if !blockers.is_empty() {
            rows[index].issues.push(PlanIssue::UnchangedSourceInWay {
                target: rows[index].destination.clone(),
                blockers,
            });
        }
    }
}

pub(super) fn ancestors(path: &VfsPath) -> impl Iterator<Item = VfsPath> + '_ {
    let mut next = path.parent();
    std::iter::from_fn(move || {
        let current = next.take()?;
        next = current.parent();
        Some(current)
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum NameKey {
    Exact(String),
    CaseInsensitive(String),
}

impl NameKey {
    pub(super) fn new(name: &str, path: &VfsPath) -> Self {
        #[cfg(target_os = "macos")]
        if path.is_local() {
            let normalized = name.nfc().collect::<String>();
            return Self::CaseInsensitive(
                default_case_fold_str(&normalized).nfc().collect::<String>(),
            );
        }
        #[cfg(windows)]
        if path.is_local() {
            return Self::CaseInsensitive(default_case_fold_str(name));
        }
        Self::Exact(name.to_string())
    }
}

fn join_paths(paths: &[VfsPath], limit: usize) -> String {
    paths
        .iter()
        .take(limit)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaseChange, FindReplace, MatchSyntax, RuleTarget, TextAffix};

    fn source(name: &str) -> VfsPath {
        VfsPath::local(format!("/tmp/{name}"))
    }

    fn prefix(prefix: &str) -> RenameRule {
        RenameRule::Prefix(TextAffix {
            value: prefix.into(),
            target: RuleTarget::Stem,
        })
    }

    fn row(old_name: &str, new_name: &str) -> RenameRow {
        let source = source(old_name);
        RenameRow {
            destination: source.parent().unwrap().join(new_name),
            source,
            old_name: old_name.into(),
            new_name: new_name.into(),
            readable: true,
            changed: true,
            issues: Vec::new(),
        }
    }

    #[test]
    fn duplicate_destinations_block_the_whole_plan() {
        let plan = build_plan(
            &[source("a.txt"), source("b.txt")],
            &[RenameRule::FindReplace(FindReplace {
                find: r".*".into(),
                replacement: "same.txt".into(),
                syntax: MatchSyntax::Regex,
                case_sensitive: true,
                target: RuleTarget::FullName,
            })],
        )
        .unwrap();
        assert!(!plan.can_apply());
        assert!(
            plan.issues()
                .any(|issue| matches!(issue, PlanIssue::DuplicateTarget { .. }))
        );
    }

    #[test]
    fn a_cycle_is_a_valid_destination_graph() {
        let mut rows = vec![row("a.txt", "b.txt"), row("b.txt", "a.txt")];
        validate_destinations(&mut rows);
        assert!(
            rows.iter().all(|row| row.issues.is_empty()),
            "{:?}",
            rows.iter()
                .flat_map(|row| row.issues.iter())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unchanged_selected_file_cannot_be_overwritten() {
        let plan = build_plan(
            &[source("a.txt"), source("b.txt")],
            &[RenameRule::FindReplace(FindReplace {
                find: "a".into(),
                replacement: "b".into(),
                syntax: MatchSyntax::Literal,
                case_sensitive: true,
                target: RuleTarget::Stem,
            })],
        )
        .unwrap();
        assert!(
            plan.issues()
                .any(|issue| matches!(issue, PlanIssue::UnchangedSourceInWay { .. })),
            "{:?}",
            plan.issues().collect::<Vec<_>>()
        );
    }

    #[test]
    fn selecting_a_parent_and_child_is_rejected() {
        let parent = VfsPath::local("/tmp/folder");
        let child = parent.join("child.txt");
        let plan = build_plan(&[parent.clone(), child.clone()], &[prefix("x-")]).unwrap();
        assert!(
            plan.issues()
                .any(|issue| matches!(issue, PlanIssue::OverlappingSelection { .. }))
        );
    }

    #[test]
    fn a_large_flat_selection_plans_and_groups_targets_linearly() {
        let sources = (0..200_000)
            .map(|index| source(&format!("item-{index:06}.txt")))
            .collect::<Vec<_>>();
        {
            let unique = build_plan(&sources, &[prefix("renamed-")]).unwrap();
            assert!(unique.can_apply());
        }

        let duplicate = build_plan(
            &sources,
            &[RenameRule::FindReplace(FindReplace {
                find: r".*".into(),
                replacement: "same.txt".into(),
                syntax: MatchSyntax::Regex,
                case_sensitive: true,
                target: RuleTarget::FullName,
            })],
        )
        .unwrap();
        assert_eq!(duplicate.issue_count(), sources.len());
    }

    #[test]
    fn invalid_windows_names_are_rejected() {
        let plan = build_plan(
            &[source("a.txt")],
            &[RenameRule::FindReplace(FindReplace {
                find: "a".into(),
                replacement: "CON".into(),
                syntax: MatchSyntax::Literal,
                case_sensitive: true,
                target: RuleTarget::Stem,
            })],
        )
        .unwrap();
        assert!(
            plan.issues()
                .any(|issue| matches!(issue, PlanIssue::InvalidName { .. }))
        );
    }

    #[test]
    fn every_case_style_keeps_the_extension_out_of_the_stem() {
        for style in [
            crate::CaseStyle::Lower,
            crate::CaseStyle::Upper,
            crate::CaseStyle::Title,
            crate::CaseStyle::Kebab,
            crate::CaseStyle::Snake,
        ] {
            let plan = build_plan(
                &[source("My File.TXT")],
                &[RenameRule::Case(CaseChange {
                    style,
                    target: RuleTarget::Stem,
                })],
            )
            .unwrap();
            assert_eq!(plan.rows()[0].new_name.split('.').next_back(), Some("TXT"));
        }
    }
}
