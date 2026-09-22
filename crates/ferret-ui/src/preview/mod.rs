//! The preview pane: whatever is selected, rendered in a persistent
//! right-hand column (`docs/preview-plan.html`, F-30).
//!
//! Two rows: a header identical for every file type, and a body that is
//! entirely the provider's. The pane is mounted once and swaps its
//! [`ferret_preview::Preview`]; the editor entity is reused across selections
//! rather than rebuilt.
//!
//! The layering rule decides the shape: classification, budgets, decoding
//! and parsing live in `ferret-preview` (GPUI-free, tested without a
//! window). This module owns *how to draw it*: one background pipeline with
//! one generation, debounced and cancel-by-drop, and one render arm per
//! state. `Render` only reads.
//!
//! Sibling files own one concern each — `grammars.rs` the extended
//! tree-sitter registrations, `edit.rs` F-36, `grid.rs` the CSV table,
//! `pdf.rs` page navigation, `overlay.rs` F-75 — so a `gpui-component`
//! bump touches one directory (PV-R04).

mod edit;
mod grammars;
mod grid;
mod header;
mod overlay;
mod pdf;
mod render;

pub(crate) use overlay::{OverlaySnapshot, PreviewOverlay, SnapshotBody, overlay_geometry};

use std::time::Instant;

use std::path::PathBuf;

use crate::browser::Browser;
use edit::{EditOriginal, EditSession};
use ferret_preview::{
    Bitmap, DecodedImage, Frame, Pages, ParsedTable, PdfUnavailable, Preview, RenderedPage,
    Truncation,
};
use ferret_vfs::{Entry, EntryKind, FileIdentity, LocalFs, Vfs as _, VfsError, VfsPath};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, Subscription, Task, Window,
};
use gpui_component::WindowExt as _;
use gpui_component::input::{Editor, EditorState};
use gpui_component::text::{TextView, TextViewState};
use grid::CsvGrid;
use smol_str::SmolStr;

use crate::format::{format_date, format_size};
use crate::state::FerretState;

/// What the browser hands the pane when the selection settles.
///
/// Deliberately small and pre-counted: building it is O(the selection), and
/// a marquee drag changes the selection on every mouse-move — so anything
/// unbounded (kind breakdowns past a cap, directory totals) is `None` here
/// and resolved by the debounced background task instead.
pub struct PreviewInput {
    pub dir: VfsPath,
    /// Rows in the browsed directory, for the [`PaneState::Empty`] line.
    pub row_count: usize,
    /// Total bytes of the browsed directory. `Some` only when freshly
    /// computed (listing finished, or the selection just emptied) — `None`
    /// keeps the pane's cache, so an arrow-key sweep never rescans.
    pub dir_bytes: Option<u64>,
    pub selected: SelectedInput,
}

/// The selection in the shape the pane consumes it.
pub enum SelectedInput {
    None,
    Single(Entry),
    Many {
        count: usize,
        bytes: u64,
        /// "4 folders · 8 images", or `None` past the breakdown cap.
        breakdown: Option<String>,
    },
}

/// Cap for the synchronous kind breakdown in a multi-selection.
///
/// Past this the pane shows the count and combined size only. Counting kinds
/// is O(the selection) on the window thread, and a full-directory marquee
/// must not cost that per mouse-move.
pub const BREAKDOWN_CAP: usize = 512;

/// Every state the pane can be in (`docs/preview-plan.html` §4).
///
/// The editor entity holds the text; states that show text carry only the
/// header strings, the footer and the editor options — never a second copy
/// of the buffer.
enum PaneState {
    Empty {
        name: String,
        rows: usize,
        bytes: Option<u64>,
    },
    Multiple {
        count: usize,
        bytes: u64,
        breakdown: Option<String>,
    },
    Directory {
        entry: Entry,
        detail: String,
        count: Option<u64>,
    },
    Loading {
        name: String,
        detail: String,
        skeleton: bool,
        remote: bool,
    },
    Ready {
        entry: Entry,
        detail: String,
        language: SmolStr,
        wrap: bool,
        footer: Option<String>,
        /// The loaded bytes plus their identity, for in-place editing (F-36).
        /// `Some` only for complete, lossless text on a backend with the
        /// atomic-save capability — the Edit button reads this, never a
        /// second guess about the file.
        original: Option<EditOriginal>,
    },
    /// Rendered Markdown. The document view holds the sanitized source;
    /// the editor holds the original for the source toggle.
    ReadyMd {
        entry: Entry,
        detail: String,
        footer: Option<String>,
        show_source: bool,
        base: Option<PathBuf>,
        remote: bool,
        /// The sanitized source the document view shows, retained so the
        /// F-75 overlay reuses it without re-sanitizing the file.
        sanitized: String,
        original: Option<EditOriginal>,
    },
    Unsupported {
        entry: Entry,
        detail: String,
        message: String,
    },
    /// A decoded image. The `RenderImage` is built once, at apply time:
    /// rebuilding it per frame would re-upload the texture every frame.
    /// `zoom` of 0 means fit-to-pane; anything else is a fixed scale.
    Image {
        entry: Entry,
        detail: String,
        render_image: std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        zoom: f32,
        footer: Option<String>,
    },
    /// A CSV/TSV sample in the virtualised grid (F-35). The delegate holds
    /// the rows as shared strings; `table` is retained so the F-75 overlay
    /// can build its own grid without re-reading the file.
    Table {
        entry: Entry,
        detail: String,
        grid: CsvGrid,
        table: std::sync::Arc<ParsedTable>,
        footer: Option<String>,
    },
    /// A PDF page through the bundled backend (F-34). The bitmap is built
    /// once per page turn at apply time; `pdf` is retained so navigation
    /// re-renders from memory. `pdf_gen` serialises page requests the way
    /// the pane generation serialises selections — a turning page never
    /// paints over a newer selection, and a new selection drops the turn.
    Pdf {
        entry: Entry,
        detail: String,
        render_image: std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        zoom: f32,
        pdf: std::sync::Arc<Vec<u8>>,
        pages: Pages,
        pdf_gen: u64,
        loading_page: bool,
        page_error: Option<String>,
    },
    Error {
        name: String,
        message: String,
    },
}

impl PaneState {
    /// The entry the header buttons act on, when there is one.
    fn entry(&self) -> Option<&Entry> {
        match self {
            PaneState::Directory { entry, .. }
            | PaneState::Ready { entry, .. }
            | PaneState::ReadyMd { entry, .. }
            | PaneState::Image { entry, .. }
            | PaneState::Table { entry, .. }
            | PaneState::Pdf { entry, .. }
            | PaneState::Unsupported { entry, .. } => Some(entry),
            _ => None,
        }
    }

    /// The edit snapshot, when the current body is saveable text.
    fn original(&self) -> Option<&EditOriginal> {
        match self {
            PaneState::Ready { original, .. } | PaneState::ReadyMd { original, .. } => {
                original.as_ref()
            }
            _ => None,
        }
    }

    /// The edit snapshot, mutably — for re-pinning after a save.
    fn original_mut(&mut self) -> Option<&mut EditOriginal> {
        match self {
            PaneState::Ready { original, .. } | PaneState::ReadyMd { original, .. } => {
                original.as_mut()
            }
            _ => None,
        }
    }
}

