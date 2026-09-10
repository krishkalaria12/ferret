//! Identifying a language, which is a different job from highlighting it.
//!
//! This table resolves a file *name* to a language *name* ("rust", "nix").
//! Some of those names have a tree-sitter grammar behind them and some do
//! not — yet. A file whose language is known but unhighlighted previews as
//! labelled monospaced text, never as "unsupported binary". Coverage of the
//! header is total; coverage of colour is finite and honest.
//!
//! The grammars themselves live in `ferret-ui` (`LanguageRegistry` is a
//! `gpui-component` type, so they have to). The two halves are kept honest by
//! one test over there: every name emitted here must either resolve in the
//! registry or appear in an explicit `IDENTIFIED_BUT_UNHIGHLIGHTED` list.

/// Exact filenames, checked before extensions.
///
/// This is the case a pure extension map gets wrong every time, and it covers
/// a lot of what sits in a repository root. Public so classification can
/// check exact names before magic bytes, and magic bytes before extensions.
pub fn filename_language(name: &str) -> Option<&'static str> {
    Some(match name {
        "Makefile" | "makefile" | "GNUmakefile" | "makefile.am" => "make",
        "Dockerfile" | "Containerfile" => "dockerfile",
        "Justfile" | "justfile" => "just",
        "CMakeLists.txt" => "cmake",
        "Cargo.lock" => "toml",
        "go.mod" | "go.sum" => "go",
        ".gitignore" | ".gitattributes" | ".gitmodules" => "gitignore",
        ".env" | ".env.local" | ".env.example" => "dotenv",
        "Gemfile" | "Rakefile" | "Vagrantfile" => "ruby",
        "Pipfile" => "toml",
        _ => return None,
    })
}

/// Every language name the tables below can emit.
///
/// The UI's registry invariant test walks this list: each name must either
/// resolve in the `LanguageRegistry` or appear in an explicit
/// `IDENTIFIED_BUT_UNHIGHLIGHTED` list there. Keep in sync with
/// `filename_language` and `extension_language_lower` below — a name added
/// to a table but not here silently escapes the invariant.
pub const ALL_NAMES: &[&str] = &[
    "actionscript",
    "ada",
    "applescript",
    "arduino",
    "asm",
    "astro",
    "autoit",
    "awk",
    "bash",
    "batch",
    "c",
    "c_sharp",
    "clojure",
    "cmake",
    "cobra",
    "cpp",
    "crystal",
    "css",
    "csv",
    "d",
    "dart",
    "diff",
    "dockerfile",
    "dotenv",
    "ejs",
    "elixir",
    "elm",
    "erb",
    "erlang",
    "fish",
    "forth",
    "fortran",
    "gitignore",
    "gleam",
    "glsl",
    "go",
    "graphql",
    "groovy",
    "haskell",
    "hcl",
    "html",
    "ini",
    "janet",
    "java",
    "javascript",
    "json",
    "jsonnet",
    "julia",
    "just",
    "kdl",
    "kotlin",
    "latex",
    "lidr",
    "log",
    "lua",
    "make",
    "markdown",
    "nginx",
    "nim",
    "nix",
    "nushell",
    "ocaml",
    "odin",
    "pascal",
    "perl",
    "php",
    "plain",
    "pony",
    "powershell",
    "prisma",
    "proto",
    "purescript",
    "python",
    "r",
    "racket",
    "rego",
    "roc",
    "ruby",
    "rust",
    "scala",
    "sed",
    "slang",
    "sml",
    "solidity",
    "sql",
    "svelte",
    "swift",
    "swig",
    "tcl",
    "toml",
    "tsv",
    "tsx",
    "typescript",
    "typst",
    "v",
    "vala",
    "vbscript",
    "vim",
    "vue",
    "wgsl",
    "wren",
    "xml",
    "yaml",
    "yuck",
    "zenscript",
    "zig",
];

/// Resolve a file name to a language name.
///
/// Lowercases once, the way the icon layer already established: an extension
/// is nearly always short enough that the common case costs nothing. Returns
/// `None` when nothing is known, which the caller renders as `Plain` — a
/// language, not an absence of one.
pub fn language_for(file_name: &str) -> Option<&'static str> {
    if let Some(language) = filename_language(file_name) {
        return Some(language);
    }
    // A leading dot makes a hidden file, not an extension: `.gitignore` is
    // handled above, `.env.local` resolves by its real extension below.
    let (_, ext) = file_name.trim_start_matches('.').rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    extension_language(ext)
}

/// Whether this extension needs lowercasing before the table sees it.
///
/// Already-lowercase names — nearly all of them — match without touching the
/// heap; only the rare `IMAGE.PNG` pays for a fold.
fn extension_language(ext: &str) -> Option<&'static str> {
    if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        return extension_language_lower(&ext.to_ascii_lowercase());
    }
    extension_language_lower(ext)
}

