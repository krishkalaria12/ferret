//! Extended tree-sitter grammars (PV-D05, Q-02).
//!
//! The built-ins behind `gpui-component`'s per-grammar features are always
//! on. These 15 register through the public `LanguageRegistry` — one
//! `register` call each — behind the default-on `languages-extended`
//! feature, so `--no-default-features` still highlights the built-ins.
//! Registration is a deterministic, duplicate-checked pass before the first
//! preview, not lazy per language: lazily, the first file of a type renders
//! unhighlighted and the second does not.
//!
//! Each entry records its crate version and query provenance in the vendored
//! file's header (`assets/queries/`). Queries are vendored, never trusted
//! from the package layout, with 41-slot adaptations noted per file.
//! Injection and locals queries are deliberately `""` for now:
//! embedded-language injections need explicit adaptation per language, which
//! is a follow-up, not a silent half-wire.
//!
//! Kept out (identified-only, with reasons in the workspace table): `just`
//! and `fish`, whose crates link `tree-sitter` 0.25/0.22 directly — only one
//! native `tree-sitter` may link into the binary. `dockerfile` and `vue`
//! arrive via maintained forks for the same class of reason.

#[cfg(feature = "languages-extended")]
use gpui_component::highlighter::{LanguageConfig, LanguageRegistry};

/// One extended language: its registry name, its parser, its query.
#[cfg(feature = "languages-extended")]
struct ExtendedLanguage {
    name: &'static str,
    language: fn() -> tree_sitter::Language,
    highlights: &'static str,
}

/// Shims for crates exposing `LANGUAGE: LanguageFn`.
///
/// The handle is runtime-version-free, so the grammar's own `tree-sitter`
/// requirement is irrelevant — what matters is the generated parser's ABI,
/// which the fixture test below proves per language.
#[cfg(feature = "languages-extended")]
macro_rules! shim {
    ($fn_name:ident, $krate:ident) => {
        fn $fn_name() -> tree_sitter::Language {
            tree_sitter::Language::new($krate::LANGUAGE)
        }
    };
}

#[cfg(feature = "languages-extended")]
shim!(dockerfile, tree_sitter_containerfile);
#[cfg(feature = "languages-extended")]
shim!(nix, tree_sitter_nix);
#[cfg(feature = "languages-extended")]
shim!(hcl, tree_sitter_hcl);
#[cfg(feature = "languages-extended")]
shim!(ini, tree_sitter_ini);
#[cfg(feature = "languages-extended")]
shim!(dart, tree_sitter_dart);
#[cfg(feature = "languages-extended")]
shim!(powershell, tree_sitter_powershell);
#[cfg(feature = "languages-extended")]
shim!(haskell, tree_sitter_haskell);
#[cfg(feature = "languages-extended")]
shim!(erlang, tree_sitter_erlang);
#[cfg(feature = "languages-extended")]
shim!(julia, tree_sitter_julia);
#[cfg(feature = "languages-extended")]
shim!(r, tree_sitter_r);
#[cfg(feature = "languages-extended")]
shim!(perl, tree_sitter_perl);
#[cfg(feature = "languages-extended")]
shim!(groovy, tree_sitter_groovy);
#[cfg(feature = "languages-extended")]
shim!(vue, tree_sitter_vue_next);

/// OCaml ships three parsers; the implementation grammar covers both
/// extensions our table emits (`.ml`, `.mli`) for this pass.
#[cfg(feature = "languages-extended")]
fn ocaml() -> tree_sitter::Language {
    tree_sitter::Language::new(tree_sitter_ocaml::LANGUAGE_OCAML)
}

/// XML ships DTD beside it; only the XML parser registers for now.
#[cfg(feature = "languages-extended")]
fn xml() -> tree_sitter::Language {
    tree_sitter::Language::new(tree_sitter_xml::LANGUAGE_XML)
}