pub struct PreviewPane {
    editor: Entity<EditorState>,
    /// One retained document view, reused across selections (F-31).
    docview: Entity<TextViewState>,
    /// Navigation lives in the browser: only it owns history and tabs.
    /// Stored weakly so link clicks can reach it with a window in hand.
    browser: gpui::WeakEntity<Browser>,
    focus: FocusHandle,
    /// The window, for background tasks to re-enter with a `Window` when
    /// the bytes land (the editor's `set_value` needs one).
    window: gpui::AnyWindowHandle,
    state: PaneState,
    /// The in-progress edit session (F-36), if the previewed text is being
    /// edited. While this is `Some`, selection changes are blocked until the
    /// user answers Save / Discard / Cancel — a preview that silently
    /// discards an edit on arrow-down is the data-loss failure in miniature.
    editing: Option<EditSession>,
    /// Set around programmatic `set_value` calls so the dirty observer can
    /// tell a load from a keystroke.
    suppress_dirty: bool,
    /// Bumps per selection; results carry theirs and stale ones are
    /// discarded. One generation, debounced, cancel-by-drop (PV-D11).
    generation: u64,
    loading_since: Option<Instant>,
    tasks: Vec<Task<()>>,
    /// Held so the dirty observer below stays subscribed for the pane's
    /// life. Dropping a `Subscription` unsubscribes, so this is never read —
    /// only kept.
    _subscriptions: Vec<Subscription>,
    /// Cached browsed-directory totals for the [`PaneState::Empty`] line.
    empty_cache: Option<(VfsPath, usize, Option<u64>)>,
    /// Session word-wrap override from the overflow menu. `None` means the
    /// per-kind default: off for code, on for plain text and logs.
    wrap_override: Option<bool>,
}

