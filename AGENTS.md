# Repository Guidelines

This file explains how humans and coding agents should work in **Ferret**.

## Project at a Glance

Ferret is an open-source, cross-platform (Linux / macOS / Windows) file manager
with a terminal that stays synced to the folder you are browsing. It is a Rust
2024 workspace built on [GPUI](https://www.gpui.rs/) and
[gpui-component](https://github.com/longbridge/gpui-component).

**The repository is currently scaffolding.** It builds and opens a window; no
product feature is implemented. The product and architecture design lives in
[`docs/plan.html`](docs/plan.html) — read it before proposing structural work.

```text
.
├── Cargo.toml            # Workspace members, shared dependencies, profiles
├── justfile              # Development commands
├── crates/
│   ├── ferret/           # Native binary bootstrap  (bin `ferret`)
│   │   ├── assets/       # Application icon files
│   │   └── src/main.rs   # Application, menu, keybindings, window setup
│   └── ferret-ui/        # Views, components, theme
│       ├── src/lib.rs    # Public API and RootView
│       ├── src/button.rs # Button component and variants
│       ├── src/theme.rs  # Runtime theme tokens
│       └── themes/groknight.json
├── docs/plan.html        # Product + architecture plan
└── README.md
```

| Crate | Package | Responsibility |
| --- | --- | --- |
| `crates/ferret` | `ferret` | Starts GPUI, registers global actions, configures the native window, mounts the root view. |
| `crates/ferret-ui` | `ferret-ui` | Owns views, components, local UI state, styling, and theme tokens. |

**Ownership rule:** put behavior in the crate that owns it. Window and
application lifecycle code belongs in `ferret`; reusable visual behavior belongs
in `ferret-ui`. Expose cross-crate APIs through the owning crate's `src/lib.rs`
instead of reaching into private modules.

As features land, `ferret-ui` splits into the service crates described in
`docs/plan.html` (`ferret-vfs`, `ferret-terminal`, `ferret-git`, …). Do not create
those crates speculatively — add one when the code that belongs in it exists.
When you do, respect the layering: no service crate may depend on `ferret-ui`,
and only `ferret-ui` may depend on `gpui-component`.

## Quick Start

### Prerequisites

- A current stable Rust toolchain (1.90+) through [rustup](https://rustup.rs/).
- [`just`](https://github.com/casey/just) for the repository shortcuts (optional).
- The native build tools required by Rust and GPUI for your operating system.
  macOS needs the Xcode Command Line Tools; Linux needs a Vulkan loader,
  `libxkbcommon` and `fontconfig`; Windows needs the MSVC toolchain.

### Run the application

```sh
just dev          # or: cargo run -p ferret
```

The app opens a `1180 × 760` window. Quit with **⌘Q** on macOS or **Ctrl+Q**
elsewhere.

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

## Advanced GPUI Guidance

### Mental model

- `RootView` owns application UI state.
- `impl Render for RootView` derives the current element tree from that state.
- Reusable, value-like components such as `Button` use `#[derive(IntoElement)]` and `RenderOnce`.
- Event handlers that mutate a view should be created with `cx.listener(...)`.
- After mutating render-relevant state, call `cx.notify()` to request another render.
- Stable element IDs are important for interactive elements.
- The root fill remains transparent because the window uses `WindowBackgroundAppearance::Blurred`.

### State update example

```rust
Button::new("save-button", "Save")
    .theme(theme)
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

Runtime theme values live in `crates/ferret-ui/src/theme.rs`. The palette in
`crates/ferret-ui/themes/groknight.json` is bundled/reference theme data and
mirrors the same colors. If a shared palette color changes, check whether both
files should stay aligned.

Preserve these visual constraints unless the task explicitly changes the design:

- `Theme::window_fill()` must remain fully transparent for system blur to show through.
- Chrome surfaces use translucent colors rather than opaque replacements.
- Components should use `Theme` tokens instead of scattering unrelated color literals.
- Interactive components should define normal, hover, active, and disabled states when applicable.
- Text and controls should retain readable contrast on the blurred background.

Ferret ships a light theme as well as a dark one. Any new token must be defined
for both; never hard-code a color that only reads correctly in one appearance.

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

Ferret targets three platforms. Every keybinding needs a non-macOS equivalent,
and anything touching paths, permissions, PTYs or the trash needs a plan for all
three before it merges.

## Testing and Validation

| Change | Minimum validation |
| --- | --- |
| Documentation only | Read the rendered Markdown/HTML and verify commands and paths. |
| Rust implementation | `just fmt` and `just check`. |
| Component behavior | Rust checks plus `just dev` and manual interaction. |
| Theme or layout | Rust checks plus visual review in light and dark, at default and minimum window sizes. |
| Window/bootstrap | Rust checks plus launch, quit shortcut, and window-behavior verification. |
| New logic or bug fix | Add focused tests where practical, then `just test`. |
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
- Replacing transparent window fills with opaque backgrounds and disabling blur.
- Updating `theme.rs` while forgetting the corresponding bundled palette, or vice versa.
- Hand-rolling a widget `gpui-component` already provides.
- Adding a `rev` to `gpui` without pinning `gpui-component` to a matching commit.
- Assuming macOS path, permission or shell semantics.
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
