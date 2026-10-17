use std::sync::atomic::{AtomicU64, Ordering};

use ferret_vfs::{Vfs, VfsError, VfsPath};

use crate::plan::BatchRenamePlan;
use crate::preflight::preflight_issues;
use crate::{BatchRenameError, RenameResult};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct StagedRename {
    source: VfsPath,
    destination: VfsPath,
    temporary: VfsPath,
    finalized: bool,
}

pub async fn apply(fs: &dyn Vfs, plan: &BatchRenamePlan) -> RenameResult<usize> {
    let changed = plan.changed_len();
    if changed == 0 {
        return Ok(0);
    }

    for row in plan.rows().iter().filter(|row| row.changed()) {
        if !fs.capabilities_for(&row.source).atomic_rename_noreplace {
            return Err(BatchRenameError::Unsupported {
                path: row.source.clone(),
                capability: "an atomic rename that never replaces an existing file",
            });
        }
    }

    let issues = preflight_issues(fs, plan).await?;
    if !issues.is_empty() {
        return Err(BatchRenameError::InvalidPlan { issues });
    }

    let operation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
        ^ u128::from(TEMP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let mut staged = Vec::with_capacity(changed);
    for row in plan.rows().iter().filter(|row| row.changed()) {
        let parent = row
            .source
            .parent()
            .ok_or(BatchRenameError::ApplyFailedRolledBack {
                source: VfsError::Unsupported {
                    capability: "renaming a filesystem root",
                },
            })?;
        let temporary = temporary_path(&parent, operation, staged.len());
        if let Err(source) = fs.rename_noreplace(&row.source, &temporary).await {
            let recovery_paths = rollback(fs, &mut staged).await;
            return Err(failure(source, recovery_paths));
        }
        staged.push(StagedRename {
            source: row.source.clone(),
            destination: row.destination.clone(),
            temporary,
            finalized: false,
        });
    }

    for item in &mut staged {
        if let Err(source) = fs
            .rename_noreplace(&item.temporary, &item.destination)
            .await
        {
            let recovery_paths = rollback(fs, &mut staged).await;
            return Err(failure(source, recovery_paths));
        }
        item.finalized = true;
    }
    Ok(changed)
}

fn temporary_path(parent: &VfsPath, operation: u128, item: usize) -> VfsPath {
    let process = std::process::id();
    parent.join(&format!(".ferret-rename-{process}-{operation}-{item}.tmp"))
}

async fn rollback(fs: &dyn Vfs, staged: &mut [StagedRename]) -> Vec<VfsPath> {
    let mut recovery = Vec::new();
    for item in staged.iter_mut().rev() {
        if item.finalized
            && let Err(_) = fs
                .rename_noreplace(&item.destination, &item.temporary)
                .await
        {
            recovery.push(item.destination.clone());
            continue;
        }
        item.finalized = false;
    }
    for item in staged.iter_mut().rev() {
        if item.finalized {
            continue;
        }
        if fs
            .rename_noreplace(&item.temporary, &item.source)
            .await
            .is_err()
        {
            recovery.push(item.temporary.clone());
        }
    }
    recovery.dedup();
    recovery
}

fn failure(source: VfsError, recovery_paths: Vec<VfsPath>) -> BatchRenameError {
    if recovery_paths.is_empty() {
        BatchRenameError::ApplyFailedRolledBack { source }
    } else {
        BatchRenameError::RecoveryRequired {
            summary: crate::errors::recovery_summary(&recovery_paths),
            recovery_paths,
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ferret_vfs::{
        Capabilities, Entry, EntryKind, ListingDetail, Metadata, ReadDirStream, RemoveMode,
    };
    use std::path::PathBuf;

    use ferret_vfs::LocalFs;

    use super::*;
    use crate::plan::RenameRow;

    #[derive(Default)]
    struct FakeFs {
        entries: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
        fail_next_final: AtomicBool,
        fail_final_at: usize,
        final_renames: AtomicUsize,
        fail_rollback: AtomicBool,
        force_rollback_failure: bool,
    }

    #[async_trait]
    impl ferret_vfs::Vfs for FakeFs {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                symlinks: false,
                create_symlinks: false,
                permissions: true,
                atomic_rename: true,
                rename_noreplace: true,
                atomic_rename_noreplace: true,
                atomic_write: false,
                watch: false,
                metadata_in_listing: true,
                hidden_in_listing: true,
            }
        }

        fn read_dir(&self, dir: &VfsPath, _detail: ListingDetail) -> ReadDirStream {
            let entries = self
                .entries
                .lock()
                .unwrap()
                .keys()
                .filter(|path| path.parent() == dir.as_local())
                .map(|path| {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    Entry::new(VfsPath::local(path.clone()), name, EntryKind::File)
                })
                .collect::<Vec<_>>();
            Box::pin(futures::stream::once(async move { Ok(entries) }))
        }

        async fn metadata(&self, _path: &VfsPath) -> ferret_vfs::Result<Metadata> {
            Ok(Metadata::default())
        }

        async fn canonicalize(&self, path: &VfsPath) -> ferret_vfs::Result<VfsPath> {
            Ok(path.clone())
        }

        async fn read(&self, path: &VfsPath, _budget: usize) -> ferret_vfs::Result<Vec<u8>> {
            self.entries
                .lock()
                .unwrap()
                .get(path.as_local().unwrap())
                .cloned()
                .ok_or_else(|| VfsError::NotFound { path: path.clone() })
        }

        async fn create_dir(&self, _path: &VfsPath) -> ferret_vfs::Result<()> {
            Ok(())
        }

        async fn create_file(&self, path: &VfsPath) -> ferret_vfs::Result<()> {
            self.entries
                .lock()
                .unwrap()
                .insert(path.as_local().unwrap().to_path_buf(), Vec::new());
            Ok(())
        }

        async fn rename(&self, from: &VfsPath, to: &VfsPath) -> ferret_vfs::Result<()> {
            self.rename_noreplace(from, to).await
        }

        async fn rename_noreplace(&self, from: &VfsPath, to: &VfsPath) -> ferret_vfs::Result<()> {
            let from_path = from.as_local().unwrap();
            let to_path = to.as_local().unwrap().to_path_buf();
            let from_is_temporary = from_path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".ferret-rename-"));
            let to_is_temporary = to_path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".ferret-rename-"));
            if from_is_temporary {
                let final_rename = self.final_renames.fetch_add(1, Ordering::Relaxed) + 1;
                let fail = self.fail_next_final.swap(false, Ordering::Relaxed)
                    || (self.fail_final_at != 0 && final_rename == self.fail_final_at);
                if fail {
                    if self.force_rollback_failure {
                        self.fail_rollback.store(true, Ordering::Relaxed);
                    }
                    return Err(VfsError::PermissionDenied { path: to.clone() });
                }
            } else if to_is_temporary && self.fail_rollback.swap(false, Ordering::Relaxed) {
                return Err(VfsError::PermissionDenied { path: to.clone() });
            }
            let mut entries = self.entries.lock().unwrap();
            if entries.contains_key(&to_path) {
                return Err(VfsError::AlreadyExists { path: to.clone() });
            }
            let value = entries
                .remove(from_path)
                .ok_or_else(|| VfsError::NotFound { path: from.clone() })?;
            entries.insert(to_path, value);
            Ok(())
        }

        async fn remove(&self, path: &VfsPath, _mode: RemoveMode) -> ferret_vfs::Result<()> {
            self.entries
                .lock()
                .unwrap()
                .remove(path.as_local().unwrap());
            Ok(())
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ferret-ops-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn row(source: VfsPath, old_name: &str, new_name: &str) -> RenameRow {
        RenameRow::for_test(source, old_name.into(), new_name.into())
    }

    #[test]
    fn a_cycle_applies_without_overwriting_either_file() {
        let root = tempdir("cycle");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        let a = VfsPath::local(root.join("a.txt"));
        let b = VfsPath::local(root.join("b.txt"));
        let plan = BatchRenamePlan::from_rows(vec![
            row(a.clone(), "a.txt", "b.txt"),
            row(b.clone(), "b.txt", "a.txt"),
        ]);

        let fs = LocalFs::new();
        assert_eq!(smol::block_on(apply(&fs, &plan)).unwrap(), 2);
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"b");
        assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"a");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_existing_destination_blocks_the_first_rename() {
        let root = tempdir("existing");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        let a = VfsPath::local(root.join("a.txt"));
        let plan = BatchRenamePlan::from_rows(vec![row(a.clone(), "a.txt", "b.txt")]);

        let fs = LocalFs::new();
        let error = smol::block_on(apply(&fs, &plan)).unwrap_err();
        assert!(matches!(error, BatchRenameError::InvalidPlan { .. }));
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"b");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_failed_finalization_restores_every_staged_path() {
        let root = PathBuf::from("/virtual");
        let a = VfsPath::local(root.join("a.txt"));
        let b = VfsPath::local(root.join("b.txt"));
        let x = VfsPath::local(root.join("x.txt"));
        let y = VfsPath::local(root.join("y.txt"));
        let fs = FakeFs {
            entries: Arc::new(Mutex::new(HashMap::from([
                (root.join("a.txt"), b"a".to_vec()),
                (root.join("b.txt"), b"b".to_vec()),
            ]))),
            fail_next_final: AtomicBool::new(true),
            fail_final_at: 0,
            final_renames: AtomicUsize::new(0),
            fail_rollback: AtomicBool::new(false),
            force_rollback_failure: false,
        };
        let plan = BatchRenamePlan::from_rows(vec![
            row(a.clone(), "a.txt", "x.txt"),
            row(b.clone(), "b.txt", "y.txt"),
        ]);

        let error = smol::block_on(apply(&fs, &plan)).unwrap_err();
        assert!(matches!(
            error,
            BatchRenameError::ApplyFailedRolledBack { .. }
        ));
        let entries = fs.entries.lock().unwrap();
        assert_eq!(entries.get(a.as_local().unwrap()), Some(&b"a".to_vec()));
        assert_eq!(entries.get(b.as_local().unwrap()), Some(&b"b".to_vec()));
        assert!(!entries.contains_key(x.as_local().unwrap()));
        assert!(!entries.contains_key(y.as_local().unwrap()));
    }

    #[test]
    fn a_later_finalization_failure_rolls_back_earlier_finalizations() {
        let root = PathBuf::from("/virtual-later");
        let a = VfsPath::local(root.join("a.txt"));
        let b = VfsPath::local(root.join("b.txt"));
        let x = VfsPath::local(root.join("x.txt"));
        let y = VfsPath::local(root.join("y.txt"));
        let fs = FakeFs {
            entries: Arc::new(Mutex::new(HashMap::from([
                (root.join("a.txt"), b"a".to_vec()),
                (root.join("b.txt"), b"b".to_vec()),
            ]))),
            fail_next_final: AtomicBool::new(false),
            fail_final_at: 2,
            final_renames: AtomicUsize::new(0),
            fail_rollback: AtomicBool::new(false),
            force_rollback_failure: false,
        };
        let plan = BatchRenamePlan::from_rows(vec![
            row(a.clone(), "a.txt", "x.txt"),
            row(b.clone(), "b.txt", "y.txt"),
        ]);

        let error = smol::block_on(apply(&fs, &plan)).unwrap_err();
        assert!(matches!(
            error,
            BatchRenameError::ApplyFailedRolledBack { .. }
        ));
        let entries = fs.entries.lock().unwrap();
        assert_eq!(entries.get(a.as_local().unwrap()), Some(&b"a".to_vec()));
        assert_eq!(entries.get(b.as_local().unwrap()), Some(&b"b".to_vec()));
        assert!(!entries.contains_key(x.as_local().unwrap()));
        assert!(!entries.contains_key(y.as_local().unwrap()));
    }

    #[test]
    fn a_rollback_failure_reports_the_unrestored_path() {
        let root = PathBuf::from("/virtual-recovery");
        let a = VfsPath::local(root.join("a.txt"));
        let b = VfsPath::local(root.join("b.txt"));
        let x = VfsPath::local(root.join("x.txt"));
        let fs = FakeFs {
            entries: Arc::new(Mutex::new(HashMap::from([
                (root.join("a.txt"), b"a".to_vec()),
                (root.join("b.txt"), b"b".to_vec()),
            ]))),
            fail_next_final: AtomicBool::new(false),
            fail_final_at: 2,
            final_renames: AtomicUsize::new(0),
            fail_rollback: AtomicBool::new(false),
            force_rollback_failure: true,
        };
        let plan = BatchRenamePlan::from_rows(vec![
            row(a.clone(), "a.txt", "x.txt"),
            row(b.clone(), "b.txt", "y.txt"),
        ]);

        let error = smol::block_on(apply(&fs, &plan)).unwrap_err();
        let BatchRenameError::RecoveryRequired { recovery_paths, .. } = error else {
            panic!("expected recovery-required failure");
        };
        assert_eq!(recovery_paths.as_slice(), std::slice::from_ref(&x));
        let entries = fs.entries.lock().unwrap();
        assert!(entries.contains_key(x.as_local().unwrap()));
        assert_eq!(entries.get(b.as_local().unwrap()), Some(&b"b".to_vec()));
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_case_only_rename_uses_the_staging_path() {
        let root = tempdir("case");
        std::fs::write(root.join("report.txt"), b"report").unwrap();
        let source = VfsPath::local(root.join("report.txt"));
        let plan = crate::plan_single(source.clone(), "Report.txt").unwrap();

        let fs = LocalFs::new();
        assert_eq!(smol::block_on(apply(&fs, &plan)).unwrap(), 1);
        assert_eq!(std::fs::read(root.join("Report.txt")).unwrap(), b"report");
        let names = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Report.txt"]);
        std::fs::remove_dir_all(root).ok();
    }
}
