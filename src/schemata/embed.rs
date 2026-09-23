// Copyright 2026 Martin Pool

//! Track which mutants are embedded in the schema, render the schema files, and
//! drop mutants that are blamed for compile errors.

#![warn(clippy::pedantic)]

use std::collections::{BTreeMap, HashMap, HashSet};

use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde::Serialize;
use tracing::{debug, trace};

use super::diagnostics::{CompileError, Location};
use super::generate::{Attribution, MutantId, Schema, helper_module, render};
use super::plan::{FallbackReason, Placement, plan_file, sites};
use crate::Result;
use crate::mutant::Mutant;
use crate::path::Utf8PathSlashes;

/// One mutant and whether it is embedded.
#[derive(Debug)]
struct Candidate {
    mutant: Mutant,
    id: MutantId,
    placement: std::result::Result<Placement, FallbackReason>,
    /// The distinct compile errors that dropped this mutant from the schema.
    blame: Vec<Blame>,
    /// Recorded as unviable without testing it the classic way.
    recorded_unviable: bool,
}

/// Which part of a schema a compile error was attributed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Blamed {
    /// The arm of one mutant: the error is in its replacement text.
    Arm,
    /// A site, outside any one arm, which drops the site's mutants.
    Site,
    /// A schema file, outside any site, which drops all the file's mutants.
    File,
}

/// A compile error that dropped a mutant from the schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Blame {
    /// The rustc error code, like `E0277`.
    pub code: Option<String>,
    pub message: String,
    /// `file:line:column` of the error in the schema. Lines match the original
    /// source; columns don't.
    pub location: Option<String>,
    pub blamed: Blamed,
}

/// Error codes that, reported inside a mutant's own arm, mean its replacement can't
/// compile anywhere, so the mutant is also unviable classically.
///
/// These are failures to resolve a name, find a method, or satisfy a trait bound or
/// operator for a type that the arm shares with the classic mutant: the function's
/// declared return type, or the operand types of an operator. Codes whose cause can
/// be the schema's `match` itself are excluded, like mismatched arm types (`E0308`)
/// for an `impl Trait` return, or inference and borrow errors.
const CONTEXT_FREE_ERROR_CODES: &[&str] = &[
    "E0061", // wrong number of arguments
    "E0277", // trait bound not satisfied
    "E0369", // binary operator not supported for the types
    "E0412", // cannot find type
    "E0422", // cannot find struct
    "E0423", // expected value, found struct or type
    "E0425", // cannot find value or function
    "E0433", // failed to resolve a path
    "E0599", // no method or associated item
    "E0600", // unary operator not supported for the type
    "E0790", // trait function called without an implementing type
];

impl Blame {
    fn new(error: &CompileError, location: Option<&Location>, blamed: Blamed) -> Blame {
        Blame {
            code: error.code.clone(),
            message: error.message.clone(),
            location: location.map(|l| format!("{}:{}:{}", l.file, l.line, l.column)),
            blamed,
        }
    }

    /// True if this error, by itself, shows that the mutant is unviable.
    fn proves_unviable(&self) -> bool {
        self.blamed == Blamed::Arm
            && self
                .code
                .as_deref()
                .is_some_and(|code| CONTEXT_FREE_ERROR_CODES.contains(&code))
    }
}

/// True if a mutant that fell back for `reason`, blamed for `blame`, is unviable
/// without needing to build it classically.
///
/// Mutants that replace the `&&` of a let chain qualify, because Rust accepts
/// `let` in a condition only in a chain of `&&`. Mutants dropped for compile errors
/// qualify only if the errors are all in their own arm, and are all ones whose
/// cause can't be the schema.
pub(crate) fn proven_unviable(reason: FallbackReason, blame: &[Blame]) -> bool {
    match reason {
        FallbackReason::LetChain => true,
        FallbackReason::CompileError => {
            !blame.is_empty() && blame.iter().all(Blame::proves_unviable)
        }
        _ => false,
    }
}

/// A source file that has mutants, and its most recently rendered schema.
#[derive(Debug)]
struct SchemaFile {
    path: Utf8PathBuf,
    code: String,
    /// Indexes into `Embedding::candidates`.
    candidates: Vec<usize>,
    lint_allow_at: usize,
    schema: Schema,
}

