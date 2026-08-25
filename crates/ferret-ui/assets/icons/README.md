# Bundled file icons

`file/*.svg` is a curated subset of the
[Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme),
used under the MIT licence — see `LICENSE-material-icon-theme`. MIT is compatible
with Ferret's GPL-3.0; the licence and copyright notice must stay with the files.

`file-icons.json` maps filenames, extensions and directory names to icon names.
An icon name is the SVG stem: `rust` means `file/rust.svg`.

## What this is for

This is the **fallback** set, not the final answer. Per D-18, Ferret's real icons
come from the OS — `NSWorkspace.icon(forFile:)` on macOS,
`SHGetFileInfo` / `IShellItemImageFactory` on Windows — because a Finder
replacement whose icons do not match Finder looks wrong on sight, and only the OS
knows about application bundles, custom folder icons and document thumbnails.

The bundled set exists so that lookup never blocks. The `IconProvider` answers
instantly from this table, and an OS icon replaces it when it resolves in the
background. **Never call the OS on the render path** — at 200,000 rows that is
the difference between scrolling and not.

## Lookup order

1. Exact filename (`Cargo.toml`, `Dockerfile`, `.gitignore`)
2. Extension, lowercased (`rs`, `png`) — note `archive.tar.gz` resolves as `gz`
3. Directory name, for directories only (`src`, `node_modules`)
4. The kind default (`document`, `folder-base`, `url`, `binary`)

## Updating

Icons are vendored deliberately rather than fetched at build time, so the build
stays offline and reproducible. To add one, copy the SVG from the upstream
repository into `file/` and add its mapping to `file-icons.json`.

The SVGs carry their own fills, so they are drawn as images rather than tinted to
the current text colour. That is intentional — the colour is what makes a Rust
file distinguishable from a Python file at a glance.
