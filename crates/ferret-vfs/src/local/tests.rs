use super::*;
use crate::config::FIRST_LISTING_BATCH;
use std::collections::BTreeMap;

fn collect(fs: &LocalFs, dir: &VfsPath) -> BTreeMap<String, Entry> {
    collect_with(fs, dir, ListingDetail::Full)
}

fn collect_with(fs: &LocalFs, dir: &VfsPath, detail: ListingDetail) -> BTreeMap<String, Entry> {
    smol::block_on(async {
        let mut stream = fs.read_dir(dir, detail);
        let mut entries = BTreeMap::new();
        while let Some(item) = stream.next().await {
            if let Ok(batch) = item {
                for entry in batch {
                    entries.insert(entry.name().to_string(), entry);
                }
            }
        }
        entries
    })
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ferret-vfs-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn lists_files_and_directories() {
    let root = tempdir("list");
    fs::write(root.join("a.txt"), b"hello").unwrap();
    fs::create_dir(root.join("sub")).unwrap();

    let entries = collect(&LocalFs::new(), &VfsPath::local(root.clone()));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries["a.txt"].kind, EntryKind::File);
    assert_eq!(entries["sub"].kind, EntryKind::Dir);
    assert!(entries["sub"].is_dir_like());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_directory_yields_one_error_not_a_panic() {
    let missing = VfsPath::local("/definitely/not/here/ferret");
    let results: Vec<_> = smol::block_on(async {
        LocalFs::new()
            .read_dir(&missing, ListingDetail::Full)
            .collect()
            .await
    });
    assert_eq!(results.len(), 1);
    assert!(results[0].as_ref().unwrap_err().is_gone());
}

#[test]
fn read_is_capped_by_the_budget() {
    let root = tempdir("budget");
    let file = root.join("big.bin");
    fs::write(&file, vec![7u8; 10_000]).unwrap();

    let got = smol::block_on(LocalFs::new().read(&VfsPath::local(file), 128)).unwrap();
    assert_eq!(got.len(), 128, "read must stop at the budget");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn trash_is_refused_rather_than_deleting_permanently() {
    let root = tempdir("trash");
    let file = root.join("keep.txt");
    fs::write(&file, b"x").unwrap();

    let err =
        smol::block_on(LocalFs::new().remove(&VfsPath::local(file.clone()), RemoveMode::Trash))
            .unwrap_err();
    assert!(matches!(err, VfsError::Unsupported { .. }));
    assert!(file.exists(), "refusing must not have deleted the file");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn dropping_the_stream_stops_the_walk() {
    let root = tempdir("cancel");
    for i in 0..(LISTING_BATCH_SIZE * (LISTING_BUFFER + 2)) {
        fs::write(root.join(format!("f{i:05}")), b"").unwrap();
    }

    smol::block_on(async {
        let mut stream =
            LocalFs::new().read_dir(&VfsPath::local(root.clone()), ListingDetail::Full);
        assert!(stream.next().await.is_some());
        drop(stream);
    });

    fs::remove_dir_all(&root).ok();
}

#[test]
fn listings_arrive_in_bounded_batches() {
    let root = tempdir("batches");
    let expected = LISTING_BATCH_SIZE * 2 + 1;
    for i in 0..expected {
        fs::write(root.join(format!("f{i:05}")), b"").unwrap();
    }

    let batches: Vec<DirEntryBatch> = smol::block_on(async {
        LocalFs::new()
            .read_dir(&VfsPath::local(root.clone()), ListingDetail::Full)
            .filter_map(|item| async move { item.ok() })
            .collect()
            .await
    });

    assert_eq!(batches[0].len(), FIRST_LISTING_BATCH);
    assert_eq!(batches[1].len(), FIRST_LISTING_BATCH * 2);
    assert!(
        batches.iter().all(|b| b.len() <= LISTING_BATCH_SIZE),
        "no batch may exceed the ceiling: {:?}",
        batches.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert!(
        batches.iter().any(|b| b.len() == LISTING_BATCH_SIZE),
        "a listing this size must reach the steady state"
    );
    assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), expected);
    assert!(batches.iter().all(|batch| !batch.is_empty()));

    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn broken_symlinks_are_listed_and_flagged() {
    let root = tempdir("symlink");
    std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();

    let entries = collect(&LocalFs::new(), &VfsPath::local(root.clone()));
    assert!(
        entries["dangling"].kind.is_broken_symlink(),
        "got {:?}",
        entries["dangling"].kind
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn names_only_skips_the_stat_that_fills_metadata() {
    let root = tempdir("detail");
    fs::write(root.join("a.txt"), b"hello").unwrap();
    fs::create_dir(root.join("sub")).unwrap();

    let fs_ = LocalFs::new();
    let dir = VfsPath::local(root.clone());

    let full = collect_with(&fs_, &dir, ListingDetail::Full);
    assert_eq!(full["a.txt"].metadata.map(|m| m.len), Some(5));
    assert!(full["sub"].metadata.is_some());

    let names = collect_with(&fs_, &dir, ListingDetail::NamesOnly);
    assert_eq!(names.len(), 2, "the same entries are listed");
    assert_eq!(names["a.txt"].kind, EntryKind::File);
    assert_eq!(names["sub"].kind, EntryKind::Dir);
    assert!(names["sub"].is_dir_like());
    assert!(
        names["a.txt"].metadata.is_none(),
        "NamesOnly must not pay for size and dates"
    );
    assert_eq!(names["a.txt"].display_len(), None);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn names_only_still_hides_dotfiles_by_name() {
    let root = tempdir("detail-hidden");
    fs::write(root.join(".secret"), b"").unwrap();
    fs::write(root.join("plain"), b"").unwrap();

    let names = collect_with(
        &LocalFs::new(),
        &VfsPath::local(root.clone()),
        ListingDetail::NamesOnly,
    );
    assert!(names[".secret"].is_hidden());
    assert!(!names["plain"].is_hidden());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn dotfiles_are_hidden() {
    let root = tempdir("hidden");
    fs::write(root.join(".secret"), b"").unwrap();
    fs::write(root.join("plain"), b"").unwrap();

    let entries = collect(&LocalFs::new(), &VfsPath::local(root.clone()));
    assert!(entries[".secret"].is_hidden());
    assert!(!entries["plain"].is_hidden());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn creates_file_and_directory_links_without_overwriting() {
    let root = tempdir("create-links");
    let file = root.join("real.txt");
    let directory = root.join("real-dir");
    fs::write(&file, b"target").unwrap();
    fs::create_dir(&directory).unwrap();

    let fs_ = LocalFs::new();
    let file_link = VfsPath::local(root.join("file-link"));
    let directory_link = VfsPath::local(root.join("directory-link"));
    smol::block_on(async {
        fs_.create_symlink(&file_link, file.to_str().unwrap(), crate::SymlinkKind::File)
            .await
            .unwrap();
        fs_.create_symlink(
            &directory_link,
            directory.to_str().unwrap(),
            crate::SymlinkKind::Directory,
        )
        .await
        .unwrap();
        assert_eq!(fs_.read_link(&file_link).await.unwrap(), file);
        assert_eq!(fs_.read_link(&directory_link).await.unwrap(), directory);
        assert!(matches!(
            fs_.create_symlink(&file_link, file.to_str().unwrap(), crate::SymlinkKind::File,)
                .await,
            Err(VfsError::AlreadyExists { .. })
        ));
    });

    assert_eq!(fs::read(&file).unwrap(), b"target");
    assert!(directory.is_dir());
    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn a_directory_link_can_be_listed_through_its_own_path() {
    let root = tempdir("navigate-link");
    let target = root.join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("inside.txt"), b"x").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let entries = collect(&LocalFs::new(), &VfsPath::local(link.clone()));
    assert_eq!(entries["inside.txt"].name(), "inside.txt");
    fs::remove_dir_all(&root).ok();
}

#[test]
fn copy_copies_files_and_trees_without_following_links() {
    let root = tempdir("copy");
    let src = root.join("src");
    fs::create_dir_all(src.join("nested")).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    fs::write(src.join("nested/b.txt"), b"beta").unwrap();

    let fs_ = LocalFs::new();
    smol::block_on(async {
        fs_.copy(
            &VfsPath::local(src.clone()),
            &VfsPath::local(root.join("dst")),
        )
        .await
        .unwrap();
        assert!(
            fs_.copy(
                &VfsPath::local(src.clone()),
                &VfsPath::local(root.join("dst"))
            )
            .await
            .is_err()
        );
    });

    assert_eq!(fs::read(root.join("dst/a.txt")).unwrap(), b"alpha");
    assert_eq!(fs::read(root.join("dst/nested/b.txt")).unwrap(), b"beta");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn copying_a_file_does_not_overwrite_an_existing_destination() {
    let root = tempdir("copy-file-conflict");
    let source = root.join("source.txt");
    let destination = root.join("destination.txt");
    fs::write(&source, b"new").unwrap();
    fs::write(&destination, b"old").unwrap();

    let error = smol::block_on(
        LocalFs::new().copy(&VfsPath::local(&source), &VfsPath::local(&destination)),
    )
    .unwrap_err();
    assert!(matches!(error, VfsError::AlreadyExists { .. }), "{error:?}");
    assert_eq!(fs::read(&destination).unwrap(), b"old");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn rename_noreplace_refuses_to_overwrite() {
    let root = tempdir("rename-conflict");
    let source = root.join("source.txt");
    let destination = root.join("destination.txt");
    fs::write(&source, b"new").unwrap();
    fs::write(&destination, b"old").unwrap();

    let fs_ = LocalFs::new();
    let error = smol::block_on(
        fs_.rename_noreplace(&VfsPath::local(&source), &VfsPath::local(&destination)),
    )
    .unwrap_err();
    assert!(matches!(error, VfsError::AlreadyExists { .. }), "{error:?}");
    assert_eq!(fs::read(&source).unwrap(), b"new");
    assert_eq!(fs::read(&destination).unwrap(), b"old");

    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn watch_reports_changes() {
    use super::super::watch::WatchEvent;
    let root = tempdir("watch");
    let watch = LocalFs::new().watch(&VfsPath::local(root.clone())).unwrap();

    for attempt in 0..20 {
        let name = format!("x{attempt}");
        fs::write(root.join(&name), b"").unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            while let Ok(event) = watch.events.try_recv() {
                let WatchEvent::Changed(paths) = event else {
                    continue;
                };
                if paths.iter().any(|p| p.ends_with(&name)) {
                    fs::remove_dir_all(&root).ok();
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    fs::remove_dir_all(&root).ok();
    panic!("LocalFs::watch never reported a write");
}
