# Ferret

An open-source, cross-platform file manager with a terminal that follows you.

Ferret is for people who live between a file browser and a shell. Navigate in the
UI and the embedded terminal is already in that directory — no `cd`. Add SSH and
SFTP mounts, git status in the file list, grep-powered search, and real previews
for Markdown, source code, images and PDFs.

Targets **Linux, macOS and Windows**, built in Rust on
[GPUI](https://www.gpui.rs/) with [gpui-component](https://github.com/longbridge/gpui-component).

> **Status: scaffolding.** The workspace builds and opens a window; none of the
> product features are implemented yet. The full design lives in
> **[`docs/plan.html`](docs/plan.html)** — open it in a browser.

## Workspace

```
crates/
  ferret/      # native binary: bootstrap, window, menus, actions  (bin `ferret`)
  ferret-ui/   # views, components, theme tokens                   (lib `ferret-ui`)
docs/
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
| macOS | Xcode Command Line Tools. |
| Linux | Wayland and X11 backends are both enabled; needs the usual GPUI dev packages (Vulkan loader, `libxkbcommon`, `fontconfig`). |
| Windows | MSVC toolchain. |

## Stack

- [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) + `gpui_platform`, tracked from Zed's Git repository and pinned by `Cargo.lock`.
- [gpui-component](https://github.com/longbridge/gpui-component) for tables, docks, inputs, Markdown and syntax highlighting.

## Licence

GPL-3.0-or-later.