/// What `drop_for_errors` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DropSummary {
    /// Number of mutants newly moved to fallback.
    pub dropped: usize,
    /// Messages of errors that could not be attributed to any schema file.
    pub unattributed: Vec<String>,
}

/// All candidate mutants, and the schema files that embed them.
#[derive(Debug)]
pub(crate) struct Embedding {
    candidates: Vec<Candidate>,
    files: Vec<SchemaFile>,
    /// Crate roots that get the helper module, with their original text, keyed by
    /// tree-relative path.
    roots: BTreeMap<Utf8PathBuf, String>,
    /// The text of the helper module.
    helper: String,
}

impl Embedding {
    /// Plan where every mutant goes; mutant `i` gets id `i + 1`.
    ///
    /// `roots` are the crate roots (tree-relative path and original text) of the
    /// packages being mutated. Mutants in the packages named in `package_fallbacks`
    /// are not embedded, for the given reason. The schema records what it observes
    /// at runtime in `marker_dir`.
    pub(crate) fn new(
        mutants: Vec<Mutant>,
        roots: BTreeMap<Utf8PathBuf, String>,
        package_fallbacks: &HashMap<String, FallbackReason>,
        marker_dir: &Utf8Path,
    ) -> Result<Embedding> {
        let n_mutants = mutants.len();
        let mut placements: Vec<Option<std::result::Result<Placement, FallbackReason>>> =
            vec![None; mutants.len()];
        let mut files = Vec::new();
        let by_file = (0..mutants.len())
            .into_group_map_by(|&i| mutants[i].source_file.tree_relative_path.clone());
        for (path, indexes) in by_file.into_iter().sorted_by(|a, b| a.0.cmp(&b.0)) {
            let code = mutants[indexes[0]].source_file.code().to_owned();
            let file_mutants = indexes.iter().map(|&i| &mutants[i]).collect_vec();
            let plan = plan_file(&code, &file_mutants)?;
            for (&i, placement) in indexes.iter().zip(plan.placements) {
                placements[i] = Some(
                    match package_fallbacks.get(&mutants[i].source_file.package.name) {
                        Some(&reason) => Err(reason),
                        None => placement,
                    },
                );
            }
            files.push(SchemaFile {
                path,
                code,
                candidates: indexes,
                lint_allow_at: plan.lint_allow_at,
                schema: Schema::default(),
            });
        }
        let candidates = mutants
            .into_iter()
            .zip(placements)
            .enumerate()
            .map(|(i, (mutant, placement))| Candidate {
                mutant,
                id: MutantId::try_from(i + 1).expect("mutant count fits in u32"),
                placement: placement.expect("every mutant was planned"),
                blame: Vec::new(),
                recorded_unviable: false,
            })
            .collect();
        Ok(Embedding {
            candidates,
            files,
            roots,
            helper: helper_module(
                marker_dir.as_str(),
                MutantId::try_from(n_mutants).expect("mutant count fits in u32"),
            ),
        })
    }

    /// Mutants embedded in the schema, with their ids.
    pub(crate) fn embedded(&self) -> impl Iterator<Item = (MutantId, &Mutant)> {
        self.candidates
            .iter()
            .filter(|c| c.placement.is_ok())
            .map(|c| (c.id, &c.mutant))
    }

    /// Mutants not embedded, and why.
    pub(crate) fn fallback(&self) -> impl Iterator<Item = (&Mutant, FallbackReason)> {
        self.candidates
            .iter()
            .filter_map(|c| c.placement.as_ref().err().map(|r| (&c.mutant, *r)))
    }

    /// Mutants not embedded, why, and the compile errors that dropped them, if any.
    pub(crate) fn fallback_blamed(
        &self,
    ) -> impl Iterator<Item = (&Mutant, FallbackReason, &[Blame])> {
        self.candidates.iter().filter_map(|c| {
            c.placement
                .as_ref()
                .err()
                .map(|r| (&c.mutant, *r, c.blame.as_slice()))
        })
    }

    /// Fallback mutants to test the classic way: all but those recorded as unviable.
    pub(crate) fn classic_fallback(&self) -> impl Iterator<Item = &Mutant> {
        self.candidates
            .iter()
            .filter(|c| c.placement.is_err() && !c.recorded_unviable)
            .map(|c| &c.mutant)
    }

