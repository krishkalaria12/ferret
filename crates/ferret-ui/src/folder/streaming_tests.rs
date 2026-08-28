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
            ferret_settings::SortKey::Name,
            ferret_settings::SortOrder::Ascending,
            true,
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
