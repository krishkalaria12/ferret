# Repository Guidelines

This file explains how humans and coding agents should work in **Ferret**.

## Project at a Glance

Ferret is an open-source file manager with a terminal that stays synced to the
folder you are browsing. It targets **macOS and Windows** (Linux deferred, see
D-19). Rust 2024 workspace built on [GPUI](https://www.gpui.rs/) and
[gpui-component](https://github.com/longbridge/gpui-component).

**The repository is currently scaffolding.** It builds and opens a window; no
product feature is implemented.

- [`docs/ui-base.html`](docs/ui-base.html) — the **base** explorer shell (sidebar,
  tabs, path bar, file pane, status bar): mockup in both themes, region geometry in
  logical pixels, the `gpui-component` widget behind each region, token mapping,
  pointer and keyboard contract, and every state the file pane can be in. **This is
  the specification for the current phase of work.**
- [`docs/plan.html`](docs/plan.html) — the whole product: 51 feature IDs, target
  architecture, platform hazards, decisions, phases.

Phase 1 is the base and must be finished first; phases 2–9 are addons that plug into
the completed shell. Read both documents before proposing structural work, and cite
feature IDs (F-nn) and decisions (D-nn) when you do.

## The Current Task: build the base

**Implement the whole base explorer shell**, specified by `docs/ui-base.html`.
Work through its §10 build order in sequence — each step leaves the app runnable,
which is what makes a long run recoverable when something goes wrong.

Already done, do not redo:

| | |
| --- | --- |
| `ferret-vfs` | `Vfs` trait, `LocalFs`, `VfsPath`. Streaming listings, cancel-by-drop. |
| `ferret-settings` | Settings schema, keymap, per-folder view preferences. |
| `crates/ferret-ui/assets/icons/` | 64 bundled file icons and their mapping table. |
| `xtask` | Fixture generator and listing benchmark. |

Still to create: `ferret-platform` (trash, reveal, open-with, volumes, Places),
the `notify` file watcher in `ferret-vfs`, and everything in `ferret-ui`.

### Gates

Do not move past a gate until it passes. These exist because each one is far
cheaper to satisfy now than to retrofit.

1. **Virtualisation.** As soon as the file pane renders rows, point it at the
   fixture's `flat/` (200,000 entries) and scroll. Full frame rate, or stop and
   fix it — no later work recovers a table that was never virtualised.
2. **Focus.** Selection is drawn differently when the pane has focus
   (`--app-selected`) than when it does not (`--app-selected-idle`), and arrow
   keys follow focus. Build this with selection, not after.
3. **Both themes.** Check light and dark at every visual step. A token defined
   for only one is a bug.
4. **Both platforms.** Every keybinding needs a Windows equivalent. Anything
   touching paths, permissions or the trash needs a plan for both.
5. **Trash before delete.** `remove(Trash)` currently returns `Unsupported` on
   purpose. Wire it to the real trash before any delete path ships — never fall
   back to permanent deletion.
6. **Run it.** `cargo check` passing proves nothing about GPUI: every failure
   mode in the bootstrap contract below is silent at compile time.

### Finished when

You can set Ferret as your default file manager and not miss the system one for
browsing, opening, renaming, moving and deleting.

### If you get stuck

Prefer leaving a step unfinished and saying so over inventing an abstraction to
route around it. Open questions Q-01 to Q-08 in `docs/plan.html` are genuinely
open — if you hit one, say which, and take the simplest option that keeps the
base shippable.

```text
.
├── Cargo.toml            # Workspace members, shared dependencies, profiles
├── justfile              # Development commands
├── crates/
│   ├── ferret/           # Native binary bootstrap  (bin `ferret`)
│   │   ├── assets/       # Application icon files
│   │   └── src/main.rs   # Application, menu, keybindings, window setup
│   ├── ferret-ui/        # Views and Ferret-specific theme tokens
│   │   ├── src/lib.rs    # Public API and RootView
│   │   ├── src/theme.rs  # Terminal ANSI palette (interface colours come from gpui-component)
│   │   ├── assets/icons/ # Bundled file icons (Material Icon Theme, MIT) + mapping
│   │   └── themes/groknight.json
│   ├── ferret-settings/  # Settings schema and per-folder view preferences
│   │   ├── src/schema.rs # The settings file's shape (D-21)
│   │   ├── src/folder_prefs.rs  # Per-folder views, bounded LRU (D-16)
│   │   ├── src/errors.rs
│   │   ├── src/config.rs
│   │   └── default-settings.json  # Generated; a test keeps it in sync
│   └── ferret-vfs/       # Storage layer
│       ├── src/vfs.rs    # The Vfs trait (D-17)
│       ├── src/local.rs  # LocalFs over std::fs
│       ├── src/path.rs   # VfsPath
│       ├── src/errors.rs # VfsError + the crate's Result alias
│       └── src/config.rs # Every tunable in the crate
├── xtask/                # Fixture generator and listing benchmark
│   ├── src/config.rs     # Fixture shape and performance budgets
│   ├── src/fixture.rs
│   └── src/bench.rs
├── docs/ui-base.html     # Base explorer shell — the UI spec for current work
├── docs/plan.html        # Product + architecture plan
└── README.md
```

| Crate | Package | Responsibility |
| --- | --- | --- |
| `crates/ferret` | `ferret` | Starts GPUI, registers global actions, configures the native window, mounts the root view. |
| `crates/ferret-ui` | `ferret-ui` | Owns views, components, local UI state, styling, and theme tokens. |
| `crates/ferret-settings` | `ferret-settings` | The settings schema, per-folder view preferences, the keymap. Knows nothing about GPUI or the filesystem layer. |
| `crates/ferret-vfs` | `ferret-vfs` | The `Vfs` trait, the local backend, path handling. Knows nothing about GPUI. |
| `xtask` | `xtask` | Development tasks: the performance fixture and the listing benchmark. |

**Ownership rule:** put behavior in the crate that owns it. Window and
application lifecycle code belongs in `ferret`; reusable visual behavior belongs
in `ferret-ui`. Expose cross-crate APIs through the owning crate's `src/lib.rs`
instead of reaching into private modules.

More service crates arrive as features land (`ferret-terminal`, `ferret-git`, …).
Do not create them speculatively — add one when the code that belongs in it
exists. `ferret-core` is the same: the shared domain types currently live in
`ferret-vfs` and move out when a second service crate needs them.

Respect the layering: no service crate may depend on `ferret-ui`, and **only
`ferret-ui` may depend on `gpui-component`**. Service crates should stay
GPUI-free where they can — `ferret-vfs` and `ferret-settings` both are, and their
50 tests run without opening a window. That is the standard to hold.

`ferret-settings` deliberately does **not** depend on `ferret-vfs`. It keys folder
preferences by plain `&str`; the caller passes `VfsPath::preference_key()`. Keep
that boundary — settings is not a storage concern.

## Quick Start

### Prerequisites

- A current stable Rust toolchain (1.90+) through [rustup](https://rustup.rs/).
- [`just`](https://github.com/casey/just) for the repository shortcuts (optional).
- The native build tools required by Rust and GPUI for your operating system.
  macOS needs the Xcode Command Line Tools (not full Xcode — see the dependency
  policy below); Windows needs the MSVC toolchain.

### Run the application

```sh
just dev          # or: cargo run -p ferret
```

The app opens a `1180 × 760` window. Quit with **⌘Q** on macOS or **Ctrl+Q** on
Windows.

## Development Commands

| Command | Purpose | When to use it |
| --- | --- | --- |
| `just dev` | Run `ferret` in debug mode. | Normal development and visual checks. |
| `just run` | Run the optimized release build. | Confirm release-mode behavior or performance. |
| `just build` | Build the release binary. | Produce or validate an optimized binary. |
| `just check` | Type-check the workspace. | Fast validation after Rust changes. |
| `just fmt` | Format all Rust code. | Before finishing any Rust edit. |
| `just lint` | `fmt --check` plus `clippy -D warnings`. | Before a PR or after non-trivial changes. |
| `just test` | Run workspace tests. | When behavior changes or tests are added. |
| `cargo xtask fixture` | Build the performance fixture. | Once, before benchmarking or testing the file pane. |
| `cargo xtask bench` | Measure listing performance against budget. | After touching `ferret-vfs` or the file pane. Exits non-zero past budget. |

Do not claim a command passed unless you actually ran it. If a command cannot run
because of the local environment, report the command and the exact blocker.

The first build compiles GPUI from source and takes a long time. `[profile.dev]`
builds dependencies at `opt-level = 2` on purpose — GPUI is unusable otherwise.

## Recommended Workflow

1. **Read before editing.** Inspect the target module, its crate's `Cargo.toml`, and nearby patterns.
2. **Check the plan.** For feature work, confirm which phase in `docs/plan.html` it belongs to.
3. **Choose the owning crate.** Keep native lifecycle concerns in `ferret` and UI concerns in `ferret-ui`.
4. **Make the smallest coherent change.** Avoid unrelated refactors, speculative abstractions, or new dependencies.
5. **Format and type-check.** Run `just fmt` and at least `just check`.
6. **Run focused validation.** Add tests for meaningful logic; run `just lint` for non-trivial work.
7. **Inspect the UI.** Run `just dev` for changes to layout, colors, interaction, window options, or rendering.
8. **Summarize precisely.** State what changed, why, what was validated, and any remaining limitations.

## Coding Style

Follow standard Rust and `rustfmt` conventions:

- Four-space indentation; never hand-align code against `rustfmt`.
- `snake_case` for modules, functions, variables, and tests.
- `PascalCase` for structs, enums, and traits.
- `SCREAMING_SNAKE_CASE` for constants.
- Prefer explicit, domain-specific names over abbreviations.
- Keep functions focused and place helpers near the behavior they support.
- Use comments to explain constraints or intent, not to restate the code.
- Avoid `unwrap()` in recoverable application paths. At process boundaries, use an error message that explains the failed operation.
- Put the error type in `errors.rs` and tunables in `config.rs` — see below.
- Do not add dependencies when GPUI, `gpui-component`, the standard library, or a small local helper already solves the problem clearly.

### Imports and public APIs

Let `rustfmt` normalize imports. In `crates/ferret-ui/src/lib.rs`, keep modules
private unless callers need the module itself, and re-export the smallest useful
API:

```rust
mod status_badge;

pub use status_badge::{StatusBadge, StatusKind};
```

This allows the binary crate to use `ferret_ui::StatusBadge` without depending on
the UI crate's internal file layout.

### Errors live in `errors.rs`

Every library crate defines its error type in its own `errors.rs`, using
[`thiserror`](https://docs.rs/thiserror). Alongside it, define the crate's
`Result` alias **with the error type as a default parameter**:

```rust
// crates/<crate>/src/errors.rs
pub type Result<T, E = VfsError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("Ferret can't read {path} — you don't have permission to open it.")]
    PermissionDenied { path: VfsPath },
    // …
}
```

The default parameter is the point: it ties the error to the `Result` so every
signature in the crate reads `Result<T>` and the error type is never spelled out,
never accidentally varies, and changes in one place. Leaving the second parameter
open means a caller that genuinely needs a different error can still write
`Result<T, OtherError>` instead of importing `std::result::Result` and shadowing
the alias.

Re-export both from `lib.rs` (`pub use errors::{Result, VfsError};`) and keep the
module private — callers want the types, not the file.

Two more rules:

- **Libraries use `thiserror`, binaries use `anyhow`.** A library's caller needs
  to match on what went wrong; a binary's caller is a person reading a message.
  `ferret-vfs` uses `thiserror`; `xtask` and `ferret` use `anyhow`.
- **Every variant carries its subject.** "Operation not permitted" with no path
  is the failure this convention exists to prevent. Write `Display` text that can
  be shown to a person unedited, and say what they can do about it.

### Configuration lives in `config.rs`

Every crate keeps its tunables in one `config.rs`, not scattered beside the code
that reads them:

```rust
// crates/ferret-vfs/src/config.rs
/// How many entries may sit in a listing channel before the producer parks.
///
/// Bounded on purpose: unbounded would let a 200,000-entry walk materialise the
/// whole directory in memory, which is what streaming exists to avoid.
pub const LISTING_BUFFER: usize = 512;
```

Knobs are then findable and comparable in one place rather than discovered one
`grep` at a time, and changing a budget or a capacity shows up as a diff to a
single file. **Document the reasoning for each value** — a constant with no
explanation is one nobody can safely change.

This is for *tunables*: buffer sizes, capacities, budgets, limits, default paths.
Values that are part of a type's contract — an enum discriminant, a protocol
constant, a magic number in a file format — belong with that type. Runtime
user-facing settings are different again and belong to `ferret-settings` when it
exists (D-12).

Keep `config` public (`pub mod config;`) so tests and other crates can assert
against the same constants the code uses, rather than restating them.

## Advanced GPUI Guidance

### Mental model

- `RootView` owns application UI state.
- `impl Render for RootView` derives the current element tree from that state.
- Reusable, value-like components use `#[derive(IntoElement)]` and `RenderOnce`.
- Event handlers that mutate a view should be created with `cx.listener(...)`.
- After mutating render-relevant state, call `cx.notify()` to request another render.
- Stable element IDs are important for interactive elements.
- Interface colours come from `cx.theme()` (`gpui_component::ActiveTheme`), never from literals.

### The bootstrap contract

`crates/ferret/src/main.rs` follows `gpui-component`'s required setup. All four
parts are load-bearing; removing any one fails at runtime, not at compile time:

1. `.with_assets(Assets)` on the application — supplies the SVGs behind `IconName`. Without it every icon renders blank.
2. `gpui_component::init(cx)` inside `run` — must precede construction of any component type.
3. The window opens from a spawned task, so the theme and assets are loaded before the first frame.
4. The window's first entity is `Root::new(view, window, cx)` — it hosts the layers dialogs, popovers, notifications and tooltips render into. A dialog mounted under a non-`Root` window silently does not appear.

`cx.activate(true)` must run *after* `open_window`, inside the same task. Calling
it earlier activates a windowless app and leaves Ferret behind whatever had focus.

Windows that draw their own title bar must build their `WindowOptions` from
`TitleBar::window_options()`, which sets `app_owns_titlebar_drag`.

### State update example

```rust
Button::new("save-button")
    .primary()
    .label("Save")
    .on_click(cx.listener(|this, _, _, cx| {
        this.clicks += 1;
        cx.notify();
    }))
```

If `cx.notify()` is omitted, the state may change without the visible UI updating.

### Adding a component

1. Put it in a focused module under `crates/ferret-ui/src/`.
2. Prefer an existing `gpui-component` widget over a new local one. Reach for `Table`, `Tree`, `Dock`, `Resizable`, `Input`, `Sidebar`, `Breadcrumb` and `Dialog` before hand-rolling.
3. Keep internal styling details private.
4. Expose only the types and methods consumers need.
5. Re-export the public surface from `crates/ferret-ui/src/lib.rs`.
6. Verify all interaction states in the running app.

## Dependency Policy

`gpui` and `gpui_platform` are tracked from Zed's Git repository; `gpui-component`
resolves `gpui` from the same Git source, so Cargo unifies them into a single
crate instance. **Do not add a `rev` to one without pinning the other to match** —
two `gpui` instances produce confusing type-mismatch errors. Reproducibility comes
from `Cargo.lock`, which records the exact commit.

Because this workspace tracks GPUI from Git rather than a pinned crates.io
release, APIs may differ from older tutorials. Treat the checked-out dependency
and current compiler errors as authoritative.

## Theme and Visual Changes

**General interface colours come from `gpui-component`.** Use `cx.theme().background`,
`.foreground`, `.muted_foreground`, `.border`, `.radius` and friends. Do not add a
token to `ferret-ui` for something the component theme already names.

`crates/ferret-ui/src/theme.rs` holds only what `gpui-component` has no concept of:
the sixteen ANSI slots the terminal panel needs, and the colours that encode file
and git state in the browser. The palette in
`crates/ferret-ui/themes/groknight.json` mirrors `TerminalPalette::groknight()`; if
one changes, change both.

Preserve these visual constraints unless the task explicitly changes the design:

- Components use theme tokens instead of scattering colour literals.
- Interactive components define normal, hover, active, and disabled states when applicable.
- Ferret ships a light theme as well as a dark one. Any new token must be defined for both; never hard-code a colour that only reads correctly in one appearance.

Window vibrancy or blur is **not** currently used. It is unreliable outside macOS
(compositor-dependent on Linux, version-gated on Windows), so the design must be
legible on an opaque themed background. Treat blur as an optional macOS
enhancement, never as something a layout depends on.

## Desktop and Window Changes

Edit `crates/ferret/src/main.rs` for:

- App startup and shutdown behavior.
- Global actions and keybindings.
- Native menus.
- Window size, titlebar, background appearance, or application ID.
- Mounting the root GPUI view.

Keep product UI and reusable controls out of `main.rs`. The binary crate should
bootstrap the app, not become a second UI implementation.

```rust
actions!(ferret, [Quit]);

cx.on_action(|_: &Quit, cx| cx.quit());
cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
```

Ferret targets macOS and Windows. Every keybinding needs a Windows equivalent, and
anything touching paths, permissions, PTYs or the trash needs a plan for both
before it merges. Linux is deferred, not abandoned — keep platform differences
behind `ferret-platform` traits so re-adding it is not a rewrite.

## Testing and Validation

| Change | Minimum validation |
| --- | --- |
| Documentation only | Read the rendered Markdown/HTML and verify commands and paths. |
| Rust implementation | `just fmt` and `just check`. |
| Component behavior | Rust checks plus `just dev` and manual interaction. |
| Theme or layout | Rust checks plus visual review in light and dark, at default and minimum window sizes. |
| Window/bootstrap | Rust checks plus launch, quit shortcut, and window-behavior verification. |
| New logic or bug fix | Add focused tests where practical, then `just test`. |
| `ferret-vfs` or the file pane | The above plus `cargo xtask bench`, which must stay within budget. |
| Broad or release-facing work | All of the above plus `just lint` and `just build`. |

Good tests protect behavior rather than restating implementation details. Prefer
testing conversions, state transitions, parsing, and invariants over snapshotting
trivial builder chains. Filesystem and path-handling logic is the highest-value
place to add tests in this project — write them against a temp directory.

## Common Mistakes

- Putting reusable UI code in `crates/ferret/src/main.rs`.
- Making a UI module public instead of re-exporting a small API from `lib.rs`.
- Mutating view state without calling `cx.notify()`.
- Using duplicate or unstable IDs for clickable elements.
- Adding a `ferret-ui` theme token that duplicates one `cx.theme()` already provides.
- Updating `theme.rs` while forgetting the corresponding bundled palette, or vice versa.
- Hand-rolling a widget `gpui-component` already provides.
- Breaking the bootstrap contract above — every failure mode there is silent at compile time.
- Adding a `rev` to `gpui` without pinning `gpui-component` to a matching commit.
- Dropping `gpui_platform`'s `runtime_shaders` feature, which is what lets macOS build without full Xcode.
- Assuming macOS path, permission or shell semantics. Windows has drive letters, UNC paths, UTF-16 names, reserved names (`CON`, `NUL`, `COM1`) and needs Developer Mode for symlinks.
- Collecting a directory listing into a `Vec` before yielding it. `cargo xtask bench` exists to catch this; time-to-first-row is 0.3 ms against 143 ms for a full 200,000-entry listing, and that gap is the whole point.
- Adding a `ferret-vfs` type that duplicates something `std` already provides. It is a thin wrapper over the platform, not a filesystem.
- Declaring a bare `std::result::Result` in a crate that has its own alias, or hard-coding a magic number next to the code that reads it. Both have a designated home.
- Writing an error variant that does not name what it failed on.
- Running only `cargo check` and skipping visual verification for a visible change.
- Creating a new workspace crate before there is code to put in it.

## Commit and Pull Request Guidelines

Use short, imperative commit subjects, optionally scoped:

```text
ui: add git status column to the file list
ferret: refine quit keybindings
docs: expand the search plan
```

Keep commits focused. A pull request should include:

- **Motivation:** what problem or user need prompted the change.
- **Summary:** the important implementation decisions.
- **Validation:** exact commands and manual checks performed.
- **Visual evidence:** screenshots or a short recording for visible GPUI changes.
- **Follow-ups:** known limitations or intentionally deferred work.

Do not mix formatting churn, unrelated cleanup, and feature work in one change.

## Resources

- [`docs/ui-base.html`](docs/ui-base.html) — the base explorer shell specification
- [`docs/plan.html`](docs/plan.html) — Ferret's product and architecture plan
- [GPUI official site and guide](https://www.gpui.rs/)
- [GPUI API documentation](https://docs.rs/gpui/)
- [GPUI source in the Zed repository](https://github.com/zed-industries/zed/tree/main/crates/gpui)
- [Zed repository](https://github.com/zed-industries/zed) for production GPUI patterns
- [gpui-component](https://github.com/longbridge/gpui-component) and its [component gallery](https://longbridge.github.io/gpui-component)
- [The Rust Programming Language](https://doc.rust-lang.org/book/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)

When framework documentation and repository code disagree, prefer this
repository's locked dependency version and existing local patterns, then update
this guide if the project intentionally adopts a newer API.
