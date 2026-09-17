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
    /// The preview pane has focus.
    pub const PREVIEW: &str = "Preview";
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
        /// Toggle the preview pane (F-30). Collapses to zero, remembers the
        /// last width.
        TogglePreview,
        /// Move keyboard focus into the preview body.
        FocusPreview,
        /// Return focus to the file pane (also `Esc` inside the preview).
        FocusFilePane,
        /// Preview-local: open the previewed file with its default app.
        PreviewOpen,
        /// Preview-local: reveal the previewed file in Finder / Explorer.
        PreviewReveal,
        /// Preview-local: copy the previewed path as text.
        PreviewCopyPath,
        /// Preview-local: toggle soft wrap for this pane.
        PreviewToggleWrap,
        /// Preview-local: flip a Markdown preview between rendered and source.
        PreviewToggleSource,
        /// Preview-local: zoom the previewed image in, out, or back to fit.
        PreviewZoomIn,
        PreviewZoomOut,
        PreviewZoomReset,
        /// Preview-local: toggle line numbers (persisted).
        PreviewToggleLineNumbers,
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
