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
    /// The terminal panel has focus.
    pub const TERMINAL: &str = "Terminal";
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
        ToggleTerminal,
        FocusTerminalOrPane,
        ToggleTerminalSync,
        /// Paste is its own action inside the terminal: the pane's `Paste`
        /// moves files, and encoding the shortcut itself would send the shell
        /// a raw `0x16` instead of the clipboard (§9).
        TerminalPaste,
        NewTab,
        CloseTab,
        FocusPathBar,
        ViewAsList,
        ViewAsColumns,
        ViewAsIcons,
        ToggleTheme,
        TogglePowerSearch,
        /// Open the saved SSH connection editor (F-20).
        NewConnection,
        /// Send the selection to a saved server (F-22). The connection is
        /// chosen from the submenu this opens, not by this action.
        SendTo,
        OpenInTerminal,
        AddToFavorites,
        FocusFilter,
        ClearFilter,
        NewWindow,
        CompletionNext,
        CompletionPrev,
        AcceptCompletion,
        FocusNextRegion,
        FocusPrevRegion,
        /// Open the commit dialog (F-52). A no-op outside a repository, rather
        /// than an error: the shortcut is global and most folders are not
        /// repositories.
        CommitChanges
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

/// Send the current selection to one saved server (F-22).
///
/// Carries the connection's id because a menu built from the saved list needs
/// one action per server, and the alternative — an action per record
/// registered at startup — could not survive the list being edited.
/// `no_json` because this is dispatched from a menu Ferret builds, never bound
/// to a key or written in a keymap file: there is no id a person could type
/// that would be meaningful.
#[derive(Clone, PartialEq, Eq, Default, Debug, gpui::Action)]
#[action(namespace = ferret, no_json)]
pub struct SendToConnection {
    pub id: String,
}