    /// Mark the fallback mutants that are [`proven_unviable`] as recorded unviable,
    /// so they aren't tested the classic way, and return them with their blame.
    pub(crate) fn take_proven_unviable(&mut self) -> Vec<(Mutant, Vec<Blame>)> {
        self.candidates
            .iter_mut()
            .filter(|c| {
                !c.recorded_unviable
                    && c.placement
                        .as_ref()
                        .is_err_and(|reason| proven_unviable(*reason, &c.blame))
            })
            .map(|c| {
                c.recorded_unviable = true;
                (c.mutant.clone(), c.blame.clone())
            })
            .collect()
    }

    /// Render every schema file and crate root, returning the files to write.
    pub(crate) fn render(&mut self) -> Vec<(Utf8PathBuf, String)> {
        let Embedding {
            candidates,
            files,
            roots,
            helper,
        } = self;
        let mut rendered = Vec::with_capacity(files.len() + roots.len());
        for file in files.iter_mut() {
            let file_sites = sites(file.candidates.iter().filter_map(|&i| {
                let candidate = &candidates[i];
                candidate.placement.as_ref().ok().map(|p| (candidate.id, p))
            }));
            let lint_allow_at = (!file_sites.is_empty()).then_some(file.lint_allow_at);
            file.schema = render(&file.code, file_sites, lint_allow_at);
            for &id in &file.schema.overlapping {
                trace!(id, "site overlaps another site");
                candidates[id as usize - 1].placement = Err(FallbackReason::OverlappingSite);
            }
            let mut text = file.schema.text.clone();
            if roots.contains_key(&file.path) {
                text.push_str(helper);
            }
            rendered.push((file.path.clone(), text));
        }
        for (path, code) in roots.iter() {
            if !files.iter().any(|file| &file.path == path) {
                rendered.push((path.clone(), format!("{code}{helper}")));
            }
        }
        rendered
    }

    /// The original text of every file that `render` returns, to restore the tree.
    pub(crate) fn originals(&self) -> Vec<(Utf8PathBuf, String)> {
        self.files
            .iter()
            .map(|file| (file.path.clone(), file.code.clone()))
            .chain(
                self.roots
                    .iter()
                    .filter(|(path, _)| !self.files.iter().any(|file| &file.path == *path))
                    .map(|(path, code)| (path.clone(), code.clone())),
            )
            .collect()
    }

    /// Move the mutants blamed for compile errors in the last rendered schema to fallback.
    ///
    /// An error inside one mutant's arm drops that mutant. An error elsewhere in a site
    /// drops that site's own mutants, or if it has none left, the mutants nested inside
    /// it; so repeated errors escalate from a site to its contents. An error elsewhere in
    /// a schema file drops every mutant of the file.
    ///
    /// `build_dir` is used to recognize absolute paths in error locations.
    pub(crate) fn drop_for_errors(
        &mut self,
        errors: &[CompileError],
        build_dir: &Utf8Path,
    ) -> DropSummary {
        let mut summary = DropSummary::default();
        let canonical_build_dir = build_dir.canonicalize_utf8().ok();
        // Mutants dropped by this call: later errors in the same output also blame them.
        let mut dropped_now = HashSet::new();
        for error in errors {
            let target = error.locations.iter().find_map(|location| {
                let path = tree_relative(&location.file, build_dir, canonical_build_dir.as_deref());
                self.files
                    .iter()
                    .position(|file| file.path.to_slash_path() == path)
                    .map(|file_index| (file_index, location))
            });
            let Some((file_index, location)) = target else {
                debug!(
                    code = error.code,
                    message = error.message,
                    "compile error outside schema files"
                );
                summary.unattributed.push(error.message.clone());
                continue;
            };
            let file = &self.files[file_index];
            let (ids, reason, blamed) = match file.schema.attribute(location.byte) {
                Attribution::Arm(id) => (vec![id], FallbackReason::CompileError, Blamed::Arm),
                Attribution::Site(ids) => {
                    (ids, FallbackReason::EnclosingSiteCompileError, Blamed::Site)
                }
                Attribution::File => (
                    file.candidates
                        .iter()
                        .map(|&i| self.candidates[i].id)
                        .collect(),
                    FallbackReason::FileCompileError,
                    Blamed::File,
                ),
            };
            let blame = Blame::new(error, Some(location), blamed);
            for id in ids {
                let candidate = &mut self.candidates[id as usize - 1];
                if candidate.placement.is_ok() {
                    debug!(
                        id,
                        mutant = candidate.mutant.name(true),
                        ?reason,
                        code = blame.code,
                        location = blame.location,
                        error = error.message,
                        "drop mutant from schema"
                    );
                    candidate.placement = Err(reason);
                    dropped_now.insert(id);
                    summary.dropped += 1;
                }
                if dropped_now.contains(&id) && !candidate.blame.contains(&blame) {
                    candidate.blame.push(blame.clone());
                }
            }
        }
        summary
    }