impl PreviewPane {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        browser: gpui::WeakEntity<Browser>,
    ) -> Self {
        // Deterministic, duplicate-checked, once per process — before any
        // preview, so the first file of a type is already highlighted.
        grammars::ensure_registered();
        let line_numbers = cx.global::<FerretState>().settings.preview.line_numbers;
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("plain")
                .line_number(line_numbers)
                .soft_wrap(true)
        });
        let docview = cx.new(|cx| TextViewState::markdown("", cx));
        let mut pane = Self {
            editor: editor.clone(),
            docview,
            browser,
            focus: cx.focus_handle(),
            window: window.window_handle(),
            state: PaneState::Empty {
                name: String::new(),
                rows: 0,
                bytes: None,
            },
            editing: None,
            suppress_dirty: false,
            generation: 0,
            loading_since: None,
            tasks: Vec::new(),
            empty_cache: None,
            wrap_override: None,
            _subscriptions: Vec::new(),
        };
        // Dirty tracking without per-frame work: the editor notifies on
        // every change, and this fires once per change — never per frame —
        // flipping one bool. Programmatic loads set `suppress_dirty` around
        // `set_value` (which also notifies) so a new file never looks dirty.
        pane._subscriptions.push(cx.observe(&editor, |this, _, cx| {
            this.on_editor_changed(cx);
        }));
        pane
    }

    /// The editor's focus, for the focus-preview shortcut.
    ///
    /// The document view exposes no programmatic focus, so a rendered
    /// Markdown preview focuses the pane itself: Esc still returns to the
    /// list, and keyboard scrolling inside the document is a follow-up.
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.state {
            PaneState::ReadyMd { show_source, .. } if !show_source => self.focus.clone(),
            _ => self.editor.read(cx).focus_handle(cx).clone(),
        }
    }

    /// Flip between rendered Markdown and its source.
    pub(crate) fn toggle_source(&mut self, cx: &mut Context<Self>) {
        if let PaneState::ReadyMd { show_source, .. } = &mut self.state {
            *show_source = !*show_source;
            cx.notify();
        }
    }

    /// Whether the source toggle is offered right now.
    fn showing_markdown(&self) -> Option<bool> {
        match &self.state {
            PaneState::ReadyMd { show_source, .. } => Some(*show_source),
            _ => None,
        }
    }

    /// Zoom the previewed image. Either direction leaves fit-to-pane for
    /// actual size first, then steps ×1.25 to 8× (in) or ÷1.25 to ⅛×
    /// (out). Resets on selection change: zoom is per-preview, never
    /// carried across files.
    pub(crate) fn zoom_image(&mut self, direction: i32, cx: &mut Context<Self>) {
        if let PaneState::Image { zoom, .. } | PaneState::Pdf { zoom, .. } = &mut self.state {
            if *zoom <= 0.0 {
                *zoom = 1.0;
            } else if direction > 0 {
                *zoom = (*zoom * 1.25).min(8.0);
            } else {
                *zoom = (*zoom / 1.25).max(0.125);
            }
            cx.notify();
        }
    }

    /// Back to fit-to-pane.
    pub(crate) fn zoom_image_reset(&mut self, cx: &mut Context<Self>) {
        if let PaneState::Image { zoom, .. } | PaneState::Pdf { zoom, .. } = &mut self.state {
            *zoom = 0.0;
            cx.notify();
        }
    }

    /// Session word-wrap override from the overflow menu.
    pub(crate) fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.effective_wrap();
        self.wrap_override = Some(next);
        let editor = self.editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_soft_wrap(next, window, cx);
        });
    }

    fn effective_wrap(&self) -> bool {
        if let Some(wrap) = self.wrap_override {
            return wrap;
        }
        matches!(&self.state, PaneState::Ready { wrap, .. } if *wrap)
    }

    /// Push a selection into the pane.
    ///
    /// Cheap states (`Empty`, `Multiple`) apply synchronously. Anything that
    /// reads does so after [`ferret_preview::config::PREVIEW_DEBOUNCE`], on a
    /// background task, guarded by the generation — so an uninterrupted
    /// key-repeat sweep settles on one request. Replacing `tasks` drops the
    /// previous requests, which is what cancels them.
    pub fn set_input(&mut self, input: PreviewInput, cx: &mut Context<Self>) {
        // An edit session pins its file: re-selecting the same path (a
        // watcher refresh, a cancelled guard, a tab switch back) keeps the
        // buffer, the cursor and the dirty flag exactly where they are.
        // Anything for a *different* path while dirty never reaches here —
        // the browser's guard holds it for Save / Discard / Cancel first.
        if let SelectedInput::Single(entry) = &input.selected
            && self.editing_path().is_some_and(|path| path == entry.path)
        {
            cx.notify();
            return;
        }
        // A load replaces the buffer, which ends any clean edit session
        // still standing. A *dirty* session never reaches here: the
        // browser's guard holds different-path inputs for Save / Discard /
        // Cancel first, so clearing here cannot lose an edit.
        self.editing = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.tasks.clear();
        self.loading_since = None;

        match input.selected {
            SelectedInput::None => {
                if let Some(bytes) = input.dir_bytes {
                    self.empty_cache = Some((input.dir.clone(), input.row_count, Some(bytes)));
                }
                let (rows, bytes) = match &self.empty_cache {
                    Some((dir, rows, bytes)) if *dir == input.dir => (*rows, *bytes),
                    _ => (input.row_count, None),
                };
                self.state = PaneState::Empty {
                    name: input.dir.file_name().unwrap_or("/").to_string(),
                    rows,
                    bytes,
                };
                cx.notify();
            }
            SelectedInput::Many {
                count,
                bytes,
                breakdown,
            } => {
                self.state = PaneState::Multiple {
                    count,
                    bytes,
                    breakdown,
                };
                cx.notify();
            }
            SelectedInput::Single(entry) => {
                if entry.is_dir_like() {
                    self.preview_directory(entry, generation, cx);
                } else {
                    self.preview_file(entry, generation, cx);
                }
            }
        }
    }

    /// Drop interest in any in-flight request.
    ///
    /// Closing the pane cancels interest; the next `set_input` starts a new
    /// generation, so a late result can never paint over newer state.
    pub(crate) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.tasks.clear();
        self.loading_since = None;
    }

    /// Whether fresh directory totals are worth computing.
    ///
    /// The totals are O(the directory), so the browser asks before paying:
    /// only when the cache misses this directory. A finished load forces a
    /// recompute from the caller's side instead.
    pub fn wants_totals(&self, dir: &VfsPath) -> bool {
        !matches!(&self.empty_cache, Some((cached, _, _)) if cached == dir)
    }

    /// The path the pane is showing, for Copy path and the header buttons.
    pub(crate) fn previewed_path(&self) -> Option<&VfsPath> {
        self.state.entry().map(|entry| &entry.path)
    }

    /// Snapshot the computed value for the F-75 overlay: shared bitmaps by
    /// `Arc`, text by one bounded clone — never a re-read.
    ///
    /// `None` when the pane holds nothing reusable (empty, loading,
    /// directory, message states): the overlay runs the pipeline on demand
    /// instead. Editing included: the snapshot reads the live buffer, so
    /// the overlay shows what is on screen, saved or not.
    pub(crate) fn overlay_snapshot(&self, cx: &App) -> Option<OverlaySnapshot> {
        let entry = self.state.entry()?.clone();
        let title = Self::display_name(entry.name());
        let body = match &self.state {
            PaneState::Ready {
                language,
                wrap,
                footer,
                ..
            } => SnapshotBody::Text {
                text: self.editor.read(cx).value().to_string(),
                language: language.clone(),
                wrap: *wrap,
                footer: footer.clone(),
            },
            PaneState::ReadyMd {
                footer,
                base,
                remote,
                sanitized,
                ..
            } => SnapshotBody::Markdown {
                sanitized: sanitized.clone(),
                base: base.clone(),
                remote: *remote,
                footer: footer.clone(),
            },
            PaneState::Image {
                render_image,
                bitmap_size,
                footer,
                ..
            } => SnapshotBody::Image {
                render_image: render_image.clone(),
                bitmap_size: *bitmap_size,
                footer: footer.clone(),
            },
            PaneState::Table { table, footer, .. } => SnapshotBody::Table {
                table: table.clone(),
                footer: footer.clone(),
            },
            PaneState::Pdf {
                render_image,
                bitmap_size,
                pdf,
                pages,
                ..
            } => SnapshotBody::Pdf {
                render_image: render_image.clone(),
                bitmap_size: *bitmap_size,
                pdf: pdf.clone(),
                pages: *pages,
            },
            _ => return None,
        };
        let detail = match &self.state {
            PaneState::Ready { detail, .. }
            | PaneState::ReadyMd { detail, .. }
            | PaneState::Image { detail, .. }
            | PaneState::Table { detail, .. }
            | PaneState::Pdf { detail, .. } => detail.clone(),
            _ => return None,
        };
        Some(OverlaySnapshot {
            path: entry.path.clone(),
            title,
            detail,
            body,
        })
    }

    /// Re-apply the persisted line-number setting to the live editor.
    pub fn set_line_numbers(
        &mut self,
        line_numbers: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.set_line_number(line_numbers, window, cx);
        });
    }

    fn preview_directory(&mut self, entry: Entry, generation: u64, cx: &mut Context<Self>) {
        let detail = directory_detail(&entry);
        let path = entry.path.clone();
        self.state = PaneState::Directory {
            entry,
            detail,
            count: None,
        };
        cx.notify();

        // The count is a walk and must be asked for — but asking is this
        // pane's job, debounced like everything else, never automatic in
        // `Render` and never on the window thread.
        let fs = cx.global::<FerretState>().fs;
        self.tasks.push(cx.spawn(async move |pane, cx| {
            cx.background_executor()
                .timer(ferret_preview::config::PREVIEW_DEBOUNCE)
                .await;
            let count = count_entries(&fs, &path).await;
            let _ = pane.update(cx, |pane, cx| {
                if pane.generation != generation {
                    return;
                }
                if let PaneState::Directory { count: slot, .. } = &mut pane.state {
                    *slot = Some(count);
                    cx.notify();
                }
            });
        }));
    }

    fn preview_file(&mut self, entry: Entry, generation: u64, cx: &mut Context<Self>) {
        // A broken symlink is a Gone-adjacent state with the dangling target
        // named, not a silent empty pane — and no read is attempted.
        if entry.kind.is_broken_symlink() {
            let target = entry
                .link_target
                .as_deref()
                .map(|target| target.to_string_lossy().into_owned())
                .unwrap_or_else(|| "(unknown target)".to_string());
            self.state = PaneState::Error {
                name: entry.name().to_string(),
                message: format!(
                    "Ferret can't preview {} — it points at {target}, which no longer exists.",
                    entry.name()
                ),
            };
            cx.notify();
            return;
        }
        // Sockets, FIFOs, devices: listed, never opened.
        if matches!(entry.kind, EntryKind::Other) {
            self.state = PaneState::Error {
                name: entry.name().to_string(),
                message: format!(
                    "Ferret can't preview {} — it isn't a regular file.",
                    entry.name()
                ),
            };
            cx.notify();
            return;
        }

        let remote = !entry.path.is_local();
        self.state = PaneState::Loading {
            name: entry.name().to_string(),
            detail: entry_detail_no_kind(&entry),
            skeleton: false,
            remote,
        };
        self.loading_since = Some(Instant::now());
        cx.notify();

        let fs = cx.global::<FerretState>().fs;
        let thumbs = cx.global::<FerretState>().thumb_cache.clone();
        let window_handle = self.window;
        // The read, after the debounce: one settled request per sweep.
        self.tasks.push(cx.spawn(async move |pane, cx| {
            cx.background_executor()
                .timer(ferret_preview::config::PREVIEW_DEBOUNCE)
                .await;
            let outcome = load_single(&fs, &thumbs, &entry, remote).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = pane.update(cx, |pane, cx| {
                    if pane.generation != generation {
                        return;
                    }
                    pane.loading_since = None;
                    pane.apply_loaded(outcome, window, cx);
                });
            });
        }));
        // The skeleton, past the loading threshold: local reads almost never
        // reach it, remote ones always do — and under it nothing flashes.
        self.tasks.push(cx.spawn(async move |pane, cx| {
            cx.background_executor()
                .timer(ferret_preview::config::LOADING_THRESHOLD)
                .await;
            let _ = pane.update(cx, |pane, cx| {
                if pane.generation != generation {
                    return;
                }
                if let PaneState::Loading { skeleton, .. } = &mut pane.state {
                    *skeleton = true;
                    cx.notify();
                }
            });
        }));
    }

    /// Swap the loaded value into the reused editor entity.
    fn apply_loaded(&mut self, outcome: LoadOutcome, window: &mut Window, cx: &mut Context<Self>) {
        match outcome {
            LoadOutcome::Text {
                entry,
                detail,
                text,
                language,
                wrap,
                footer,
                original,
            } => {
                let wrap = self.wrap_override.unwrap_or(wrap);
                self.suppress_dirty = true;
                self.editor.update(cx, |editor, cx| {
                    editor.set_highlighter(gpui::SharedString::from(registry_name(&language)), cx);
                    editor.set_soft_wrap(wrap, window, cx);
                    editor.set_value(text, window, cx);
                });
                self.suppress_dirty = false;
                self.state = PaneState::Ready {
                    entry,
                    detail,
                    language,
                    wrap,
                    footer,
                    original,
                };
                cx.notify();
            }
            LoadOutcome::Markdown {
                entry,
                detail,
                original,
                sanitized,
                footer,
                base,
                remote,
                edit_original,
            } => {
                self.suppress_dirty = true;
                self.editor.update(cx, |editor, cx| {
                    editor.set_highlighter(gpui::SharedString::from("markdown"), cx);
                    editor.set_soft_wrap(self.wrap_override.unwrap_or(true), window, cx);
                    editor.set_value(original, window, cx);
                });
                self.suppress_dirty = false;
                self.docview.update(cx, |docview, cx| {
                    docview.set_text(&sanitized, cx);
                });
                self.state = PaneState::ReadyMd {
                    entry,
                    detail,
                    footer,
                    show_source: false,
                    base,
                    remote,
                    sanitized,
                    original: edit_original,
                };
                cx.notify();
            }
            LoadOutcome::Table {
                entry,
                detail,
                table,
                widths,
                footer,
            } => {
                // The grid edits nothing: this is a bounded sample, and a
                // sample must never be saved back over the file.
                let table = std::sync::Arc::new(table);
                let grid = CsvGrid::new(&table, widths, window, cx);
                self.state = PaneState::Table {
                    entry,
                    detail,
                    grid,
                    table,
                    footer,
                };
                cx.notify();
            }
            LoadOutcome::Pdf {
                entry,
                detail,
                pdf,
                page,
            } => match render_bitmap(&page.bitmap) {
                Some(render_image) => {
                    self.state = PaneState::Pdf {
                        entry,
                        detail,
                        bitmap_size: (page.bitmap.width, page.bitmap.height),
                        render_image,
                        zoom: 0.0,
                        pages: Pages {
                            current: 1,
                            total: page.pages_total,
                        },
                        pdf,
                        pdf_gen: 0,
                        loading_page: false,
                        page_error: None,
                    };
                    cx.notify();
                }
                None => {
                    self.state = PaneState::Error {
                        name: entry.name().to_string(),
                        message: format!(
                            "Ferret can't preview {} — the rendered page didn't fit its frame.",
                            entry.name()
                        ),
                    };
                    cx.notify();
                }
            },
            LoadOutcome::Image {
                entry,
                detail,
                image,
                footer: note,
            } => {
                let Some(first) = image.frames.first() else {
                    self.state = PaneState::Error {
                        name: entry.name().to_string(),
                        message: format!(
                            "Ferret can't preview {} — nothing decoded.",
                            entry.name()
                        ),
                    };
                    cx.notify();
                    return;
                };
                match render_image(&image) {
                    Some(render_image) => {
                        let mut footer =
                            if (first.bitmap.width, first.bitmap.height) != image.natural {
                                Some(format!(
                                    "Scaled from {} × {}.",
                                    image.natural.0, image.natural.1
                                ))
                            } else {
                                None
                            };
                        if image.frames_capped {
                            let note =
                                format!("Animation shortened to {} frames.", image.frames.len());
                            footer = Some(match footer {
                                Some(existing) => format!("{existing} {note}"),
                                None => note,
                            });
                        }
                        if let Some(trailing) = note {
                            footer = Some(match footer {
                                Some(existing) => format!("{existing} {trailing}"),
                                None => trailing,
                            });
                        }
                        self.state = PaneState::Image {
                            entry,
                            detail,
                            bitmap_size: (first.bitmap.width, first.bitmap.height),
                            render_image,
                            zoom: 0.0,
                            footer,
                        };
                        cx.notify();
                    }
                    None => {
                        self.state = PaneState::Error {
                            name: entry.name().to_string(),
                            message: format!(
                                "Ferret can't preview {} — the decoded pixels didn't fit their frame.",
                                entry.name()
                            ),
                        };
                        cx.notify();
                    }
                }
            }
            LoadOutcome::Unsupported {
                entry,
                detail,
                message,
            } => {
                self.state = PaneState::Unsupported {
                    entry,
                    detail,
                    message,
                };
                cx.notify();
            }
            LoadOutcome::Error { name, message } => {
                self.state = PaneState::Error { name, message };
                cx.notify();
            }
        }
    }

    /// Open the previewed path with its default application (double-click).
    pub(crate) fn open_current(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.state.entry().map(|entry| entry.path.clone()) else {
            return;
        };
        let Some(local) = path.as_local().map(std::path::Path::to_path_buf) else {
            return;
        };
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Err(err) = platform.open_with_default(&local).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    use gpui_component::notification::Notification;
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
    }

    /// Show the previewed path in Finder / Explorer, selected.
    pub(crate) fn reveal_current(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.state.entry().map(|entry| entry.path.clone()) else {
            return;
        };
        let Some(local) = path.as_local().map(std::path::Path::to_path_buf) else {
            return;
        };
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Err(err) = platform.reveal_in_file_manager(&local).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    use gpui_component::notification::Notification;
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
    }

    fn editor_element(&self) -> Editor {
        // The element gates input: writable exactly while an edit session
        // pins the buffer, read-only otherwise. `EditorState` itself is
        // never reconfigured, so toggling costs a re-render, not a reload.
        Editor::new(&self.editor)
            .readonly(self.editing.is_none())
            .bordered(false)
    }
}

