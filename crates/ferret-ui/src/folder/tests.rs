use super::*;
use ferret_vfs::{EntryKind, Metadata, SymlinkTarget};

fn entry(name: &str, kind: EntryKind, len: u64, modified: Option<SystemTime>) -> Entry {
    Entry {
        path: VfsPath::local(format!("/x/{name}")),
        name: name.to_string(),
        kind,
        metadata: Some(Metadata {
            len,
            modified,
            // Mirror what LocalFs produces: the leading dot is hidden.
            hidden: name.starts_with('.'),
            ..Default::default()
        }),
        link_target: None,
    }
}

fn t(secs: u64) -> Option<SystemTime> {
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

fn populated() -> Folder {
    let mut f = Folder::new(
        VfsPath::local("/x"),
        SortKey::Name,
        SortOrder::Ascending,
        false,
    );
    f.entries = vec![
        entry("report.pdf", EntryKind::File, 500, t(300)),
        entry("zebra.txt", EntryKind::File, 100, t(200)),
        entry("src", EntryKind::Dir, 0, t(100)),
        entry(".hidden", EntryKind::File, 10, t(400)),
    ];
    // Tests skip the streaming phase; sort as `finish_loading` would.
    f.loading = false;
    f.resort();
    f
}

#[test]
fn hidden_entries_are_filtered_until_shown() {
    let f = populated();
    assert_eq!(f.row_count(), 3);
    assert_eq!(f.entry_at(0).unwrap().name, "src");

    let mut f = populated();
    f.set_show_hidden(true);
    assert_eq!(f.row_count(), 4);
}

#[test]
fn folders_come_first_then_natural_name_order() {
    let mut f = populated();
    f.set_show_hidden(true);
    let names: Vec<_> = (0..f.row_count())
        .map(|r| f.entry_at(r).unwrap().name.clone())
        .collect();
    assert_eq!(names, ["src", ".hidden", "report.pdf", "zebra.txt"]);
}

#[test]
fn size_sort_uses_display_len_and_reverses() {
    let mut f = populated();
    f.set_show_hidden(true);
    f.set_sort(SortKey::Size, SortOrder::Ascending);
    assert_eq!(f.entry_at(0).unwrap().name, "src"); // dirs pinned first
    assert_eq!(
        f.entry_at(1).unwrap().display_len(),
        Some(10),
        "smallest file first"
    );

    f.set_sort(SortKey::Size, SortOrder::Descending);
    assert_eq!(f.entry_at(1).unwrap().display_len(), Some(500));
}

#[test]
fn modified_sort_orders_by_time() {
    let mut f = populated();
    f.set_show_hidden(true);
    f.set_sort(SortKey::Modified, SortOrder::Ascending);
    assert_eq!(f.entry_at(0).unwrap().name, "src");
    assert_eq!(f.entry_at(1).unwrap().name, "zebra.txt");
    assert_eq!(f.entry_at(f.row_count() - 1).unwrap().name, ".hidden");
}

#[test]
fn selection_survives_resorting() {
    let mut f = populated();
    f.set_show_hidden(true);
    // Select "zebra" (last row).
    f.select_only(f.row_count() - 1);
    assert_eq!(f.selected_paths().len(), 1);

    // Flip the sort; zebra moves but stays selected.
    f.set_sort(SortKey::Name, SortOrder::Descending);
    let sel = &f.selected_paths()[0];
    assert!(sel.to_string().ends_with("zebra.txt"));
}

#[test]
fn shift_click_extends_from_anchor() {
    let mut f = populated();
    f.set_show_hidden(true);
    f.select_only(1); // .hidden
    f.extend_select(3); // report.pdf
    assert_eq!(f.selection_count(), 3);
    assert_eq!(f.selection_size(), 10 + 100 + 500);

    // Extend backwards past the anchor collapses onto the new range.
    f.extend_select(2);
    assert_eq!(f.selection_count(), 2);
}

#[test]
fn cmd_click_toggles_without_disturbing_others() {
    let mut f = populated();
    f.select_only(0);
    f.toggle_select(2);
    assert_eq!(f.selection_count(), 2);
    f.toggle_select(2);
    assert_eq!(f.selection_count(), 1);
}

#[test]
fn filter_narrows_and_reports_misses() {
    let mut f = populated();
    f.set_filter(Some("PDF".into()));
    assert_eq!(f.row_count(), 1);
    assert_eq!(f.entry_at(0).unwrap().name, "report.pdf");

    f.set_filter(Some("zzz".into()));
    assert_eq!(f.row_count(), 0);
    // And clearing restores.
    f.set_filter(None);
    assert_eq!(f.row_count(), 3);
}

#[test]
fn shift_arrow_grows_and_shrinks_against_a_fixed_anchor() {
    let mut f = populated();
    f.set_show_hidden(true); // src, .hidden, report.pdf, zebra.txt
    f.select_only(1); // anchor on .hidden

    f.extend_cursor(1);
    f.extend_cursor(1);
    assert_eq!(f.selection_count(), 3, "grew to rows 1..=3");

    // Reversing shrinks the range rather than starting a new one, which
    // is the whole difference from a plain arrow.
    f.extend_cursor(-1);
    assert_eq!(f.selection_count(), 2);

    // Past the anchor it grows the other way.
    f.extend_cursor(-1);
    f.extend_cursor(-1);
    assert_eq!(f.selection_count(), 2, "rows 0..=1");
    assert_eq!(f.cursor_row(), Some(0));
}

/// Expand a folder without touching the executor, by planting its
/// children directly — the model half of what `load_children` does.
fn expand_with(f: &mut Folder, parent: &str, children: Vec<Entry>) {
    let parent = VfsPath::local(parent);
    f.expanded.insert(parent.clone());
    f.children.insert(parent, children);
    f.rebuild_index_and_order();
}

#[test]
fn expanded_children_are_spliced_in_beneath_their_parent() {
    let mut f = populated(); // src, report.pdf, zebra.txt (hidden off)
    assert_eq!(f.row_count(), 3);

    expand_with(
        &mut f,
        "/x/src",
        vec![
            entry("main.rs", EntryKind::File, 10, t(1)),
            entry("lib.rs", EntryKind::File, 20, t(2)),
        ],
    );

    // Children land directly under their parent, not at the end.
    let names: Vec<_> = (0..f.row_count())
        .map(|r| f.entry_at(r).unwrap().name.clone())
        .collect();
    assert_eq!(
        names,
        ["src", "main.rs", "lib.rs", "report.pdf", "zebra.txt"]
    );

    // And they are one level deeper, which is what the indent draws.
    assert_eq!(f.depth_at(0), 0);
    assert_eq!(f.depth_at(1), 1);
    assert_eq!(f.depth_at(3), 0);
}

#[test]
fn only_folders_report_an_expansion_state() {
    let f = populated();
    assert_eq!(f.expansion_at(0), Some(false), "src is a collapsed folder");
    assert_eq!(f.expansion_at(1), None, "report.pdf is a file");
}

#[test]
fn a_child_row_is_selectable_like_any_other() {
    let mut f = populated();
    expand_with(
        &mut f,
        "/x/src",
        vec![entry("main.rs", EntryKind::File, 10, t(1))],
    );

    f.select_only(1); // main.rs
    assert_eq!(f.selection_count(), 1);
    assert!(f.selected_paths()[0].to_string().ends_with("main.rs"));

    // Range selection spans the tree boundary without noticing it.
    f.extend_select(3);
    assert_eq!(f.selection_count(), 3);
}

#[test]
fn collapsing_forgets_the_subtree_it_was_showing() {
    let mut f = populated();
    expand_with(
        &mut f,
        "/x/src",
        vec![entry("main.rs", EntryKind::File, 10, t(1))],
    );
    assert_eq!(f.row_count(), 4);

    // Collapse by hand, mirroring `toggle_expanded`'s model half.
    f.expanded.remove(&VfsPath::local("/x/src"));
    f.forget_subtree(&VfsPath::local("/x/src"));
    f.rebuild_index_and_order();

    assert_eq!(f.row_count(), 3);
    assert!(
        !f.has_entry(&VfsPath::local("/x/src/main.rs")),
        "a collapsed subtree is not still loaded"
    );
}

#[test]
fn the_filter_reaches_into_expanded_children() {
    let mut f = populated();
    expand_with(
        &mut f,
        "/x/src",
        vec![
            entry("main.rs", EntryKind::File, 10, t(1)),
            entry("notes.txt", EntryKind::File, 20, t(2)),
        ],
    );

    f.set_filter(Some("rs".into()));
    let names: Vec<_> = (0..f.row_count())
        .map(|r| f.entry_at(r).unwrap().name.clone())
        .collect();
    // "src" does not itself match, but it is kept as the route to a child
    // that does — a filter that hid it would hide the match with it.
    assert_eq!(names, ["src", "main.rs"], "{names:?}");
}

/// The model half of `start_loading`, without the executor.
fn begin_reload(f: &mut Folder) {
    f.entries.clear();
    f.order.clear();
    f.row_of_path.clear();
    f.top_index.clear();
    f.known_paths.clear();
    f.children.clear();
}

#[test]
fn a_reload_leaves_no_index_pointing_at_the_old_listing() {
    let mut f = populated(); // src, report.pdf, zebra.txt
    expand_with(
        &mut f,
        "/x/src",
        vec![entry("main.rs", EntryKind::File, 10, t(1))],
    );
    assert!(f.has_entry(&VfsPath::local("/x/report.pdf")));

    begin_reload(&mut f);

    // Every derived index goes with the listing it described. A stale
    // `top_index` is the dangerous one: a watcher probe arriving mid-
    // reload would resolve an old path to an index that now points at a
    // different entry, and overwrite that one instead.
    assert!(f.top_index.is_empty(), "top_index must not survive");
    assert!(f.known_paths.is_empty(), "known_paths must not survive");
    assert!(f.children.is_empty(), "children must be re-read");
    assert!(!f.has_entry(&VfsPath::local("/x/report.pdf")));

    // Which folders are open is the user's intent, not derived state, so
    // a refresh must not collapse the tree.
    assert!(f.expanded.contains(&VfsPath::local("/x/src")));
}

#[test]
fn a_probe_after_a_reload_edits_the_right_entry() {
    let mut f = populated();
    begin_reload(&mut f);

    // The new listing happens to put a different file where "src" was.
    f.entries = vec![
        entry("aaa.txt", EntryKind::File, 1, t(1)),
        entry("bbb.txt", EntryKind::File, 2, t(2)),
    ];
    f.rebuild_index_and_order();

    // A probe for a path from the *old* listing must find nothing rather
    // than land on whatever now sits at the old index.
    let changed = f.apply_probes(vec![(
        std::path::PathBuf::from("/x/zebra.txt"),
        Err(ferret_vfs::VfsError::NotFound {
            path: VfsPath::local("/x/zebra.txt"),
        }),
    )]);
    assert!(!changed, "a vanished stale path changes nothing");
    assert_eq!(f.row_count(), 2);
    assert_eq!(f.entry_at(0).unwrap().name, "aaa.txt");
    assert_eq!(f.entry_at(1).unwrap().name, "bbb.txt");
}

#[test]
fn marquee_selection_replaces_rather_than_accumulates() {
    let mut f = populated();
    f.set_show_hidden(true);

    f.select_rows([0, 1]);
    assert_eq!(f.selection_count(), 2);

    // Dragging the band elsewhere selects only what it now covers — a
    // marquee that accumulated would make a downward drag unshrinkable.
    f.select_rows([3]);
    assert_eq!(f.selection_count(), 1);
    assert!(f.is_row_selected(3));

    // An empty band clears.
    f.select_rows([]);
    assert_eq!(f.selection_count(), 0);
}

#[test]
fn extending_in_an_empty_folder_is_harmless() {
    let mut f = Folder::new(
        VfsPath::local("/x"),
        SortKey::Name,
        SortOrder::Ascending,
        false,
    );
    f.loading = false;
    assert_eq!(f.extend_cursor(1), None);
    assert_eq!(f.selection_count(), 0);
}

#[test]
fn the_status_bar_never_counts_what_the_filter_hides() {
    let mut f = populated();
    f.select_all();
    assert_eq!(f.selection_count(), 3);

    // Narrowing to one match leaves the other two selected in the model,
    // but they are neither visible nor actionable, so they are not
    // reported — `selected_paths` is what Trash and Copy operate on.
    f.set_filter(Some("pdf".into()));
    assert_eq!(f.selection_count(), 1);
    assert_eq!(f.selection_size(), 500);
    assert_eq!(f.selected_paths().len(), 1);

    // Clearing the filter brings the rest back.
    f.set_filter(None);
    assert_eq!(f.selection_count(), 3);
}

#[test]
fn type_ahead_finds_wrapping_matches() {
    let mut f = populated();
    f.set_show_hidden(true);
    // Visible order: src, .hidden, report.pdf, zebra.txt.
    assert!(f.select_by_prefix("rep", 0));
    assert_eq!(f.cursor_row(), Some(2));

    // From after the match it wraps around.
    assert!(f.select_by_prefix("z", 3));
    assert_eq!(f.cursor_row(), Some(3));

    assert!(!f.select_by_prefix("qq", 0));
}

#[test]
fn watcher_probes_insert_and_remove_without_relisting() {
    let mut f = populated(); // src, report.pdf, zebra.txt (hidden off)

    // A new file lands: inserted at its sorted spot.
    let new_entry = entry("aaa.txt", EntryKind::File, 7, t(999));
    let changed = f.apply_probes(vec![(
        std::path::PathBuf::from("/x/aaa.txt"),
        Ok(new_entry),
    )]);
    assert!(changed);
    assert_eq!(f.row_count(), 4);
    assert_eq!(f.entry_at(1).unwrap().name, "aaa.txt"); // after src, before report

    // It vanishes: the row goes without a re-list.
    let changed = f.apply_probes(vec![(
        std::path::PathBuf::from("/x/aaa.txt"),
        Err(ferret_vfs::VfsError::NotFound {
            path: VfsPath::local("/x/aaa.txt"),
        }),
    )]);
    assert!(changed);
    assert_eq!(f.row_count(), 3);
}

#[test]
fn selecting_out_of_range_is_harmless() {
    let mut f = populated();
    f.select_only(99);
    assert!(f.selection.is_empty());
    assert!(f.cursor.is_none());
}

#[test]
fn symlinked_directories_sort_as_directories() {
    let mut f = Folder::new(
        VfsPath::local("/x"),
        SortKey::Name,
        SortOrder::Ascending,
        true,
    );
    f.entries = vec![
        entry("b.txt", EntryKind::File, 1, None),
        entry(
            "a-link",
            EntryKind::Symlink {
                target_kind: Some(SymlinkTarget::Dir),
            },
            0,
            None,
        ),
    ];
    f.loading = false;
    f.resort();
    assert_eq!(f.entry_at(0).unwrap().name, "a-link");
}
