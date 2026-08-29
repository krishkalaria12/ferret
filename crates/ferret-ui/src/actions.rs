//! Browser actions and their keymap contexts.
//!
//! Actions live in the UI crate because they name UI behaviour; the binary
//! binds keys to them (macOS and Windows equivalents side by side) and the
//! views handle them. Context names must match the `key_context` strings the
//! views install.

use gpui::actions;

/// Focus scopes used with `KeyBinding::new`.
pub mod context {
    /// The file list has focus: selection keys, open, rename, clipboard.
    pub const FILE_PANE: &str = "FilePane";
    /// The path bar is in edit mode.
    pub const PATH_BAR: &str = "PathBar";
    /// The sidebar has focus.
    pub const SIDEBAR: &str = "Sidebar";
    /// `gpui-component`'s table installs this inside the file pane. Ferret
    /// rebinds Tab here so focus can leave the pane instead of walking the
    /// table's columns (§8).
    pub const DATA_TABLE: &str = "DataTable";
}

actions!(
    ferret_browser,
    [
        Back,
        Forward,
        Up,
        Refresh,
        ToggleHidden,
        ToggleSidebar,
        NewTab,
        CloseTab,
        FocusPathBar,
        ViewAsList,
        ViewAsColumns,
        ViewAsIcons,
        ToggleTheme,
        OpenInTerminal,
        AddToFavorites,
        FocusFilter,
        ClearFilter,
        NewWindow,
        CompletionNext,
        CompletionPrev,
        AcceptCompletion,
        FocusNextRegion,
        FocusPrevRegion
    ]
);

actions!(
    ferret_pane,
    [
        Open,
        Rename,
        Trash,
        DeletePermanently,
        Copy,
        Cut,
        Paste,
        SelectAll,
        NewFolder,
        NewFile,
        CancelEdit,
        SortByName,
        SortByModified,
        SortBySize,
        SortAscending,
        SortDescending,
        CursorUp,
        CursorDown,
        CursorLeft,
        CursorRight,
        ExtendUp,
        ExtendDown,
        EnterDirectory,
        ToggleColumnModified,
        ToggleColumnSize
    ]
);
