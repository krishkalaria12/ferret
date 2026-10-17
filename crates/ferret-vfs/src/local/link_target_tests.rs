use super::*;

#[test]
#[cfg(unix)]
fn every_link_carries_its_raw_target() {
    let root = std::env::temp_dir().join(format!(
        "ferret-vfs-links-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("real.txt"), b"x").unwrap();
    std::os::unix::fs::symlink("real.txt", root.join("works")).unwrap();
    std::os::unix::fs::symlink("/nowhere/at/all", root.join("dead")).unwrap();

    let fs_impl = LocalFs::new();
    let entries: Vec<Entry> = smol::block_on(async {
        fs_impl
            .read_dir(&VfsPath::local(root.clone()), ListingDetail::Full)
            .filter_map(|r| async move { r.ok() })
            .flat_map(futures::stream::iter)
            .collect()
            .await
    });
    let by_name = |name: &str| {
        entries
            .iter()
            .find(|e| e.name() == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .clone()
    };

    let dead = by_name("dead");
    assert!(dead.kind.is_broken_symlink());
    assert_eq!(
        dead.link_target_path(),
        Some(std::path::Path::new("/nowhere/at/all"))
    );

    let works = by_name("works");
    assert!(!works.kind.is_broken_symlink());
    assert_eq!(
        works.link_target_path(),
        Some(std::path::Path::new("real.txt"))
    );
    assert_eq!(works.link_target_label(), Some("real.txt"));
    assert!(by_name("real.txt").link_target.is_none());

    fs::remove_dir_all(&root).ok();
}