impl Focusable for PreviewPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What the background read produced, ready to swap into the view.
///
/// The `Entry` travels with every file outcome: the header buttons and the
/// type icon need it, and it is one small clone — never file content.
///
/// Shared with the F-75 overlay, which consumes the same outcomes into its
/// own views — the same pipeline, two surfaces.
pub(crate) enum LoadOutcome {
    Text {
        entry: Entry,
        detail: String,
        text: String,
        language: SmolStr,
        wrap: bool,
        footer: Option<String>,
        /// The loaded bytes behind the text, for in-place editing. `Some`
        /// only for complete, lossless, atomic-saveable files.
        original: Option<EditOriginal>,
    },
    Markdown {
        entry: Entry,
        detail: String,
        /// The file's own source, for the source toggle.
        original: String,
        /// Sanitized source for the document view: no loadable image.
        sanitized: String,
        footer: Option<String>,
        base: Option<PathBuf>,
        remote: bool,
        /// The loaded bytes behind the source, for in-place editing.
        edit_original: Option<EditOriginal>,
    },
    Table {
        entry: Entry,
        detail: String,
        table: ParsedTable,
        /// Column widths in logical pixels, measured from the sample on the
        /// background task — never measured on the window thread.
        widths: Vec<f32>,
        footer: Option<String>,
    },
    Pdf {
        entry: Entry,
        detail: String,
        /// The complete bytes, shared with page navigation: turning a page
        /// re-renders from this input, never re-reads the file.
        pdf: std::sync::Arc<Vec<u8>>,
        /// Page one, rendered. Navigation replaces this, never the bytes.
        page: RenderedPage,
    },
    Unsupported {
        entry: Entry,
        detail: String,
        message: String,
    },
    Image {
        entry: Entry,
        detail: String,
        image: ferret_preview::DecodedImage,
        /// A note the pipeline already phrased ("First page."): appended
        /// after the decode footnotes the view derives itself.
        footer: Option<String>,
    },
    Error {
        name: String,
        message: String,
    },
}

/// Read, classify and build one file's preview. Blocking work stays inside
/// `Vfs` (which unblocks internally); nothing here touches a window.
///
/// Shared with the F-75 overlay, which runs the same pipeline on demand when
/// the pane has nothing fresh to reuse.
pub(crate) async fn load_single(
    fs: &LocalFs,
    thumbs: &ferret_preview::ThumbCache,
    entry: &Entry,
    remote: bool,
) -> LoadOutcome {
    let name = entry.name().to_string();
    let path = entry.path.clone();

    // The sniff first: reading 2 MiB of a 4 GB video to discover it is a
    // video is the waste this avoids.
    let sniff = match fs.read(&path, ferret_preview::config::SNIFF_BYTES).await {
        Ok(sniff) => sniff,
        Err(err) => return vfs_error(&name, err),
    };
    let known_size = entry.metadata.map(|metadata| metadata.len);
    let kind = ferret_preview::classify::classify(entry.name(), &sniff, known_size);

    // The full read, under the kind's budget. The sniff prefix is reread
    // within it — counted in benchmarks, never concatenated from offset
    // zero, because the API has no continuation handle.
    let budget = ferret_preview::budget_for(&kind, remote);
    let bytes = match fs.read(&path, budget).await {
        Ok(bytes) => bytes,
        Err(err) => {
            // A corrupt file must still show its size: fall back to hex over
            // the sniff when there is something to show.
            if !sniff.is_empty() && matches!(kind, ferret_preview::Kind::Binary) {
                return text_outcome(entry, kind, sniff, known_size, remote, fs, thumbs).await;
            }
            return vfs_error(&name, err);
        }
    };
    // The footer needs the total. The listing usually knows it; a point `stat`
    // fills the gap when it does not — except when the read itself proves the
    // file fits, which costs nothing at all.
    let total = match known_size {
        Some(size) => Some(size),
        None if bytes.len() < budget => None,
        None => fs.metadata(&path).await.ok().map(|md| md.len),
    };
    text_outcome(entry, kind, bytes, total, remote, fs, thumbs).await
}