    /// Move one embedded mutant to fallback.
    pub(crate) fn fall_back(&mut self, id: MutantId, reason: FallbackReason) {
        let candidate = &mut self.candidates[id as usize - 1];
        debug_assert!(candidate.placement.is_ok(), "mutant {id} is embedded");
        candidate.placement = Err(reason);
    }

    /// Move every embedded mutant to fallback.
    pub(crate) fn fall_back_all(&mut self, reason: FallbackReason) {
        for candidate in &mut self.candidates {
            if candidate.placement.is_ok() {
                candidate.placement = Err(reason);
            }
        }
    }
}

/// Convert a path from a compiler message to a tree-relative path with slashes.
///
/// Cargo usually reports workspace files relative to the workspace root, but may use
/// absolute paths inside the build directory.
fn tree_relative(
    file: &str,
    build_dir: &Utf8Path,
    canonical_build_dir: Option<&Utf8Path>,
) -> String {
    let path = Utf8Path::new(file);
    [Some(build_dir), canonical_build_dir]
        .into_iter()
        .flatten()
        .find_map(|dir| path.strip_prefix(dir).ok())
        .unwrap_or(path)
        .to_slash_path()
}

#[cfg(test)]
mod test {
    use itertools::Itertools;
    use pretty_assertions::assert_eq;

    use super::super::diagnostics::Location;
    use super::*;
    use crate::Options;
    use crate::visit::mutate_source_str;

    const MARKERS: &str = "/tmp/markers";

    const CODE: &str =
        "fn f(a: u32, b: u32) -> u32 {\n    a + b\n}\n\nfn g(x: bool) -> bool {\n    !x\n}\n";

    fn embedding() -> Embedding {
        let mutants = mutate_source_str(CODE, &Options::default()).unwrap();
        Embedding::new(
            mutants,
            BTreeMap::from([("src/main.rs".into(), CODE.to_owned())]),
            &HashMap::new(),
            MARKERS.into(),
        )
        .unwrap()
    }

    fn error_at(file: &str, byte: usize) -> CompileError {
        coded_error_at("E0277", file, byte)
    }

    fn coded_error_at(code: &str, file: &str, byte: usize) -> CompileError {
        CompileError {
            code: Some(code.to_owned()),
            message: "oops".to_owned(),
            locations: vec![Location {
                file: file.to_owned(),
                byte,
                line: 2,
                column: 7,
            }],
        }
    }

    fn embedded_names(embedding: &Embedding) -> Vec<String> {
        embedding
            .embedded()
            .map(|(id, m)| format!("{id}: {}", m.name(false)))
            .collect()
    }

    #[test]
    fn embedding_numbers_mutants_from_one() {
        assert_eq!(
            embedded_names(&embedding()),
            [
                "1: src/main.rs: replace f -> u32 with 0",
                "2: src/main.rs: replace f -> u32 with 1",
                "3: src/main.rs: replace + with - in f",
                "4: src/main.rs: replace + with * in f",
                "5: src/main.rs: replace g -> bool with true",
                "6: src/main.rs: replace g -> bool with false",
                "7: src/main.rs: delete ! in g",
            ]
        );
    }

