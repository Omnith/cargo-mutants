// Copyright 2026 Martin Pool

//! Find source files that tests might read as text.
//!
//! A schema rewrites the source files it embeds mutants in, and the crate roots that
//! get its helper module. Tests that read those files, for example with
//! `include_str!("lib.rs")` or `read_to_string("src/lib.rs")`, see the schema text,
//! the same whichever mutant is active: if that makes them fail, the baseline fails.
//! But tested the classic way, they see the mutated text, which might make them fail
//! where they passed with the schema. So mutants in files named by string literals
//! anywhere in the package are tested again the classic way if the schema misses
//! them.
//!
//! This only finds paths written literally, not those computed at runtime, for
//! example from `file!()`.

#![warn(clippy::pedantic)]

use std::collections::BTreeSet;
use std::str::FromStr;

use camino::Utf8Path;
use proc_macro2::{TokenStream, TokenTree};

/// Find which of `candidates` are named by string literals in `sources`.
///
/// All paths are relative to the tree root, with `/` separators. `sources` are
/// Rust files of a package in `package_dir`, with their text.
///
/// A literal ending in `.rs` names a file if, as a path relative to the directory of
/// the file containing it (as for `include_str!`), to the package directory (as for
/// a test reading a file from its working directory), or to the tree root, it is
/// that file. A leading `/` is ignored, as in
/// `concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")`.
///
/// Files that can't be tokenized are skipped.
pub(crate) fn named_files(
    sources: &[(String, String)],
    package_dir: &str,
    candidates: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    for (path, text) in sources {
        let Ok(tokens) = TokenStream::from_str(text) else {
            continue;
        };
        let file_dir = Utf8Path::new(path).parent().map_or("", Utf8Path::as_str);
        let mut literals = Vec::new();
        string_literals(tokens, &mut literals);
        for literal in literals
            .iter()
            .filter(|l| Utf8Path::new(l).extension() == Some("rs"))
        {
            let relative = literal.trim_start_matches('/');
            for base in [file_dir, package_dir, ""] {
                if let Some(joined) = normalize(&format!("{base}/{relative}"))
                    && candidates.contains(&joined)
                {
                    named.insert(joined);
                }
            }
        }
    }
    named
}

/// Collect the values of string literals, including raw strings, in `tokens`.
fn string_literals(tokens: TokenStream, literals: &mut Vec<String>) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => string_literals(group.stream(), literals),
            TokenTree::Literal(literal) => {
                if let Ok(lit) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    literals.push(lit.value());
                }
            }
            TokenTree::Ident(_) | TokenTree::Punct(_) => (),
        }
    }
}

/// Resolve `.` and `..` in a relative path, returning it with `/` separators and no
/// leading `/`, or `None` if it goes above the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;

    fn named(sources: &[(&str, &str)], package_dir: &str, candidates: &[&str]) -> Vec<String> {
        let sources = sources
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect::<Vec<_>>();
        let candidates = candidates.iter().map(|c| (*c).to_owned()).collect();
        named_files(&sources, package_dir, &candidates)
            .into_iter()
            .collect()
    }

    #[test]
    fn named_files_finds_include_str_relative_to_including_file() {
        assert_eq!(
            named(
                &[(
                    "p/src/shapes.rs",
                    r#"#[test] fn t() { include_str!("shapes.rs"); }"#
                )],
                "p",
                &["p/src/shapes.rs", "p/src/lib.rs"]
            ),
            ["p/src/shapes.rs"]
        );
    }

    #[test]
    fn named_files_finds_paths_relative_to_package_or_manifest_dir() {
        let sources = [(
            "p/tests/docs.rs",
            r##"
                fn a() { std::fs::read_to_string("src/lib.rs"); }
                fn b() { concat!(env!("CARGO_MANIFEST_DIR"), "/src/util/../main.rs"); }
                fn c() { r#"p/src/bin/tool.rs"#; }
            "##,
        )];
        assert_eq!(
            named(
                &sources,
                "p",
                &[
                    "p/src/lib.rs",
                    "p/src/main.rs",
                    "p/src/bin/tool.rs",
                    "p/src/other.rs"
                ]
            ),
            ["p/src/bin/tool.rs", "p/src/lib.rs", "p/src/main.rs"]
        );
    }

    #[test]
    fn named_files_ignores_other_literals_and_untokenizable_files() {
        assert!(
            named(
                &[
                    (
                        "src/lib.rs",
                        r#"fn f() { "lib.rs is great"; b"src/lib.rs"; }"#
                    ),
                    ("src/bad.rs", r#"fn f() { "src/lib.rs" "#),
                    ("src/up.rs", r#"fn f() { "../../lib.rs"; }"#),
                ],
                "",
                &["src/lib.rs"]
            )
            .is_empty()
        );
    }
}