/// Turn a kind plus its budgeted bytes into the outcome the view consumes.
async fn text_outcome(
    entry: &Entry,
    kind: ferret_preview::Kind,
    mut bytes: Vec<u8>,
    total: Option<u64>,
    remote: bool,
    fs: &LocalFs,
    thumbs: &ferret_preview::ThumbCache,
) -> LoadOutcome {
    let shown = bytes.len() as u64;
    let total = total.unwrap_or(shown);
    let base = entry.path.parent().unwrap_or_else(|| VfsPath::local("/"));
    // The edit snapshot is taken here, from the bytes the user is about to
    // see — never re-read later. Only text kinds keep their bytes, and only
    // a complete read on a plain file behind an atomic-save backend can ever
    // become an Edit button; everything else drops the clone below.
    let identity = FileIdentity::new(
        total,
        entry.metadata.and_then(|metadata| metadata.modified),
        &bytes,
    );
    // The bytes in hand cover the whole file: an empty file is complete,
    // a short read of a longer file is not.
    let complete = shown >= total;
    let candidate = matches!(
        kind,
        ferret_preview::Kind::Markdown | ferret_preview::Kind::Code { .. }
    )
    .then(|| {
        let can_write = !remote
            && matches!(entry.kind, EntryKind::File)
            && fs.capabilities_for(&entry.path).atomic_write;
        (bytes.clone(), identity, complete && can_write)
    });
    // PDFs render through the bundled backend where one answers (PV-D08):
    // installed shell handlers never decide whether ordinary PDFs work, so
    // Pdfium is asked before the OS thumbnail service. Complete input only —
    // PDFs index from the end, so a truncated head is useless — local or
    // remote alike, since a complete remote read already paid its latency.
    if matches!(&kind, ferret_preview::Kind::Pdf) && shown >= total {
        let pdf = std::sync::Arc::new(bytes);
        // Encrypted files decline here — `render_page` refuses before any
        // library call — and `build` below labels them Protected. Anything
        // else that declines gets the native question after this.
        if let Ok(page) = pdfium_page(pdf.clone(), 0).await {
            return pdf_load_outcome(entry, pdf, page);
        }
        // Sole owner again once the settled render dropped its clone; a
        // shared straggler costs one bounded copy down the native path,
        // never a panic.
        bytes = match std::sync::Arc::try_unwrap(pdf) {
            Ok(bytes) => bytes,
            Err(shared) => (*shared).clone(),
        };
    }
    let preview = ferret_preview::build(kind, bytes, Some(total), base);
    // Kinds with no in-process decoder get one best-effort native question —
    // a local PDF or HEIC answered by the OS — before falling through to
    // metadata. Anything the service declines keeps its `Unsupported`.
    let preview = if wants_native(&preview) {
        native_preview(fs, thumbs, entry, total)
            .await
            .unwrap_or(preview)
    } else {
        preview
    };
    preview_outcome(entry, preview, total, remote, candidate)
}

/// Decide the edit snapshot for one loaded text file.
///
/// `candidate` carries the raw bytes, their identity, and whether the read
/// was complete on a writable backend. The snapshot exists only when a fresh
/// decode of those bytes is saveable — uniform endings, lossless, untruncated
/// — so the Edit button is a flag read, never a second judgement.
fn edit_original(candidate: Option<(Vec<u8>, FileIdentity, bool)>) -> Option<EditOriginal> {
    let (bytes, identity, writable) = candidate?;
    if !writable {
        return None;
    }
    let saveable = ferret_preview::text::decode(&bytes).is_some_and(|decoded| decoded.saveable());
    saveable.then_some(EditOriginal {
        bytes,
        identity,
        saveable: true,
    })
}

/// Whether a built preview deserves one native question before it settles.
///
/// Only the kinds with no in-process decoder that the OS may cover: an
/// in-budget PDF, or an image format we recognise but cannot rasterise.
/// `TooLarge` never asks — past even the head budget is metadata, never an
/// attempted anything — and neither do text kinds, which already rendered.
fn wants_native(preview: &Preview) -> bool {
    matches!(
        preview,
        Preview::Unsupported {
            kind: ferret_preview::Kind::Pdf | ferret_preview::Kind::Image,
            reason: ferret_preview::Unsupported::NoRenderer,
        }
    )
}

/// Build the outcome for a rendered first page: the bytes travel with it so
/// page navigation re-renders from the same input, never re-reads the file.
///
/// A declined render never reaches here — the caller falls through to the
/// native thumbnail and then to metadata rather than waiting on a spinner.
fn pdf_load_outcome(
    entry: &Entry,
    pdf: std::sync::Arc<Vec<u8>>,
    page: RenderedPage,
) -> LoadOutcome {
    let total = pdf.len() as u64;
    let size_label = format_size(entry.metadata.map(|md| md.len).unwrap_or(total));
    let modified = entry
        .metadata
        .and_then(|md| md.modified)
        .map(format_date)
        .unwrap_or_else(|| "unknown date".to_string());
    LoadOutcome::Pdf {
        entry: entry.clone(),
        detail: format!("PDF document · {size_label} · {modified}"),
        pdf,
        page,
    }
}

/// Render one PDF page (zero-based) off the window thread, bounded by the
/// render deadline.
///
/// The deadline bounds the *wait*, not the native call: a hung helper is the
/// supervised-process follow-up, not something a timer preempts. On timeout
/// the caller falls through to the native thumbnail rather than failing.
async fn pdfium_page(
    pdf: std::sync::Arc<Vec<u8>>,
    page: u32,
) -> Result<RenderedPage, PdfUnavailable> {
    let render = smol::unblock(move || ferret_preview::render_page(&pdf, page));
    let deadline = smol::Timer::after(ferret_preview::config::PDF_RENDER_DEADLINE);
    futures::pin_mut!(render, deadline);
    match futures::future::select(render, deadline).await {
        futures::future::Either::Left((outcome, _)) => outcome,
        futures::future::Either::Right(_) => Err(PdfUnavailable::Failed),
    }
}