fn extension_language_lower(ext: &str) -> Option<&'static str> {
    Some(match ext {
        // Systems and compiled languages.
        "rs" => "rust",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        "cs" => "c_sharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "scala" | "sc" => "scala",
        "zig" => "zig",
        "s" | "asm" => "asm",
        "f" | "for" | "f90" | "f95" => "fortran",
        "adb" | "ads" => "ada",
        // Scripting.
        "py" | "pyw" | "pyi" => "python",
        "rb" | "rake" | "gemspec" => "ruby",
        "php" => "php",
        "pl" | "pm" | "t" => "perl",
        "lua" => "lua",
        "sh" | "bash" | "zsh" | "ksh" | "mksh" => "bash",
        "fish" => "fish",
        "ps1" | "psm1" | "psd1" => "powershell",
        "bat" | "cmd" => "batch",
        "awk" => "awk",
        "sed" => "sed",
        "tcl" => "tcl",
        "vbs" | "vba" => "vbscript",
        // Functional and friends.
        "hs" | "lhs" => "haskell",
        "ml" | "mli" => "ocaml",
        "erl" | "hrl" => "erlang",
        "ex" | "exs" => "elixir",
        "clj" | "cljs" | "cljc" | "edn" => "clojure",
        "jl" => "julia",
        "r" => "r",
        "elm" => "elm",
        "gleam" => "gleam",
        // Web.
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "html" | "htm" | "xhtml" => "html",
        "css" => "css",
        "scss" | "sass" | "less" => "css",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "ejs" => "ejs",
        "erb" => "erb",
        // Data and config.
        "json" | "jsonc" | "json5" | "webmanifest" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" | "xsl" | "xsd" | "svg" | "plist" | "csproj" | "vcxproj" => "xml",
        "ini" | "cfg" | "conf" | "editorconfig" => "ini",
        "env" => "dotenv",
        "hcl" | "tf" | "tfvars" => "hcl",
        "nix" => "nix",
        "prisma" => "prisma",
        "jsonnet" | "libsonnet" => "jsonnet",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "sql" => "sql",
        "md" | "markdown" | "mdown" | "mkd" => "markdown",
        "tex" | "sty" | "cls" => "latex",
        "vim" | "vimrc" => "vim",
        "nginx" | "nginxconf" => "nginx",
        "wgsl" => "wgsl",
        "glsl" | "vert" | "frag" | "geom" | "comp" => "glsl",
        "sol" => "solidity",
        "dart" => "dart",
        "groovy" | "gradle" => "groovy",
        "cmake" => "cmake",
        "make" | "mak" | "mk" => "make",
        "diff" | "patch" => "diff",
        "dockerfile" => "dockerfile",
        "just" => "just",
        // Identified only: a correct label, no grammar (yet). These render as
        // plain monospaced text with the right name in the header.
        "applescript" | "scpt" => "applescript",
        "as" => "actionscript",
        "au3" => "autoit",
        "cobra" => "cobra",
        "cr" => "crystal",
        "d" => "d",
        "dpr" | "pas" => "pascal",
        "forth" | "fth" => "forth",
        "ino" | "pde" => "arduino",
        "janet" => "janet",
        "kdl" => "kdl",
        "lidr" => "lidr",
        "nim" | "nims" => "nim",
        "nu" => "nushell",
        "odin" => "odin",
        "pony" => "pony",
        "purs" => "purescript",
        "racket" | "rkt" => "racket",
        "rego" => "rego",
        "roc" => "roc",
        "slang" => "slang",
        "sml" => "sml",
        "swig" | "i" => "swig",
        "typ" => "typst",
        "v" => "v",
        "vala" | "vapi" => "vala",
        "wren" => "wren",
        "yuck" => "yuck",
        "zenscript" | "zs" => "zenscript",
        "zsh-theme" => "bash",
        "gitignore" | "gitattributes" | "gitmodules" => "gitignore",
        "log" => "log",
        "txt" | "text" => "plain",
        "csv" => "csv",
        "tsv" | "tab" => "tsv",
        _ => return None,
    })
}