/// The extended set, in registration order.
///
/// `name` is the same registry key `ferret_preview::languages` emits, which
/// is what makes the editor and Markdown fences agree with the header.
#[cfg(feature = "languages-extended")]
const EXTENDED: &[ExtendedLanguage] = &[
    ExtendedLanguage {
        name: "dockerfile",
        language: dockerfile,
        highlights: include_str!("../../assets/queries/dockerfile.scm"),
    },
    ExtendedLanguage {
        name: "nix",
        language: nix,
        highlights: include_str!("../../assets/queries/nix.scm"),
    },
    ExtendedLanguage {
        name: "hcl",
        language: hcl,
        highlights: include_str!("../../assets/queries/hcl.scm"),
    },
    ExtendedLanguage {
        name: "vue",
        language: vue,
        highlights: include_str!("../../assets/queries/vue.scm"),
    },
    ExtendedLanguage {
        name: "xml",
        language: xml,
        highlights: include_str!("../../assets/queries/xml.scm"),
    },
    ExtendedLanguage {
        name: "ini",
        language: ini,
        highlights: include_str!("../../assets/queries/ini.scm"),
    },
    ExtendedLanguage {
        name: "dart",
        language: dart,
        highlights: include_str!("../../assets/queries/dart.scm"),
    },
    ExtendedLanguage {
        name: "powershell",
        language: powershell,
        highlights: include_str!("../../assets/queries/powershell.scm"),
    },
    ExtendedLanguage {
        name: "haskell",
        language: haskell,
        highlights: include_str!("../../assets/queries/haskell.scm"),
    },
    ExtendedLanguage {
        name: "ocaml",
        language: ocaml,
        highlights: include_str!("../../assets/queries/ocaml.scm"),
    },
    ExtendedLanguage {
        name: "erlang",
        language: erlang,
        highlights: include_str!("../../assets/queries/erlang.scm"),
    },
    ExtendedLanguage {
        name: "julia",
        language: julia,
        highlights: include_str!("../../assets/queries/julia.scm"),
    },
    ExtendedLanguage {
        name: "r",
        language: r,
        highlights: include_str!("../../assets/queries/r.scm"),
    },
    ExtendedLanguage {
        name: "perl",
        language: perl,
        highlights: include_str!("../../assets/queries/perl.scm"),
    },
    ExtendedLanguage {
        name: "groovy",
        language: groovy,
        highlights: include_str!("../../assets/queries/groovy.scm"),
    },
];

/// Register every extended language, duplicate-checked.
///
/// A name already present would mean a grammar added without removing its
/// placeholder — fail loudly in debug rather than silently shadowing.
#[cfg(feature = "languages-extended")]
fn register_extended() {
    let registry = LanguageRegistry::singleton();
    for lang in EXTENDED {
        debug_assert!(
            registry.language(lang.name).is_none(),
            "{} is already registered",
            lang.name
        );
        registry.register(
            lang.name,
            &LanguageConfig::new(
                lang.name,
                (lang.language)(),
                vec![],
                lang.highlights,
                "",
                "",
            ),
        );
    }
}