/// One best-effort OS thumbnail for the docked pane, cached on disk.
///
/// Order: memory is the pane's own (populated on apply), so disk first, the
/// service second, each once. Misses at any stage are `None` and the caller
/// keeps the `Unsupported` — a tile that cannot be drawn is a glyph, and a
/// pane that cannot be drawn is metadata, both finished states.
async fn native_preview(
    fs: &LocalFs,
    thumbs: &ferret_preview::ThumbCache,
    entry: &Entry,
    total: u64,
) -> Option<Preview> {
    use ferret_preview::config::NATIVE_PANE_PX;

    let local = entry.path.as_local()?;
    if total > ferret_platform::config::NATIVE_THUMBNAIL_INPUT_LIMIT {
        return None;
    }
    // The disk key needs freshness. The listing usually knows it; a point
    // `stat` fills the gap when it does not. Unknown stays unknown — a tile
    // that cannot be invalidated is future staleness, so no key, no cache.
    let modified = match entry.metadata.and_then(|md| md.modified) {
        Some(modified) => Some(modified),
        None => fs
            .metadata(&entry.path)
            .await
            .ok()
            .and_then(|md| md.modified),
    };
    let (len, secs, nanos) = ferret_preview::fingerprint(total, modified)?;
    let canonical = std::fs::canonicalize(local).ok()?;
    let key = ferret_preview::thumb_key(&canonical, len, secs, nanos, NATIVE_PANE_PX);
    if let Some(bitmap) = thumbs.lookup(&key) {
        return Some(rendered_preview(entry, bitmap));
    }
    let bitmap = ferret_platform::thumbnail_provider()
        .thumbnail(local, NATIVE_PANE_PX)
        .await
        .ok()??;
    if !bitmap.is_well_formed() {
        return None;
    }
    let bitmap = ferret_preview::Bitmap {
        width: bitmap.width,
        height: bitmap.height,
        has_transparency: ferret_preview::image::detect_transparency(&bitmap.bgra),
        bgra: bitmap.bgra,
    };
    // A failed store still returns the bitmap for this viewing: the disk
    // being unwritable must not cost a service round-trip every selection.
    thumbs.store(&key, &bitmap);
    Some(rendered_preview(entry, bitmap))
}

/// Wrap a native bitmap in the kind it was requested for, so the header
/// reads "PDF document" rather than "PDF image".
fn rendered_preview(entry: &Entry, bitmap: ferret_preview::Bitmap) -> Preview {
    let kind = if entry.extension() == Some("pdf") {
        ferret_preview::Kind::Pdf
    } else {
        ferret_preview::Kind::Image
    };
    Preview::Rendered {
        bitmap,
        kind,
        pages: None,
        by: ferret_preview::RenderedBy::NativeThumbnail,
    }
}

fn preview_outcome(
    entry: &Entry,
    preview: Preview,
    total: u64,
    remote: bool,
    edit: Option<(Vec<u8>, FileIdentity, bool)>,
) -> LoadOutcome {
    let size_label = format_size(entry.metadata.map(|md| md.len).unwrap_or(total));
    let modified = entry
        .metadata
        .and_then(|md| md.modified)
        .map(format_date)
        .unwrap_or_else(|| "unknown date".to_string());
    match preview {
        Preview::Markdown {
            source,
            truncated,
            base,
            ..
        } => markdown_outcome(
            entry,
            &size_label,
            &modified,
            source,
            truncated,
            base,
            remote,
            edit_original(edit),
        ),
        Preview::Code {
            text,
            language,
            truncated,
        } => {
            // Soft wrap defaults off for code and on for plain text and
            // logs — the split that matches how people read those two.
            let wrap = matches!(language.as_str(), "plain" | "log");
            let label = ferret_preview::languages::display_name(&language).to_string();
            LoadOutcome::Text {
                entry: entry.clone(),
                detail: format!("{label} · {size_label} · {modified}"),
                text,
                language,
                wrap,
                footer: truncation_footer(&truncated),
                original: edit_original(edit),
            }
        }
        Preview::Table { table, truncated } => {
            let label = if table.delimiter == b'\t' {
                "TSV table"
            } else {
                "CSV table"
            };
            // Column widths are a character-count heuristic over the sample:
            // O(the sample) on the background task, never measured per frame.
            let widths = grid::column_widths(&table);
            let width = widths.len();
            let dims = format!("{} rows × {width} cols", table.rows.len());
            let mut footer = format!("{dims} sample");
            if let Some(tail) = truncation_footer(&truncated) {
                footer.push_str(" · ");
                footer.push_str(&tail);
            }
            if table.omitted_trailing_record {
                footer.push_str(" · incomplete final record omitted");
            }
            if table.cut_field {
                footer.push_str(" · a field was cut to its cap");
            }
            let ragged = grid::ragged_count(&table);
            if ragged > 0 {
                footer.push_str(&format!(
                    " · {ragged} ragged row{}",
                    if ragged == 1 { "" } else { "s" }
                ));
            }
            LoadOutcome::Table {
                entry: entry.clone(),
                detail: format!("{label} · {size_label} · {modified}"),
                table,
                widths,
                footer: Some(footer),
            }
        }
        Preview::Binary { head, size } => LoadOutcome::Text {
            entry: entry.clone(),
            detail: format!("Binary · {} · {modified}", format_size(size)),
            text: ferret_preview::text::format_hex(&head),
            language: SmolStr::new("plain"),
            wrap: false,
            footer: (size > head.len() as u64).then(|| {
                format!(
                    "First {} of {}.",
                    format_size(head.len() as u64),
                    format_size(size)
                )
            }),
            // Hex is a view, not a buffer: there is nothing to save.
            original: None,
        },
        Preview::Image { image, .. } => LoadOutcome::Image {
            entry: entry.clone(),
            detail: format!("{} image · {size_label} · {modified}", image.format_name),
            image,
            footer: None,
        },
        Preview::Rendered {
            bitmap,
            kind,
            pages,
            by,
        } => {
            // Native bitmaps ride the image state: fit-to-pane, zoom, and
            // animation-free stills all come along. The kind labels the
            // header, so a PDF reads "PDF document", never "PDF image".
            let detail = format!("{} · {size_label} · {modified}", kind.display_name());
            let footer = match (pages, by) {
                (Some(pages), _) => Some(format!("Page {} of {}.", pages.current, pages.total)),
                // The service's best representation of a PDF is its first
                // page; saying so keeps nobody mistaking the poster for the
                // document. Other kinds are what they are: one bitmap.
                (None, ferret_preview::RenderedBy::NativeThumbnail)
                    if matches!(kind, ferret_preview::Kind::Pdf) =>
                {
                    Some("First page.".to_string())
                }
                _ => None,
            };
            LoadOutcome::Image {
                entry: entry.clone(),
                detail,
                image: DecodedImage {
                    natural: (bitmap.width, bitmap.height),
                    format_name: match kind {
                        ferret_preview::Kind::Pdf => "PDF",
                        _ => "Image",
                    },
                    frames: vec![Frame {
                        bitmap,
                        delay_ms: 0,
                    }],
                    frames_capped: false,
                },
                footer,
            }
        }
        Preview::Directory { .. } | Preview::Multiple { .. } => LoadOutcome::Error {
            name: entry.name().to_string(),
            message: "Ferret can't preview this selection here.".to_string(),
        },
        Preview::Unsupported { kind, reason } => {
            let detail = format!("{} · {size_label} · {modified}", kind.display_name());
            let message = match reason {
                ferret_preview::Unsupported::TooLarge { size, budget } => format!(
                    "Too large to preview: {} over a {} budget.",
                    format_size(size),
                    format_size(budget as u64)
                ),
                ferret_preview::Unsupported::Protected => {
                    "This file needs a password to open — Ferret can't preview it. Open it externally."
                        .to_string()
                }
                ferret_preview::Unsupported::NoRenderer => match kind {
                    ferret_preview::Kind::Image => {
                        "Image preview isn't available in this build — metadata and Open for now."
                            .to_string()
                    }
                    ferret_preview::Kind::Pdf => {
                        "Couldn't render this PDF here — it may need a handler this machine doesn't have. Metadata and Open for now."
                            .to_string()
                    }
                    _ => "No preview for this file yet — metadata and Open for now.".to_string(),
                },
            };
            LoadOutcome::Unsupported {
                entry: entry.clone(),
                detail,
                message,
            }
        }
    }
}

/// Translate our language name to the registry's.
///
/// The two vocabularies agree everywhere except here: the component's
/// built-in table knows `"csharp"`, not `"c_sharp"`. Unknown names pass
/// through untouched — the registry resolves them or gracefully declines,
/// and an unhighlighted file is never an error.
fn registry_name(language: &SmolStr) -> &str {
    match language.as_str() {
        "c_sharp" => "csharp",
        other => other,
    }
}

