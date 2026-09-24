use super::*;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ferret-vfs-tree-{tag}-{}-{:?}",
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
fn a_copied_tree_keeps_its_shape_at_depth() {
    let root = tempdir("deep-copy");
    let src = root.join("src");
    let mut here = src.clone();
    for level in 0..12 {
        here = here.join(format!("level-{level}"));
        fs::create_dir_all(&here).unwrap();
        for i in 0..5 {
            fs::write(here.join(format!("f{i}.txt")), format!("{level}:{i}")).unwrap();
        }
    }

    let dst = root.join("dst");
    smol::block_on(LocalFs::new().copy(&VfsPath::local(&src), &VfsPath::local(&dst))).unwrap();

    let mut check = dst.clone();
    for level in 0..12 {
        check = check.join(format!("level-{level}"));
        assert!(check.is_dir(), "{} must exist", check.display());
        for i in 0..5 {
            assert_eq!(
                fs::read_to_string(check.join(format!("f{i}.txt"))).unwrap(),
                format!("{level}:{i}")
            );
        }
    }

    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn deleting_a_tree_unlinks_symlinks_rather_than_following_them() {
    let root = tempdir("delete-links");
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious.txt"), b"do not delete").unwrap();

    let doomed = root.join("doomed");
    fs::create_dir_all(doomed.join("nested")).unwrap();
    fs::write(doomed.join("nested/a.txt"), b"x").unwrap();
    std::os::unix::fs::symlink(&outside, doomed.join("link-to-dir")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("precious.txt"),
        doomed.join("nested/link-to-file"),
    )
    .unwrap();

    smol::block_on(LocalFs::new().remove(&VfsPath::local(&doomed), RemoveMode::Permanent)).unwrap();

    assert!(!doomed.exists(), "the tree is gone");
    assert!(outside.is_dir(), "the link's target directory survives");
    assert_eq!(
        fs::read_to_string(outside.join("precious.txt")).unwrap(),
        "do not delete",
        "nothing outside the tree was followed and deleted"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn deleting_a_wide_directory_removes_every_child() {
    let root = tempdir("delete-wide");
    let doomed = root.join("doomed");
    fs::create_dir(&doomed).unwrap();
    for i in 0..500 {
        fs::write(doomed.join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    fs::create_dir(doomed.join("sub")).unwrap();
    fs::write(doomed.join("sub/inner.txt"), b"x").unwrap();

    smol::block_on(LocalFs::new().remove(&VfsPath::local(&doomed), RemoveMode::Permanent)).unwrap();
    assert!(!doomed.exists());

    fs::remove_dir_all(&root).ok();
}

fn identity_of(path: &VfsPath, bytes: &[u8]) -> FileIdentity {
    let md = fs::metadata(path.as_local().unwrap()).unwrap();
    FileIdentity::new(md.len(), md.modified().ok(), bytes)
}

#[test]
fn an_unedited_save_round_trips_byte_identically() {
    let root = tempdir("atomic-noop");
    let file = root.join("config.conf");
    fs::write(&file, b"key = value\nsecond = 2").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let path = VfsPath::local(&file);
    let before = fs::read(&file).unwrap();
    let expect = identity_of(&path, &before);

    let after = smol::block_on(LocalFs::new().write_atomic(&path, &before, expect)).unwrap();
    assert_eq!(fs::read(&file).unwrap(), before, "bytes untouched");
    assert_eq!(after.len, before.len() as u64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&file).unwrap().mode() & 0o777,
            0o640,
            "permission bits preserved"
        );
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn crlf_without_trailing_newline_survives_a_save() {
    let root = tempdir("atomic-crlf");
    let file = root.join("win.ini");
    let before = b"[section]\r\nkey=value";
    fs::write(&file, before).unwrap();
    let path = VfsPath::local(&file);
    let expect = identity_of(&path, before);

    let changed = b"[section]\r\nkey=other";
    smol::block_on(LocalFs::new().write_atomic(&path, changed, expect)).unwrap();
    assert_eq!(fs::read(&file).unwrap(), changed);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_bom_is_opaque_bytes_to_the_atomic_layer() {
    let root = tempdir("atomic-bom");
    let file = root.join("utf16.txt");
    let mut before = vec![0xFF, 0xFE];
    before.extend_from_slice(&[b'h', 0, b'i', 0]);
    fs::write(&file, &before).unwrap();
    let path = VfsPath::local(&file);
    let expect = identity_of(&path, &before);

    let mut changed = before.clone();
    changed.extend_from_slice(&[b'!', 0]);
    smol::block_on(LocalFs::new().write_atomic(&path, &changed, expect)).unwrap();
    assert_eq!(fs::read(&file).unwrap(), changed);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn an_external_edit_refuses_rather_than_winning() {
    let root = tempdir("atomic-conflict");
    let file = root.join("shared.txt");
    fs::write(&file, b"version one").unwrap();
    let path = VfsPath::local(&file);
    let stale = identity_of(&path, b"version one");

    fs::write(&file, b"version two, longer").unwrap();
    let err = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", stale))
        .expect_err("a changed file must refuse");
    assert!(
        matches!(err, VfsError::Conflict { .. }),
        "refusal, not resolution: {err:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "version two, longer",
        "the refused save changed nothing"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_same_size_same_mtime_edit_still_conflicts() {
    let root = tempdir("atomic-sneaky");
    let file = root.join("sneaky.txt");
    fs::write(&file, b"aaaaaaaa").unwrap();
    let path = VfsPath::local(&file);

    fs::write(&file, b"bbbbbbbb").unwrap();
    let current = fs::read(&file).unwrap();
    let md = fs::metadata(&file).unwrap();
    let mut stale = FileIdentity::new(md.len(), md.modified().ok(), &current);
    stale.content_hash = content_hash(b"aaaaaaaa");

    let err = smol::block_on(LocalFs::new().write_atomic(&path, b"cccccccc", stale))
        .expect_err("same-size edits must conflict");
    assert!(matches!(err, VfsError::Conflict { .. }), "{err:?}");
    assert_eq!(fs::read(&file).unwrap(), b"bbbbbbbb");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn conflict_messages_name_the_file() {
    let err = VfsError::Conflict {
        path: VfsPath::local("/docs/notes.md"),
    };
    let msg = err.to_string();
    assert!(msg.contains("/docs/notes.md"), "{msg}");
    assert!(msg.contains("changed"), "{msg}");
}

#[cfg(unix)]
#[test]
fn multiply_linked_files_are_refused_for_atomic_saves() {
    let root = tempdir("atomic-hard-link");
    let file = root.join("linked.txt");
    let second = root.join("second-name.txt");
    fs::write(&file, b"original").unwrap();
    fs::hard_link(&file, &second).unwrap();
    let path = VfsPath::local(&file);
    let expect = identity_of(&path, b"original");

    let error = smol::block_on(LocalFs::new().write_atomic(&path, b"changed", expect))
        .expect_err("a multiply linked file must refuse");
    assert!(matches!(error, VfsError::Unsupported { .. }), "{error:?}");
    assert_eq!(fs::read(&file).unwrap(), b"original");
    assert_eq!(fs::read(&second).unwrap(), b"original");

    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn symlinks_are_refused_not_followed() {
    let root = tempdir("atomic-link");
    let target = root.join("real.txt");
    fs::write(&target, b"real").unwrap();
    let link = root.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let path = VfsPath::local(&link);
    let expect = FileIdentity::new(4, None, b"real");

    let err = smol::block_on(LocalFs::new().write_atomic(&path, b"swapped", expect))
        .expect_err("a symlink must never be saved through");
    assert!(matches!(err, VfsError::Unsupported { .. }), "{err:?}");
    assert_eq!(fs::read(&target).unwrap(), b"real");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_failed_save_leaves_no_litter() {
    let root = tempdir("atomic-litter");
    let file = root.join("note.txt");
    fs::write(&file, b"original").unwrap();
    let path = VfsPath::local(&file);
    let stale = FileIdentity::new(999, None, b"wrong");

    let _ = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", stale));
    let litter: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ferret-save-")
        })
        .collect();
    assert!(litter.is_empty(), "temp files must not survive: {litter:?}");
    assert_eq!(fs::read(&file).unwrap(), b"original");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_successful_save_leaves_no_litter_either() {
    let root = tempdir("atomic-litter-ok");
    let file = root.join("note.txt");
    fs::write(&file, b"original").unwrap();
    let path = VfsPath::local(&file);
    let expect = identity_of(&path, b"original");

    smol::block_on(LocalFs::new().write_atomic(&path, b"updated", expect)).expect("a clean save");
    let litter: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ferret-save-")
        })
        .collect();
    assert!(litter.is_empty(), "temp files must not survive: {litter:?}");
    assert_eq!(fs::read(&file).unwrap(), b"updated");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn saving_a_directory_is_a_named_refusal() {
    let root = tempdir("atomic-is-dir");
    let path = VfsPath::local(&root);
    let expect = FileIdentity::new(0, None, b"");

    let err = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", expect))
        .expect_err("a directory is not a file");
    assert!(
        matches!(err, VfsError::NotADirectory { .. }),
        "words, not an `open` Io: {err:?}"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn saving_a_read_only_file_refuses_on_every_platform() {
    let root = tempdir("atomic-readonly");
    let file = root.join("locked.txt");
    fs::write(&file, b"original").unwrap();
    let mut permissions = fs::metadata(&file).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&file, permissions).unwrap();
    let path = VfsPath::local(&file);
    let expect = identity_of(&path, b"original");

    let err = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", expect))
        .expect_err("a read-only file must refuse");
    assert!(
        matches!(err, VfsError::PermissionDenied { .. }),
        "same answer on Unix and Windows: {err:?}"
    );
    assert_eq!(fs::read(&file).unwrap(), b"original");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o644));
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&file, permissions).unwrap();
    }
    fs::remove_dir_all(&root).ok();
}