    #[test]
    fn render_appends_helper_module_to_crate_root() {
        let mut embedding = embedding();
        let files = embedding.render();
        assert_eq!(files.len(), 1);
        let (path, text) = &files[0];
        assert_eq!(path, "src/main.rs");
        assert!(text.ends_with(&helper_module(MARKERS, 7)));
        assert!(text.contains("_ => (a + b)"));
        assert!(text.contains(r#"const MARKER_DIR: &str = "/tmp/markers";"#));
        assert_eq!(embedding.originals(), [(path.clone(), CODE.to_owned())]);
    }

    #[test]
    fn render_writes_helper_into_root_without_mutants() {
        let mutants = mutate_source_str(CODE, &Options::default()).unwrap();
        let mut embedding = Embedding::new(
            mutants,
            BTreeMap::from([
                ("src/main.rs".into(), CODE.to_owned()),
                ("src/bin/other.rs".into(), "fn main() {}\n".to_owned()),
            ]),
            &HashMap::new(),
            MARKERS.into(),
        )
        .unwrap();
        let files = embedding.render();
        let other = files.iter().find(|(p, _)| p == "src/bin/other.rs").unwrap();
        assert_eq!(
            other.1,
            format!("fn main() {{}}\n{}", helper_module(MARKERS, 7))
        );
    }

    #[test]
    fn drop_for_errors_blames_mutant_whose_arm_contains_error() {
        let mut embedding = embedding();
        let (_, text) = embedding.render().remove(0);
        let offset = text.find("(a - b)").unwrap();
        let summary = embedding.drop_for_errors(&[error_at("src/main.rs", offset)], "/tmp".into());
        assert_eq!(summary.dropped, 1);
        assert_eq!(
            embedding
                .fallback()
                .map(|(m, reason)| (m.name(false), reason))
                .collect_vec(),
            [(
                "src/main.rs: replace + with - in f".to_owned(),
                FallbackReason::CompileError
            )]
        );
    }

    #[test]
    fn drop_for_errors_records_each_distinct_error_blamed_on_a_mutant() {
        let mut embedding = embedding();
        let (_, text) = embedding.render().remove(0);
        let arm = text.find("(a - b)").unwrap();
        // The lib and lib test targets report the same error twice.
        let errors = [
            coded_error_at("E0369", "src/main.rs", arm),
            coded_error_at("E0369", "src/main.rs", arm),
            coded_error_at("E0599", "src/main.rs", arm + 1),
        ];
        embedding.drop_for_errors(&errors, "/tmp".into());
        let blame = embedding.fallback_blamed().next().unwrap().2;
        assert_eq!(
            blame,
            [
                Blame {
                    code: Some("E0369".to_owned()),
                    message: "oops".to_owned(),
                    location: Some("src/main.rs:2:7".to_owned()),
                    blamed: Blamed::Arm,
                },
                Blame {
                    code: Some("E0599".to_owned()),
                    message: "oops".to_owned(),
                    location: Some("src/main.rs:2:7".to_owned()),
                    blamed: Blamed::Arm,
                },
            ]
        );
    }

    #[test]
    fn proven_unviable_needs_every_blame_on_its_own_arm_with_a_context_free_code() {
        let mut embedding = embedding();
        let (_, text) = embedding.render().remove(0);
        let minus = text.find("(a - b)").unwrap();
        let times = text.find("(a * b)").unwrap();
        // The header of `f`'s body site, whose own mutants are its `FnValue` mutants.
        let body_header = text.find("match crate").unwrap();
        embedding.drop_for_errors(
            &[
                // A missing trait impl in the arm is wrong whatever surrounds it.
                coded_error_at("E0277", "src/main.rs", minus),
                // Mismatched arm types can come from the schema's `match` itself.
                coded_error_at("E0308", "src/main.rs", times),
                coded_error_at("E0277", "src/main.rs", body_header),
            ],
            "/tmp".into(),
        );
        assert_eq!(
            embedding
                .fallback_blamed()
                .map(|(m, reason, blame)| (m.name(false), proven_unviable(reason, blame)))
                .collect_vec(),
            [
                ("src/main.rs: replace f -> u32 with 0".to_owned(), false),
                ("src/main.rs: replace f -> u32 with 1".to_owned(), false),
                ("src/main.rs: replace + with - in f".to_owned(), true),
                ("src/main.rs: replace + with * in f".to_owned(), false),
            ]
        );
        let (_, reason, blame) = embedding.fallback_blamed().next().unwrap();
        assert_eq!(reason, FallbackReason::EnclosingSiteCompileError);
        assert_eq!(
            blame
                .iter()
                .map(|b| (b.code.as_deref(), b.blamed))
                .collect_vec(),
            [(Some("E0277"), Blamed::Site)]
        );
    }

    #[test]
    fn take_proven_unviable_keeps_them_out_of_classic_fallback_only() {
        let mut embedding = embedding();
        let (_, text) = embedding.render().remove(0);
        let minus = text.find("(a - b)").unwrap();
        let times = text.find("(a * b)").unwrap();
        embedding.drop_for_errors(
            &[
                coded_error_at("E0277", "src/main.rs", minus),
                coded_error_at("E0308", "src/main.rs", times),
            ],
            "/tmp".into(),
        );
        let taken = embedding.take_proven_unviable();
        assert_eq!(
            taken
                .iter()
                .map(|(m, blame)| (m.name(false), blame.len()))
                .collect_vec(),
            [("src/main.rs: replace + with - in f".to_owned(), 1)]
        );
        // Still a fallback mutant, for reports, but not to be tested again.
        assert_eq!(embedding.fallback().count(), 2);
        assert_eq!(
            embedding
                .classic_fallback()
                .map(|m| m.name(false))
                .collect_vec(),
            ["src/main.rs: replace + with * in f"]
        );
        assert!(embedding.take_proven_unviable().is_empty());
    }

    #[test]
    fn proven_unviable_holds_for_let_chain_mutants_without_building() {
        // `let` is only allowed in a chain of `&&`, so changing its `&&` never compiles.
        let code = "fn f(o: Option<u32>, y: bool) -> bool {\n    if let Some(x) = o && y { x > 1 } else { false }\n}\n";
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let embedding = Embedding::new(
            mutants,
            BTreeMap::from([("src/main.rs".into(), code.to_owned())]),
            &HashMap::new(),
            MARKERS.into(),
        )
        .unwrap();
        let let_chain = embedding
            .fallback_blamed()
            .filter(|(_, reason, _)| *reason == FallbackReason::LetChain)
            .collect_vec();
        assert_eq!(let_chain.len(), 1);
        let (mutant, reason, blame) = let_chain[0];
        assert_eq!(mutant.name(false), "src/main.rs: replace && with || in f");
        assert!(proven_unviable(reason, blame));
    }

    #[test]
    fn drop_for_errors_blames_whole_site_for_error_in_site_header() {
        let mut embedding = embedding();
        let (_, text) = embedding.render().remove(0);
        // The binary site's `match` header, inside the fn body's default arm.
        let offset = text.find("(match").unwrap();
        let summary = embedding.drop_for_errors(
            &[error_at("/tmp/build/src/main.rs", offset)],
            "/tmp/build".into(),
        );
        assert_eq!(summary.dropped, 2);
        assert!(
            embedding
                .fallback()
                .all(|(_, reason)| reason == FallbackReason::EnclosingSiteCompileError)
        );
        assert_eq!(embedding.embedded().count(), 5);
    }

    #[test]
    fn drop_for_errors_blames_whole_file_for_error_outside_sites() {
        let mut embedding = embedding();
        embedding.render();
        let summary = embedding.drop_for_errors(&[error_at("src/main.rs", 0)], "/tmp".into());
        assert_eq!(summary.dropped, 7);
        assert_eq!(embedding.embedded().count(), 0);
    }

    #[test]
    fn drop_for_errors_reports_errors_outside_schema_files() {
        let mut embedding = embedding();
        embedding.render();
        let summary = embedding.drop_for_errors(&[error_at("src/other.rs", 3)], "/tmp".into());
        assert_eq!(
            summary,
            DropSummary {
                dropped: 0,
                unattributed: vec!["oops".to_owned()]
            }
        );
    }

    #[test]
    fn embedding_falls_back_for_proc_macro_packages() {
        let mutants = mutate_source_str(CODE, &Options::default()).unwrap();
        let embedding = Embedding::new(
            mutants,
            BTreeMap::new(),
            &HashMap::from([(
                "cargo-mutants-testdata-internal".to_owned(),
                FallbackReason::ProcMacroCrate,
            )]),
            MARKERS.into(),
        )
        .unwrap();
        assert_eq!(embedding.embedded().count(), 0);
        assert!(
            embedding
                .fallback()
                .all(|(_, reason)| reason == FallbackReason::ProcMacroCrate)
        );
    }
}
