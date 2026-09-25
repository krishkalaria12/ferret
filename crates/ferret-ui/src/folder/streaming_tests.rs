use super::*;
use gpui::AppContext as _;
use std::fs;

#[gpui::test]
async fn streams_a_listing_into_rows(cx: &mut gpui::TestAppContext) {
    let root = std::env::temp_dir().join(format!(
        "ferret-ui-stream-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    for i in 0..50 {
        fs::write(root.join(format!("file-{i:03}.txt")), b"x").unwrap();
    }
    fs::create_dir(root.join("subdir")).unwrap();

    cx.update(|cx| {
        cx.set_global(crate::state::load_state());
    });

    let folder = cx.new(|_| {
        Folder::new(
            VfsPath::local(root.clone()),
            SortRules::by_name(),
            true,
            ListingProfile::BROWSING,
        )
    });
    folder.update(cx, |f, cx| f.start_loading(cx));

    // The listing producer runs on its own OS thread (smol::unblock), so
    // this is not a fully deterministic test: allow real threads, then
    // wait for the walk to land.
    cx.executor().allow_parking();
    let mut rows = 0;
    let mut loading = true;
    for _ in 0..600 {
        cx.run_until_parked();
        (rows, loading) = folder.read_with(cx, |f, _| (f.row_count(), f.is_loading()));
        if rows == 51 && !loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(rows, 51, "all entries must land");
    assert!(!loading, "loading must finish");

    // Dirs first, then natural name order.
    let first = folder.read_with(cx, |f, _| f.entry_at(0).unwrap().name().to_string());
    assert_eq!(first, "subdir");

    fs::remove_dir_all(&root).ok();
}

#[gpui::test]
async fn applies_renames_in_place_and_preserves_selection(cx: &mut gpui::TestAppContext) {
    let root = std::env::temp_dir().join(format!(
        "ferret-ui-rename-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join("b.txt"), b"b").unwrap();

    cx.update(|cx| {
        cx.set_global(crate::state::load_state());
    });
    let folder = cx.new(|_| {
        Folder::new(
            VfsPath::local(root.clone()),
            SortRules::by_name(),
            false,
            ListingProfile::BROWSING,
        )
    });
    folder.update(cx, |folder, cx| folder.start_loading(cx));
    cx.executor().allow_parking();
    for _ in 0..600 {
        cx.run_until_parked();
        if folder.read_with(cx, |folder, _| {
            !folder.is_loading() && folder.row_count() == 2
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    folder.update(cx, |folder, cx| {
        folder.select_rows([0, 1]);
        let sources = folder.selected_paths();
        let renames = sources
            .iter()
            .map(|source| {
                let name = if source.file_name() == Some("a.txt") {
                    "z.txt"
                } else {
                    "a.txt"
                };
                (source.clone(), source.parent().unwrap().join(name))
            })
            .collect::<Vec<_>>();
        assert!(folder.apply_renames(&renames, cx));
    });

    let names = folder.read_with(cx, |folder, _| row_names_for_test(folder));
    assert_eq!(names, ["a.txt", "z.txt"]);
    let selected = folder.read_with(cx, |folder, _| folder.selected_paths());
    assert_eq!(
        selected,
        [
            VfsPath::local(root.join("a.txt")),
            VfsPath::local(root.join("z.txt"))
        ]
    );

    folder.update(cx, |folder, cx| {
        let sources = folder.selected_paths();
        let renames = sources
            .iter()
            .map(|source| {
                let name = if source.file_name() == Some("a.txt") {
                    "z.txt"
                } else {
                    "a.txt"
                };
                (source.clone(), source.parent().unwrap().join(name))
            })
            .collect::<Vec<_>>();
        assert!(folder.apply_renames(&renames, cx));
        assert_eq!(
            folder.by_name.get("a.txt").copied(),
            folder.id_of(&VfsPath::local(root.join("a.txt")))
        );
        assert_eq!(
            folder.by_name.get("z.txt").copied(),
            folder.id_of(&VfsPath::local(root.join("z.txt")))
        );
    });

    fs::remove_dir_all(&root).ok();
}

fn row_names_for_test(folder: &Folder) -> Vec<String> {
    (0..folder.row_count())
        .map(|row| folder.entry_at(row).unwrap().name().to_string())
        .collect()
}

/// An ancestor column in column view must not attach a watcher.
///
/// This is the visible half of A-14: entering column view in a deep path
/// built three real `Folder`s, and each one attached an OS watcher, started a
/// full metadata backfill and resolved platform icons — for a column that
/// draws an icon, a name and a chevron.
#[gpui::test]
async fn a_navigating_listing_attaches_no_watcher(cx: &mut gpui::TestAppContext) {
    let root = std::env::temp_dir().join(format!(
        "ferret-ui-profile-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"x").unwrap();

    cx.update(|cx| {
        cx.set_global(crate::state::load_state());
    });

    let load = |profile, cx: &mut gpui::TestAppContext| {
        let folder = cx.new(|_| {
            Folder::new(
                VfsPath::local(root.clone()),
                SortRules::by_name(),
                true,
                profile,
            )
        });
        folder.update(cx, |f, cx| f.start_loading(cx));
        folder
    };

    let navigating = load(ListingProfile::NAVIGATING, cx);
    let browsing = load(ListingProfile::BROWSING, cx);

    cx.executor().allow_parking();
    for _ in 0..600 {
        cx.run_until_parked();
        if !browsing.read_with(cx, |f, _| f.is_loading())
            && !navigating.read_with(cx, |f, _| f.is_loading())
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    // Both listed the same directory...
    assert_eq!(navigating.read_with(cx, |f, _| f.row_count()), 1);
    assert_eq!(browsing.read_with(cx, |f, _| f.row_count()), 1);
    // ...and only the one being worked in is holding it open.
    assert!(
        navigating.read_with(cx, |f, _| f.watch.is_none()),
        "a navigating listing must not subscribe to the watcher registry"
    );
    assert!(
        browsing.read_with(cx, |f, _| f.watch.is_some()),
        "a browsed listing still must"
    );

    fs::remove_dir_all(&root).ok();
}

/// A refresh must not lose a selection that lives inside an expanded folder.
///
/// The top-level walk cannot resolve it — the row is in a child listing that
/// is re-read afterwards — so the carried state has to outlive
/// `finish_loading` and be settled only once those listings land.
#[gpui::test]
async fn a_reload_restores_state_carried_inside_expanded_children(cx: &mut gpui::TestAppContext) {
    let root = std::env::temp_dir().join(format!(
        "ferret-ui-carry-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Two levels, so the reload has to re-open a folder it only learned was
    // open while restoring the level above it.
    fs::create_dir_all(root.join("sub/deep")).unwrap();
    fs::write(root.join("top.txt"), b"x").unwrap();
    fs::write(root.join("sub/nested.txt"), b"x").unwrap();
    fs::write(root.join("sub/deep/buried.txt"), b"x").unwrap();

    cx.update(|cx| cx.set_global(crate::state::load_state()));
    let folder = cx.new(|_| {
        Folder::new(
            VfsPath::local(root.clone()),
            SortRules::by_name(),
            true,
            ListingProfile::BROWSING,
        )
    });
    folder.update(cx, |f, cx| f.start_loading(cx));
    cx.executor().allow_parking();

    let settle = |cx: &mut gpui::TestAppContext, want: usize| {
        for _ in 0..600 {
            cx.run_until_parked();
            let ready = folder.read_with(cx, |f, _| !f.is_loading() && f.row_count() == want);
            if ready {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = folder.read_with(cx, |f, _| f.row_count());
        panic!("expected {want} rows, settled at {rows}");
    };

    settle(cx, 2); // sub, top.txt

    // Open both levels: sub, then deep.
    folder.update(cx, |f, cx| assert!(f.toggle_expanded(0, cx)));
    settle(cx, 4); // sub, deep, nested.txt, top.txt
    folder.update(cx, |f, cx| assert!(f.toggle_expanded(1, cx)));
    settle(cx, 5); // sub, deep, buried.txt, nested.txt, top.txt

    // Select the row two levels down — the one nothing in the top-level
    // walk can ever resolve.
    let buried = VfsPath::local(root.join("sub/deep/buried.txt"));
    let row = folder.read_with(cx, |f, _| f.row_of(&buried)).unwrap();
    folder.update(cx, |f, _| f.select_only(row));
    assert_eq!(folder.read_with(cx, |f, _| f.selection_count()), 1);

    folder.update(cx, |f, cx| f.reload_preserving_state(cx));
    settle(cx, 5);

    // Both folders are open again...
    assert_eq!(
        folder.read_with(cx, |f, _| f.row_count()),
        5,
        "both expanded folders must be re-read"
    );
    // ...and the selection and cursor came back with them, on a row that
    // only exists because the child listings were re-read.
    assert_eq!(
        folder.read_with(cx, |f, _| f.selection_count()),
        1,
        "a selection inside an expanded folder must survive a refresh"
    );
    assert_eq!(
        folder.read_with(cx, |f, _| f.cursor_path()),
        Some(buried.clone()),
        "and so must the cursor"
    );
    assert!(
        folder.read_with(cx, |f, _| f.row_of(&buried).is_some()),
        "the carried row must be back on screen"
    );
    // Nothing is still waiting, so the carried state is released rather than
    // pinned for the life of the listing.
    assert!(folder.read_with(cx, |f, _| f.carried.is_empty()));
    assert_eq!(folder.read_with(cx, |f, _| f.pending_child_loads), 0);

    fs::remove_dir_all(&root).ok();
}
