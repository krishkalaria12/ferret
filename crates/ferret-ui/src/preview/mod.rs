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
//! Later steps add `grammars.rs` (extended tree-sitter registrations),
//! `edit.rs` (F-36) and `overlay.rs` (F-75). They land here, inside this
//! module, so a `gpui-component` bump touches one directory (PV-R04).

mod header;
mod render;

use std::time::Instant;

use ferret_preview::{Preview, Truncation};
use ferret_vfs::{Entry, EntryKind, LocalFs, Vfs as _, VfsError, VfsPath};
use gpui::{App, AppContext as _, Context, Entity, FocusHandle, Focusable, Task, Window};
use gpui_component::WindowExt as _;
use gpui_component::input::{Editor, EditorState};
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
        wrap: bool,
        footer: Option<String>,
    },
    Unsupported {
        entry: Entry,
        detail: String,
        message: String,
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
            | PaneState::Unsupported { entry, .. } => Some(entry),
            _ => None,
        }
    }
}

pub struct PreviewPane {
    editor: Entity<EditorState>,
    focus: FocusHandle,
    /// The window, for background tasks to re-enter with a `Window` when
    /// the bytes land (the editor's `set_value` needs one).
    window: gpui::AnyWindowHandle,
    state: PaneState,
    /// Bumps per selection; results carry theirs and stale ones are
    /// discarded. One generation, debounced, cancel-by-drop (PV-D11).
    generation: u64,
    loading_since: Option<Instant>,
    tasks: Vec<Task<()>>,
    /// Cached browsed-directory totals for the [`PaneState::Empty`] line.
    empty_cache: Option<(VfsPath, usize, Option<u64>)>,
    /// Session word-wrap override from the overflow menu. `None` means the
    /// per-kind default: off for code, on for plain text and logs.
    wrap_override: Option<bool>,
}

impl PreviewPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let line_numbers = cx.global::<FerretState>().settings.preview.line_numbers;
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("plain")
                .line_number(line_numbers)
                .soft_wrap(true)
        });
        Self {
            editor,
            focus: cx.focus_handle(),
            window: window.window_handle(),
            state: PaneState::Empty {
                name: String::new(),
                rows: 0,
                bytes: None,
            },
            generation: 0,
            loading_since: None,
            tasks: Vec::new(),
            empty_cache: None,
            wrap_override: None,
        }
    }

    /// The editor's focus, for the focus-preview shortcut.
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx).clone()
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
        let window_handle = self.window;
        // The read, after the debounce: one settled request per sweep.
        self.tasks.push(cx.spawn(async move |pane, cx| {
            cx.background_executor()
                .timer(ferret_preview::config::PREVIEW_DEBOUNCE)
                .await;
            let outcome = load_single(&fs, &entry, remote).await;
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
            } => {
                let wrap = self.wrap_override.unwrap_or(wrap);
                self.editor.update(cx, |editor, cx| {
                    editor.set_highlighter(gpui::SharedString::from(language.as_str()), cx);
                    editor.set_soft_wrap(wrap, window, cx);
                    editor.set_value(text, window, cx);
                });
                self.state = PaneState::Ready {
                    entry,
                    detail,
                    wrap,
                    footer,
                };
                cx.notify();
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
        Editor::new(&self.editor).readonly(true).bordered(false)
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
enum LoadOutcome {
    Text {
        entry: Entry,
        detail: String,
        text: String,
        language: SmolStr,
        wrap: bool,
        footer: Option<String>,
    },
    Unsupported {
        entry: Entry,
        detail: String,
        message: String,
    },
    Error {
        name: String,
        message: String,
    },
}

/// Read, classify and build one file's preview. Blocking work stays inside
/// `Vfs` (which unblocks internally); nothing here touches a window.
async fn load_single(fs: &LocalFs, entry: &Entry, remote: bool) -> LoadOutcome {
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
                return text_outcome(entry, &path, kind, sniff, known_size);
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
    text_outcome(entry, &path, kind, bytes, total)
}

/// Turn a kind plus its budgeted bytes into the outcome the view consumes.
fn text_outcome(
    entry: &Entry,
    path: &VfsPath,
    kind: ferret_preview::Kind,
    bytes: Vec<u8>,
    total: Option<u64>,
) -> LoadOutcome {
    let shown = bytes.len() as u64;
    let total = total.unwrap_or(shown);
    let base = path.parent().unwrap_or_else(|| VfsPath::local("/"));
    let preview = ferret_preview::build(kind, bytes, Some(total), base);
    preview_outcome(entry, preview, total)
}

fn preview_outcome(entry: &Entry, preview: Preview, total: u64) -> LoadOutcome {
    let size_label = format_size(entry.metadata.map(|md| md.len).unwrap_or(total));
    let modified = entry
        .metadata
        .and_then(|md| md.modified)
        .map(format_date)
        .unwrap_or_else(|| "unknown date".to_string());
    match preview {
        Preview::Markdown {
            source, truncated, ..
        } => LoadOutcome::Text {
            entry: entry.clone(),
            detail: format!("Markdown · {size_label} · {modified}"),
            text: source,
            language: SmolStr::new("markdown"),
            // Rendered Markdown arrives with the document view; the source
            // wraps like prose until then.
            wrap: true,
            footer: truncation_footer(&truncated),
        },
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
            }
        }
        Preview::Table { table, truncated } => {
            let label = if table.delimiter == b'\t' {
                "TSV table"
            } else {
                "CSV table"
            };
            let dims = match table.header {
                Some(ref header) => format!("{} rows × {} cols", table.rows.len(), header.len()),
                None => format!("{} rows", table.rows.len()),
            };
            let mut footer = format!("{dims} sample");
            if let Some(tail) = truncation_footer(&truncated) {
                footer.push_str(" · ");
                footer.push_str(&tail);
            }
            if table.omitted_trailing_record {
                footer.push_str(" · incomplete final record omitted");
            }
            LoadOutcome::Text {
                entry: entry.clone(),
                detail: format!("{label} · {size_label} · {modified}"),
                text: sample_text(&table),
                language: SmolStr::new("plain"),
                wrap: false,
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
        },
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
                ferret_preview::Unsupported::NoRenderer => match kind {
                    ferret_preview::Kind::Image => {
                        "Image preview isn't available in this build — metadata and Open for now."
                            .to_string()
                    }
                    ferret_preview::Kind::Pdf => {
                        "PDF preview isn't available in this build — metadata and Open for now."
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

/// Render the bounded table sample as text until the grid lands.
///
/// Fields rejoined with the sniffed delimiter, display lines cut at the same
/// width the text view enforces. Lossy around quoting by construction — the
/// grid (step 09) replaces this rather than refining it.
fn sample_text(table: &ferret_preview::ParsedTable) -> String {
    use ferret_preview::config::TEXT_MAX_LINE_BYTES;
    let delimiter = table.delimiter as char;
    let mut out = String::new();
    let mut rows = Vec::with_capacity(table.rows.len() + 1);
    if let Some(header) = &table.header {
        rows.push(header);
    }
    rows.extend(&table.rows);
    for row in rows {
        let mut line = row.join(&delimiter.to_string());
        if line.len() > TEXT_MAX_LINE_BYTES {
            let mut end = TEXT_MAX_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push('…');
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
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