/// The header label for a language name.
///
/// The name is a registry key ("c_sharp"); the header wants the words a
/// person would use ("C#"). Unlisted names come back unchanged — an honest
/// label beats a clever one nobody maintains.
pub fn display_name(language: &str) -> &str {
    match language {
        "c_sharp" => "C#",
        "cpp" => "C++",
        "c" => "C",
        "plain" => "Plain text",
        "log" => "Log",
        "markdown" => "Markdown",
        "bash" => "Shell",
        "batch" => "Batch",
        "dockerfile" => "Dockerfile",
        "make" => "Makefile",
        "cmake" => "CMake",
        "gitignore" => "Git ignore",
        "dotenv" => "Environment",
        "javascript" => "JavaScript",
        "typescript" => "TypeScript",
        "tsx" => "TSX",
        "python" => "Python",
        "rust" => "Rust",
        "go" => "Go",
        "java" => "Java",
        "kotlin" => "Kotlin",
        "swift" => "Swift",
        "scala" => "Scala",
        "ruby" => "Ruby",
        "php" => "PHP",
        "perl" => "Perl",
        "lua" => "Lua",
        "zig" => "Zig",
        "haskell" => "Haskell",
        "ocaml" => "OCaml",
        "erlang" => "Erlang",
        "elixir" => "Elixir",
        "clojure" => "Clojure",
        "julia" => "Julia",
        "r" => "R",
        "dart" => "Dart",
        "html" => "HTML",
        "css" => "CSS",
        "json" => "JSON",
        "yaml" => "YAML",
        "toml" => "TOML",
        "xml" => "XML",
        "ini" => "INI",
        "hcl" => "HCL",
        "nix" => "Nix",
        "sql" => "SQL",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "astro" => "Astro",
        "proto" => "Protocol Buffers",
        "graphql" => "GraphQL",
        "latex" => "LaTeX",
        "vim" => "Vim script",
        "nginx" => "Nginx",
        "wgsl" => "WGSL",
        "glsl" => "GLSL",
        "solidity" => "Solidity",
        "groovy" => "Groovy",
        "powershell" => "PowerShell",
        "fish" => "Fish",
        "fortran" => "Fortran",
        "ada" => "Ada",
        "asm" => "Assembly",
        "elm" => "Elm",
        "gleam" => "Gleam",
        "just" => "Just",
        "diff" => "Diff",
        "csv" => "CSV",
        "tsv" => "TSV",
        "applescript" => "AppleScript",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_filenames_win_over_extensions() {
        // `CMakeLists.txt` ends in `.txt` — a pure extension map calls it
        // plain text, and a repository root is full of these.
        assert_eq!(language_for("CMakeLists.txt"), Some("cmake"));
        assert_eq!(language_for("Makefile"), Some("make"));
        assert_eq!(language_for("Dockerfile"), Some("dockerfile"));
        assert_eq!(language_for("Justfile"), Some("just"));
        assert_eq!(language_for("Cargo.lock"), Some("toml"));
        assert_eq!(language_for("go.sum"), Some("go"));
        assert_eq!(language_for(".gitignore"), Some("gitignore"));
        assert_eq!(language_for(".env"), Some("dotenv"));
    }

    #[test]
    fn extensions_resolve_case_insensitively() {
        assert_eq!(language_for("main.rs"), Some("rust"));
        assert_eq!(language_for("MAIN.RS"), Some("rust"));
        assert_eq!(language_for("IMAGE.PNG"), None, "images are not languages");
        assert_eq!(language_for("app.TSX"), Some("tsx"));
    }

    #[test]
    fn identified_only_languages_get_a_label() {
        // PV-D06: an `.applescript` file previews as monospaced text labelled
        // AppleScript, not as "unsupported binary".
        assert_eq!(language_for("script.applescript"), Some("applescript"));
        assert_eq!(display_name("applescript"), "AppleScript");
    }

    #[test]
    fn all_names_covers_the_tables() {
        // The UI invariant test trusts ALL_NAMES to be the tables' full
        // output. This ties the two together for a sample across every
        // region of the tables; extend it when the tables grow.
        for (file, name) in [
            ("Makefile", "make"),
            ("Dockerfile", "dockerfile"),
            (".gitignore", "gitignore"),
            ("main.rs", "rust"),
            ("app.tsx", "tsx"),
            ("query.sql", "sql"),
            ("site.vue", "vue"),
            ("flake.nix", "nix"),
            ("main.tf", "hcl"),
            ("notes.md", "markdown"),
            ("data.csv", "csv"),
            ("script.applescript", "applescript"),
            ("photo.png", "plain"),
        ] {
            assert_eq!(language_for(file).or(Some("plain")), Some(name));
            assert!(
                ALL_NAMES.contains(&name),
                "{name} (from {file}) is missing from ALL_NAMES"
            );
        }
    }

    #[test]
    fn unknown_files_have_no_language() {
        assert_eq!(language_for("Makefile2"), None);
        assert_eq!(language_for("noextension"), None);
        assert_eq!(language_for("trailing."), None);
        assert_eq!(language_for(".hidden"), None);
    }

    #[test]
    fn registry_keys_have_human_labels() {
        assert_eq!(display_name("c_sharp"), "C#");
        assert_eq!(display_name("plain"), "Plain text");
        // Unlisted names pass through rather than breaking.
        assert_eq!(display_name("yuck"), "yuck");
    }
}
