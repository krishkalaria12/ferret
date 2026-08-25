# Ferret

An open-source, cross-platform file manager with a terminal that follows you.

Ferret is for people who live between a file browser and a shell. Navigate in the
UI and the embedded terminal is already in that directory — no `cd`. Add SSH and
SFTP mounts, git status in the file list, grep-powered search, and real previews
for Markdown, source code, images and PDFs.

Targets **Linux, macOS and Windows**, built in Rust on
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
  ferret/      # native binary: bootstrap, window, menus, actions  (bin `ferret`)
  ferret-ui/   # views and Ferret-specific theme tokens            (lib `ferret-ui`)
docs/
  ui-base.html # base explorer shell — the UI reference to build against
  plan.html    # product + architecture plan
```

The two-crate layout is the starting point. `docs/plan.html` describes the target
architecture this splits into as features land.

## Build

Requires a stable Rust toolchain (1.90+) and the native build tools GPUI needs on
your platform. [`just`](https://github.com/casey/just) is optional.

```sh
just dev          # cargo run -p ferret
just run          # release
just check        # cargo check --workspace --all-targets
just lint         # fmt --check + clippy -D warnings
```

Quit with **⌘Q** / **Ctrl+Q**.

### Platform notes

| Platform | Notes |
| --- | --- |
| macOS | Xcode Command Line Tools are enough — see below. |
| Linux | Wayland and X11 backends are both enabled; needs the usual GPUI dev packages (Vulkan loader, `libxkbcommon`, `fontconfig`). |
| Windows | MSVC toolchain. |

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
