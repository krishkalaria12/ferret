# Ferret

An open-source, cross-platform file manager with a terminal that follows you.

Ferret is for people who live between a file browser and a shell. Navigate in the
UI and the embedded terminal is already in that directory — no `cd`. Add SSH and
SFTP mounts, git status in the file list, grep-powered search, and real previews
for Markdown, source code, images and PDFs.

Targets **macOS and Windows** (Linux deferred, not abandoned), built in Rust on
[GPUI](https://www.gpui.rs/) with [gpui-component](https://github.com/longbridge/gpui-component).

> **Status: scaffolding.** The workspace builds and opens a window; none of the
> product features are implemented yet.
>
> - **[`docs/ui-base.html`](docs/ui-base.html)** — the base explorer shell: mockup,
>   geometry, component map, tokens, interaction contract. **Build this first.**
> - **[`docs/plan.html`](docs/plan.html)** — the whole product: 51 features, target
>   architecture, platform hazards, decisions, phases.
>
> Open either in a browser.

## Workspace

```
crates/
  ferret/           # native binary: bootstrap, window, menus, actions
  ferret-ui/        # views, theme tokens, bundled file icons
  ferret-vfs/       # the Vfs trait, local backend, path handling
  ferret-settings/  # settings schema, keymap, per-folder view preferences
xtask/              # fixture generator and listing benchmark
docs/
  ui-base.html # base explorer shell — the UI reference to build against
  plan.html    # product + architecture plan
```

`docs/plan.html` describes the target architecture the workspace grows into.
`ferret-core` does not exist yet — the shared domain types live in `ferret-vfs`
until a second service crate needs them.

## Settings

A human-editable JSON file at `~/Library/Application Support/Ferret/settings.json`
(macOS) or `%APPDATA%\Ferret\settings.json` (Windows). Every key is optional, so
a partial file works and deleting it is always safe. See
`crates/ferret-settings/default-settings.json` for the full shape.

## Build

Requires a stable Rust toolchain (1.90+) and the native build tools GPUI needs on
your platform. [`just`](https://github.com/casey/just) is optional.

```sh
just dev          # cargo run -p ferret
just run          # release
just check        # cargo check --workspace --all-targets
just test         # cargo test --workspace
just lint         # fmt --check + clippy -D warnings
```

### Performance fixture

Correctness and performance are measured against a generated tree, not against
whatever happens to be on your disk:

```sh
cargo xtask fixture   # build it (~25 s; 200,000 files in flat/ alone)
cargo xtask bench     # measure; exits non-zero past budget
cargo xtask clean     # remove it
```

It covers 200,000 flat entries, maximal nesting, 255-character and NFD and emoji
names, broken and cyclic symlinks, an unreadable directory, a 4 GB sparse file and
59 extensions. It is built outside the repository so Cargo and editor
file-watchers do not crawl it.

| Measure | Budget | Actual (M-series, APFS) |
| --- | --- | --- |
| Time to first row, 200k entries | 50 ms | 0.3 ms |
| Full listing, 200k entries | 3 s | 143 ms |

Quit with **⌘Q** / **Ctrl+Q**.

### Platform notes

| Platform | Notes |
| --- | --- |
| macOS | Xcode Command Line Tools are enough — see below. |
| Windows | MSVC toolchain. Symlink creation needs Developer Mode or an elevated shell. |

Linux is deferred rather than abandoned — `ferret-platform`'s trait boundary stays
intact so it is a re-add, not a rewrite.

Building GPUI on macOS normally needs the Metal shader compiler, which ships only
inside **full Xcode** — with Command Line Tools alone, `gpui_macos`'s build script
fails with `xcrun: error: unable to find utility "metal"`. Ferret enables
`gpui_platform`'s `runtime_shaders` feature, which stitches the shaders and compiles
them at runtime instead, so Command Line Tools suffice. If you remove that feature,
you need full Xcode and `sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer`.

## Stack

- [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) + `gpui_platform`, tracked from Zed's Git repository and pinned by `Cargo.lock`.
- [gpui-component](https://github.com/longbridge/gpui-component) for tables, docks, inputs, Markdown and syntax highlighting.

The bootstrap in `crates/ferret/src/main.rs` follows `gpui-component`'s required
setup, all four parts of which are load-bearing:

1. `.with_assets(Assets)` — registers the SVGs behind `IconName`; without it every icon is blank.
2. `gpui_component::init(cx)` — must run before any component type is constructed.
3. The window opens from a spawned task.
4. The window's first entity is a `Root`, which hosts the layers that dialogs, popovers, notifications and tooltips render into.

## Licence

GPL-3.0-or-later.