/// Idempotent entry point: the pane calls this before its first preview.
pub(crate) fn ensure_registered() {
    #[cfg(feature = "languages-extended")]
    {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(register_extended);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures that never style anything by design: the explicit opt-out,
    /// spell-check directives, and error nodes (plain text until error
    /// styling exists). Anything else must resolve to a theme slot.
    const DIRECTIVES: &[&str] = &["none", "spell", "nospell", "error"];

    /// Language names with no grammar in this build.
    ///
    /// With `languages-extended` on, the 15 extended names resolve and must NOT
    /// be here; with it off, they must be. Everything else on this list is an
    /// identified-only label (PV-D06): correct header, plain monospaced body.
    #[cfg(feature = "languages-extended")]
    const IDENTIFIED_BUT_UNHIGHLIGHTED: &[&str] = &[
        "actionscript",
        "ada",
        "applescript",
        "arduino",
        "asm",
        "autoit",
        "awk",
        "batch",
        "clojure",
        "cobra",
        "crystal",
        "csv",
        "d",
        "dotenv",
        "elm",
        "fish",
        "fortran",
        "forth",
        "gitignore",
        "gleam",
        "glsl",
        "janet",
        "jsonnet",
        "just",
        "kdl",
        "latex",
        "lidr",
        "log",
        "nginx",
        "nim",
        "nushell",
        "odin",
        "pascal",
        "pony",
        "prisma",
        "purescript",
        "racket",
        "rego",
        "roc",
        "sed",
        "slang",
        "sml",
        "solidity",
        "sql",
        "swig",
        "tcl",
        "tsv",
        "typst",
        "v",
        "vala",
        "vbscript",
        "vim",
        "wgsl",
        "wren",
        "yuck",
        "zenscript",
    ];

    /// Same list for builds without the extended set.
    #[cfg(not(feature = "languages-extended"))]
    const IDENTIFIED_BUT_UNHIGHLIGHTED_OFF: &[&str] = &[
        "actionscript",
        "ada",
        "applescript",
        "arduino",
        "asm",
        "autoit",
        "awk",
        "batch",
        "clojure",
        "cobra",
        "crystal",
        "csv",
        "d",
        "dart",
        "dockerfile",
        "dotenv",
        "elm",
        "erlang",
        "fish",
        "fortran",
        "forth",
        "gitignore",
        "gleam",
        "glsl",
        "groovy",
        "haskell",
        "hcl",
        "ini",
        "janet",
        "jsonnet",
        "julia",
        "just",
        "kdl",
        "latex",
        "lidr",
        "log",
        "nginx",
        "nim",
        "nix",
        "nushell",
        "ocaml",
        "odin",
        "pascal",
        "perl",
        "pony",
        "powershell",
        "prisma",
        "purescript",
        "r",
        "racket",
        "rego",
        "roc",
        "sed",
        "slang",
        "sml",
        "solidity",
        "sql",
        "swig",
        "tcl",
        "tsv",
        "typst",
        "v",
        "vala",
        "vbscript",
        "vim",
        "vue",
        "wgsl",
        "wren",
        "xml",
        "yuck",
        "zenscript",
    ];

    /// One fixture per extended language: representative source plus the
    /// floor for meaningful highlight spans.
    #[cfg(feature = "languages-extended")]
    const FIXTURES: &[(&str, &str, usize)] = &[
        (
            "dockerfile",
            "FROM ubuntu:22.04\n# comment\nRUN apt-get update\nENV FOO=bar\n",
            2,
        ),
        (
            "nix",
            "{ stdenv }: stdenv.mkDerivation { name = \"hi\"; }\n",
            4,
        ),
        (
            "hcl",
            "resource \"aws_instance\" \"web\" {\n ami = \"ami-123\"\n}\n",
            4,
        ),
        ("vue", "<template>\n <div>{{ msg }}</div>\n</template>\n", 4),
        (
            "xml",
            "<?xml version=\"1.0\"?>\n<root attr=\"v\"><!-- c --><a/></root>\n",
            4,
        ),
        ("ini", "; comment\n[section]\nkey = value\n", 2),
        ("dart", "void main() {\n // hi\n print('yo');\n}\n", 4),
        (
            "powershell",
            "# comment\nfunction Get-X {\n param($Name)\n Write-Host \"hi $Name\"\n}\n",
            4,
        ),
        (
            "haskell",
            "module Main where\n-- comment\nmain :: IO ()\nmain = putStrLn \"hi\"\n",
            4,
        ),
        (
            "ocaml",
            "(* comment *)\nlet x = 42\nlet add a b = a + b\n",
            4,
        ),
        (
            "erlang",
            "% comment\n-module(hi).\n-export([t/0]).\nt() -> ok.\n",
            4,
        ),
        ("julia", "# comment\nfunction f(x)\n return x + 1\nend\n", 4),
        ("r", "# comment\nf <- function(x) {\n x + 1\n}\n", 4),
        ("perl", "# comment\nmy $x = 42;\nprint \"hi $x\\n\";\n", 4),
        (
            "groovy",
            "// comment\ndef greet(String name) {\n println \"hi\"\n}\n",
            4,
        ),
    ];

    /// The active list for this build configuration.
    #[cfg(feature = "languages-extended")]
    const ACTIVE_LIST: &[&str] = IDENTIFIED_BUT_UNHIGHLIGHTED;
    #[cfg(not(feature = "languages-extended"))]
    const ACTIVE_LIST: &[&str] = IDENTIFIED_BUT_UNHIGHLIGHTED_OFF;

    /// Registry lookup through our name mapping (`c_sharp` → `csharp`).
    fn resolves(registry: &LanguageRegistry, name: &str) -> bool {
        let owned = smol_str::SmolStr::new(name);
        registry
            .language(super::super::registry_name(&owned))
            .is_some()
    }

    /// A grammar added without a name, or a name added without a decision
    /// about its grammar, fails this test — in both feature configurations.
    #[test]
    fn every_emitted_name_resolves_or_is_listed() {
        ensure_registered();
        let registry = LanguageRegistry::singleton();
        for listed in ACTIVE_LIST {
            assert!(
                ferret_preview::languages::ALL_NAMES.contains(listed),
                "{listed} is listed but never emitted — remove it"
            );
            assert!(
                !resolves(registry, listed),
                "{listed} now resolves — remove it from the list"
            );
        }
        for name in ferret_preview::languages::ALL_NAMES {
            assert!(
                resolves(registry, name) || ACTIVE_LIST.contains(name),
                "{name} neither resolves nor is listed — register a grammar or list it"
            );
        }
    }

    /// Per-language proof: the parser loads (ABI), the query compiles
    /// against this grammar version, every capture styles something, and a
    /// representative fixture produces meaningful spans.
    #[cfg(feature = "languages-extended")]
    #[test]
    fn extended_grammars_highlight_their_fixtures() {
        ensure_registered();
        assert_eq!(
            EXTENDED.len(),
            FIXTURES.len(),
            "a grammar without a fixture (or vice versa)"
        );
        for (name, fixture, min_captures) in FIXTURES {
            let lang = EXTENDED
                .iter()
                .find(|lang| lang.name == *name)
                .unwrap_or_else(|| panic!("{name} has a fixture but no registration"));
            check_grammar(lang, fixture, *min_captures);
        }
    }

    #[cfg(feature = "languages-extended")]
    fn check_grammar(lang: &ExtendedLanguage, fixture: &str, min_captures: usize) {
        let language = (lang.language)();
        // The parser loads: an incompatible ABI refuses here, not at render.
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .unwrap_or_else(|_| panic!("{}: parser ABI refused", lang.name));
        let tree = parser
            .parse(fixture, None)
            .unwrap_or_else(|| panic!("{}: fixture parses", lang.name));
        assert!(
            !tree.root_node().has_error(),
            "{}: fixture must parse cleanly",
            lang.name
        );
        // The vendored query compiles against this grammar version.
        let query = tree_sitter::Query::new(&language, lang.highlights)
            .unwrap_or_else(|err| panic!("{}: query compiles ({err:?})", lang.name));
        // Every capture styles something — or is an explicit directive.
        // Mirrors the component's own resolution: exact slot, else the
        // first dotted segment.
        for capture in query.capture_names() {
            let first = capture.split('.').next().unwrap_or("");
            if capture.starts_with('_') || DIRECTIVES.contains(&first) {
                continue;
            }
            let styled = crate::theme::SYNTAX_SLOTS.contains(capture)
                || crate::theme::SYNTAX_SLOTS.contains(&first);
            assert!(
                styled,
                "{}: @{capture} styles nothing — adapt the vendored query",
                lang.name
            );
        }
        // The fixture exercises the query: meaningful spans, not just a root.
        use tree_sitter::StreamingIterator as _;
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), fixture.as_bytes());
        let mut styled = 0;
        while let Some(mat) = matches.next() {
            for capture in mat.captures {
                let name = query.capture_names()[capture.index as usize];
                if !name.starts_with('_')
                    && !DIRECTIVES.contains(&name.split('.').next().unwrap_or(""))
                {
                    styled += 1;
                }
            }
        }
        assert!(
            styled >= min_captures,
            "{}: only {styled} styled spans, want {min_captures}",
            lang.name
        );
    }
}
