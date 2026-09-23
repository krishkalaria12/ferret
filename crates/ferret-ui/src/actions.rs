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
    /// The full-window preview overlay has focus. It holds no text fields,
    /// so `space` here closes rather than types.
    pub const OVERLAY: &str = "PreviewOverlay";
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
        /// Preview-local: turn one PDF page back or forward. No-ops off the
        /// document's ends or while a turn is in flight.
        PreviewPdfPrev,
        PreviewPdfNext,
        /// Preview-local: zoom the previewed image in, out, or back to fit.
        PreviewZoomIn,
        PreviewZoomOut,
        PreviewZoomReset,
        /// Preview-local: toggle line numbers (persisted).
        PreviewToggleLineNumbers,
        /// Preview-local: flip a CSV/TSV grid between the inferred header
        /// and the first row as data. Reversible, sample-local.
        PreviewToggleCsvHeader,
        /// Full-window preview overlay (F-75). `space` in the file pane opens
        /// it for the selected file; `space` or `esc` inside it closes it.
        PreviewOverlayToggle,
        /// Preview-local: enter in-place editing (F-36). Only offered for
        /// complete, lossless text on an atomic-save backend.
        PreviewEditBegin,
        /// Preview-local: save the edited buffer, atomically or not at all.
        PreviewEditSave,
        /// Preview-local: throw away the edits and reload the pinned truth.
        PreviewEditRevert,
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