/// Build the Markdown outcome: sanitize for the document view, keep the
/// original for the source toggle.
///
/// Sanitizing fails safe: any refusal shows the bounded original source
/// with the reason, never a half-sanitized document.
#[allow(clippy::too_many_arguments)]
fn markdown_outcome(
    entry: &Entry,
    size_label: &str,
    modified: &str,
    source: String,
    truncated: Truncation,
    base: VfsPath,
    remote: bool,
    edit_original: Option<EditOriginal>,
) -> LoadOutcome {
    let detail = format!("Markdown · {size_label} · {modified}");
    let base_dir: Option<PathBuf> = if remote {
        None
    } else {
        base.as_local().map(std::path::Path::to_path_buf)
    };
    let mut footer = truncation_footer(&truncated);
    match ferret_preview::markdown::sanitize(&source, base_dir.as_deref()) {
        Ok(sanitized) => {
            let mut notes = Vec::new();
            if sanitized.blocked_remote > 0 {
                notes.push(format!(
                    "{} remote image{} not loaded",
                    sanitized.blocked_remote,
                    plural(sanitized.blocked_remote)
                ));
            }
            if sanitized.blocked_unsafe > 0 {
                notes.push(format!(
                    "{} unsafe image{} removed",
                    sanitized.blocked_unsafe,
                    plural(sanitized.blocked_unsafe)
                ));
            }
            if !sanitized.local.is_empty() {
                notes.push(format!(
                    "{} local image{} shown as text for now",
                    sanitized.local.len(),
                    plural(sanitized.local.len())
                ));
            }
            if sanitized.over_count > 0 {
                notes.push(format!("{} more images omitted", sanitized.over_count));
            }
            if !notes.is_empty() {
                let note = notes.join(" · ") + ".";
                footer = Some(match footer {
                    Some(existing) => format!("{existing} {note}"),
                    None => note,
                });
            }
            LoadOutcome::Markdown {
                entry: entry.clone(),
                detail,
                original: source,
                sanitized: sanitized.source,
                footer,
                base: base_dir,
                remote,
                edit_original,
            }
        }
        Err(reason) => {
            let why = match reason {
                ferret_preview::markdown::SanitizeError::Parse => "could not be parsed",
                ferret_preview::markdown::SanitizeError::Span => "failed its resource check",
                ferret_preview::markdown::SanitizeError::Limits => "exceeds rendered limits",
            };
            let note = format!("Rendered view {why} — showing source.");
            footer = Some(match footer {
                Some(existing) => format!("{existing} {note}"),
                None => note,
            });
            LoadOutcome::Text {
                entry: entry.clone(),
                detail,
                text: source,
                language: SmolStr::new("markdown"),
                wrap: true,
                footer,
                // The sanitizer refused, so the source view is a fallback
                // surface, not an editing one: saving from a refused
                // document would write back bytes the policy never approved.
                original: None,
            }
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Build one `RenderImage` for a single bitmap, once, at apply time.
///
/// PDF pages render one at a time: the current page is one upload, and a
/// page turn replaces it rather than retaining the document.
fn render_bitmap(bitmap: &Bitmap) -> Option<std::sync::Arc<gpui::RenderImage>> {
    let buffer = ::image::RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.bgra.clone())?;
    let frame = ::image::Frame::from_parts(buffer, 0, 0, ::image::Delay::from_numer_denom_ms(0, 1));
    Some(std::sync::Arc::new(gpui::RenderImage::new(vec![frame])))
}

/// Build one `RenderImage` per preview, once, at apply time.
///
/// The frames carry our BGRA bytes inside `RgbaImage` shells: the renderer
/// uploads the bytes opaquely as a BGRA texture, which is exactly the layout
/// the pipeline produced. Multi-frame images animate through the element's
/// own frame advance — no timer task, no per-frame allocation.
fn render_image(image: &ferret_preview::DecodedImage) -> Option<std::sync::Arc<gpui::RenderImage>> {
    let frames: Option<Vec<::image::Frame>> = image
        .frames
        .iter()
        .map(|frame| {
            ::image::RgbaImage::from_raw(
                frame.bitmap.width,
                frame.bitmap.height,
                frame.bitmap.bgra.clone(),
            )
            .map(|buffer| {
                ::image::Frame::from_parts(
                    buffer,
                    0,
                    0,
                    ::image::Delay::from_numer_denom_ms(frame.delay_ms, 1),
                )
            })
        })
        .collect();
    frames.map(|frames| std::sync::Arc::new(gpui::RenderImage::new(frames)))
}

/// "Showing the first 2 MiB of 4.1 GB." — or `None` for a whole file.
fn truncation_footer(truncated: &Truncation) -> Option<String> {
    truncated.is_truncated().then(|| {
        format!(
            "Showing the first {} of {}.",
            format_size(truncated.shown),
            format_size(truncated.total)
        )
    })
}

fn vfs_error(name: &str, err: VfsError) -> LoadOutcome {
    // `Denied` shows the error's own `Display` text, which the repo's error
    // convention already requires to be showable to a person unedited.
    // `Gone` keeps the header and offers nothing that would fail.
    let message = match &err {
        VfsError::NotFound { .. } => format!("{name} no longer exists."),
        _ => err.to_string(),
    };
    LoadOutcome::Error {
        name: name.to_string(),
        message,
    }
}

/// Count entries for a selected directory: one `read_dir` stream, bounded
/// batches, dropped with the generation when the selection moves on.
async fn count_entries(fs: &LocalFs, dir: &VfsPath) -> u64 {
    use ferret_vfs::ListingDetail;
    use futures::StreamExt;
    let mut stream = fs.read_dir(dir, ListingDetail::NamesOnly);
    let mut count = 0u64;
    while let Some(item) = stream.next().await {
        match item {
            Ok(batch) => count += batch.len() as u64,
            Err(_) => break,
        }
    }
    count
}

/// Header detail for a file whose kind is not classified yet.
fn entry_detail_no_kind(entry: &Entry) -> String {
    let size = entry
        .metadata
        .map(|md| format_size(md.len))
        .unwrap_or_else(|| "…".to_string());
    let modified = entry
        .metadata
        .and_then(|md| md.modified)
        .map(format_date)
        .unwrap_or_else(|| "unknown date".to_string());
    format!("{size} · {modified}")
}

fn directory_detail(entry: &Entry) -> String {
    let modified = entry
        .metadata
        .and_then(|md| md.modified)
        .map(format_date)
        .unwrap_or_else(|| "unknown date".to_string());
    format!("Folder · {modified}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_preview::Pages;

    fn pdf_entry() -> Entry {
        Entry::new(
            VfsPath::local("/docs/report.pdf"),
            "report.pdf",
            EntryKind::File,
        )
    }

    fn unsupported(kind: ferret_preview::Kind, reason: ferret_preview::Unsupported) -> Preview {
        Preview::Unsupported { kind, reason }
    }

    #[test]
    fn only_decoderless_kinds_earn_a_native_question() {
        // An in-budget PDF and an undecodable image: the service may cover
        // both, so both ask exactly once.
        assert!(wants_native(&unsupported(
            ferret_preview::Kind::Pdf,
            ferret_preview::Unsupported::NoRenderer
        )));
        assert!(wants_native(&unsupported(
            ferret_preview::Kind::Image,
            ferret_preview::Unsupported::NoRenderer
        )));
        // Everything else already has its answer: too large is metadata by
        // policy, text rendered, corruption fell to hex.
        assert!(!wants_native(&unsupported(
            ferret_preview::Kind::Pdf,
            ferret_preview::Unsupported::TooLarge { size: 1, budget: 1 }
        )));
        assert!(!wants_native(&Preview::Binary {
            head: vec![0],
            size: 1
        }));
        assert!(!wants_native(&Preview::Directory { entries: None }));
        // Protected is terminal: a password is required, so no service is
        // asked and nothing is retried in the background.
        assert!(!wants_native(&unsupported(
            ferret_preview::Kind::Pdf,
            ferret_preview::Unsupported::Protected
        )));
    }

    #[test]
    fn a_protected_pdf_names_the_password() {
        let outcome = preview_outcome(
            &pdf_entry(),
            unsupported(
                ferret_preview::Kind::Pdf,
                ferret_preview::Unsupported::Protected,
            ),
            1024,
            false,
            None,
        );
        let LoadOutcome::Unsupported {
            detail, message, ..
        } = outcome
        else {
            panic!("a protected PDF must stay metadata, never a render");
        };
        assert!(
            detail.starts_with("PDF document · "),
            "the kind labels the header: {detail}"
        );
        assert!(
            message.contains("password"),
            "the message names what is required: {message}"
        );
    }

    #[test]
    fn a_rendered_first_page_carries_its_bytes_for_navigation() {
        let pdf = std::sync::Arc::new(vec![1u8, 2, 3]);
        let outcome = pdf_load_outcome(
            &pdf_entry(),
            pdf.clone(),
            RenderedPage {
                bitmap: ferret_preview::Bitmap {
                    width: 100,
                    height: 80,
                    bgra: vec![255; 100 * 80 * 4],
                    has_transparency: false,
                },
                natural: (100, 80),
                pages_total: 9,
            },
        );
        let LoadOutcome::Pdf {
            detail, pdf: kept, ..
        } = outcome
        else {
            panic!("a rendered page must become the navigable outcome");
        };
        assert!(
            detail.starts_with("PDF document · "),
            "never 'PDF image': {detail}"
        );
        assert!(
            std::sync::Arc::ptr_eq(&kept, &pdf),
            "navigation shares the bytes; it never re-reads the file"
        );
    }

    #[test]
    fn a_rendered_pdf_reads_as_a_document_with_a_first_page_footer() {
        let outcome = preview_outcome(
            &pdf_entry(),
            Preview::Rendered {
                bitmap: ferret_preview::Bitmap {
                    width: 100,
                    height: 80,
                    bgra: vec![255; 100 * 80 * 4],
                    has_transparency: false,
                },
                kind: ferret_preview::Kind::Pdf,
                pages: None,
                by: ferret_preview::RenderedBy::NativeThumbnail,
            },
            1024,
            false,
            None,
        );
        let LoadOutcome::Image {
            detail,
            image,
            footer,
            ..
        } = outcome
        else {
            panic!("a rendered bitmap must ride the image state");
        };
        assert!(
            detail.starts_with("PDF document · "),
            "never 'PDF image': {detail}"
        );
        assert_eq!(footer.as_deref(), Some("First page."));
        assert_eq!(image.frames.len(), 1);
        assert!(!image.frames_capped);
    }

    #[test]
    fn a_rendered_image_carries_no_footer() {
        let outcome = preview_outcome(
            &pdf_entry(),
            Preview::Rendered {
                bitmap: ferret_preview::Bitmap {
                    width: 64,
                    height: 64,
                    bgra: vec![255; 64 * 64 * 4],
                    has_transparency: false,
                },
                kind: ferret_preview::Kind::Image,
                pages: None,
                by: ferret_preview::RenderedBy::NativeThumbnail,
            },
            512,
            false,
            None,
        );
        let LoadOutcome::Image { footer, .. } = outcome else {
            panic!("a rendered bitmap must ride the image state");
        };
        assert_eq!(footer, None);
    }

    #[test]
    fn paged_renderers_report_their_position() {
        let outcome = preview_outcome(
            &pdf_entry(),
            Preview::Rendered {
                bitmap: ferret_preview::Bitmap {
                    width: 100,
                    height: 80,
                    bgra: vec![255; 100 * 80 * 4],
                    has_transparency: false,
                },
                kind: ferret_preview::Kind::Pdf,
                pages: Some(Pages {
                    current: 3,
                    total: 9,
                }),
                by: ferret_preview::RenderedBy::Pdfium,
            },
            1024,
            false,
            None,
        );
        let LoadOutcome::Image { footer, .. } = outcome else {
            panic!("a rendered bitmap must ride the image state");
        };
        assert_eq!(footer.as_deref(), Some("Page 3 of 9."));
    }

    /// One hand-made single-page PDF: base-14 Helvetica, no embedding,
    /// byte-exact xref. `qlmanage -t` renders it, which is what makes it a
    /// probe of the native path rather than of our patience.
    const MINIMAL_PDF: &[u8] = b"\
        %PDF-1.4\n\
        1 0 obj\n\
        << /Type /Catalog /Pages 2 0 R >>\n\
        endobj\n\
        2 0 obj\n\
        << /Type /Pages /Kids [3 0 R] /Count 1 >>\n\
        endobj\n\
        3 0 obj\n\
        << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\n\
        endobj\n\
        4 0 obj\n\
        << /Length 45 >>\n\
        stream\n\
        BT /F1 18 Tf 50 120 Td (Hello thumbnail) Tj ET\n\
        endstream\n\
        endobj\n\
        5 0 obj\n\
        << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n\
        endobj\n\
        xref\n\
        0 6\n\
        0000000000 65535 f \n\
        0000000009 00000 n \n\
        0000000058 00000 n \n\
        0000000115 00000 n \n\
        0000000241 00000 n \n\
        0000000337 00000 n \n\
        trailer\n\
        << /Size 6 /Root 1 0 R >>\n\
        startxref\n\
        407\n\
        %%EOF\n\
        ";

    /// The native path end to end, where the OS answers.
    ///
    /// Writes the fixture to a temp dir, asks for a pane preview, and
    /// requires a `Rendered` bitmap — then asks again and requires
    /// byte-identical pixels, which is the disk cache proving itself.
    /// Where the service is absent (Windows pending its helper, headless
    /// CI) the provider declines and the test skips with its reason on
    /// record: the OS renderer is verified manually per the plan, never
    /// asserted blindly here.
    #[test]
    fn a_pdf_round_trips_the_native_path_and_its_cache() {
        let dir = std::env::temp_dir().join(format!("ferret-native-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("report.pdf");
        std::fs::write(&file, MINIMAL_PDF).unwrap();
        let entry = Entry::new(VfsPath::local(&file), "report.pdf", EntryKind::File);
        let len = MINIMAL_PDF.len() as u64;
        let cache = ferret_preview::ThumbCache::new(dir.join("cache"));
        let fs = LocalFs::new();
        let first = futures::executor::block_on(native_preview(&fs, &cache, &entry, len));
        let Some(Preview::Rendered { bitmap, kind, .. }) = first else {
            eprintln!("SKIP: the native thumbnail service declined the probe PDF");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        assert_eq!(kind, ferret_preview::Kind::Pdf);
        assert!(bitmap.is_well_formed());
        // Second visit: identical pixels without touching the service.
        let second = futures::executor::block_on(native_preview(&fs, &cache, &entry, len));
        let Some(Preview::Rendered { bitmap: cached, .. }) = second else {
            panic!("a stored tile must hit its own cache entry");
        };
        assert_eq!(cached.bgra, bitmap.bgra);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_names_match_the_builtin_table_where_it_differs() {
        // The component's built-in table knows "csharp", not "c_sharp" — a
        // C# file that misses this renders with no colour at all.
        assert_eq!(registry_name(&SmolStr::new("c_sharp")), "csharp");
        // Everything else passes through: the registry resolves what it
        // knows and gracefully declines the rest.
        assert_eq!(registry_name(&SmolStr::new("rust")), "rust");
        assert_eq!(registry_name(&SmolStr::new("plain")), "plain");
        assert_eq!(registry_name(&SmolStr::new("nix")), "nix");
    }
}
