// Copyright 2026 Martin Pool

//! Experimental coverage-based test selection for `--schemata`
//! (`--test-selection=coverage`, or `auto` when [`decision`] expects it to pay off).
//!
//! Once the schema is built and its baseline has passed, a copy of the unmutated tree
//! is built with only the workspace's crates instrumented for coverage, and each test
//! is run alone to learn which functions it executes (see [`collect`]). Then each
//! mutant gets a [`Plan`]:
//!
//! - If tests execute a function whose code spans the mutant, only those tests
//!   run, fastest first, in batches that double in size, stopping at the first
//!   failure. If they all pass, the full suite runs to confirm the mutant is missed,
//!   so "missed" means the same as without selection.
//! - If no test executes the mutated code, the mutant is reported missed without
//!   running tests only if the schema also recorded that its code never ran in the
//!   baseline, when all the tests ran with no mutant active: then it doesn't run
//!   with the mutant either. If it did run, coverage missed it, for example in a
//!   process that was killed, which writes no profile, so the full suite runs.
//! - Otherwise, when coverage can't show that, the full suite runs, for a recorded
//!   [`FullSuiteReason`].
//!
//! A test executes a function if any of the function's counters is nonzero in the
//! test's profile. This is coarser than the regions within the function, so more
//! tests may be selected than strictly necessary, but never fewer.

#![warn(clippy::pedantic)]

pub(crate) mod collect;
pub(crate) mod decision;
mod listing;
pub(crate) mod llvm;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde::Serialize;
use tracing::warn;

use self::llvm::MappedFunction;
use super::replay::{ReplayCommand, is_rustdoc};
use crate::span::{LineColumn, Span};

/// Environment variable choosing which mutants are confirmed by running the full
/// suite: `reached` (the default) confirms those whose selected tests all pass, and
/// uncovered mutants whose code the schema recorded running in the baseline; `all`
/// confirms every uncovered mutant too; `passed` confirms only those whose selected
/// tests pass, so uncovered mutants are reported missed without running tests; `none`
/// confirms none.
///
/// `passed` and `none` can report mutants missed that the full suite catches, for
/// example code run only by a program that a test kills, which writes no coverage.
pub(crate) const CONFIRM_ENV: &str = "CARGO_MUTANTS_COVERAGE_CONFIRM";

/// Which mutants are confirmed by running the full suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Confirm {
    /// Never: a mutant whose selected tests pass is reported missed.
    None,
    /// Mutants whose selected tests all pass, but not uncovered mutants.
    Passed,
    /// Mutants whose selected tests all pass, and uncovered mutants whose code ran in
    /// the baseline, where coverage missed it. Uncovered code that didn't run in the
    /// baseline doesn't run with the mutant either, so all the tests miss it.
    Reached,
    /// Mutants whose selected tests all pass, and all uncovered mutants.
    All,
}

impl Confirm {
    /// Parse the value of [`CONFIRM_ENV`], if set, using [`Confirm::Reached`] for a
    /// value that's not recognized.
    pub(crate) fn parse(value: Option<&str>) -> Confirm {
        match value {
            None | Some("reached") => Confirm::Reached,
            Some("all") => Confirm::All,
            Some("passed") => Confirm::Passed,
            Some("none") => Confirm::None,
            Some(other) => {
                warn!(
                    "Unknown {CONFIRM_ENV}={other:?}: expected reached, all, passed, or none; using reached"
                );
                Confirm::Reached
            }
        }
    }

    /// True if an uncovered mutant is confirmed by running the full suite, given
    /// whether its code ran in the baseline.
    pub(crate) fn confirms_uncovered(self, ran_in_baseline: bool) -> bool {
        match self {
            Confirm::All => true,
            Confirm::Reached => ran_in_baseline,
            Confirm::Passed | Confirm::None => false,
        }
    }
}

/// Environment variable choosing how to test mutants in a file where no code ran in
/// any test: `full_suite` (the default) runs all the tests, in case the file's code
/// ran in a program whose profile was lost; `uncovered` reports them missed without
/// running tests, like other code no test executes.
pub(crate) const UNOBSERVED_FILES_ENV: &str = "CARGO_MUTANTS_COVERAGE_UNOBSERVED_FILES";

/// How to test mutants in a file where no code ran in any test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnobservedFiles {
    FullSuite,
    Uncovered,
}

impl UnobservedFiles {
    /// Parse the value of [`UNOBSERVED_FILES_ENV`], if set, using
    /// [`UnobservedFiles::FullSuite`] for a value that's not recognized.
    pub(crate) fn parse(value: Option<&str>) -> UnobservedFiles {
        match value {
            None | Some("full_suite") => UnobservedFiles::FullSuite,
            Some("uncovered") => UnobservedFiles::Uncovered,
            Some(other) => {
                warn!(
                    "Unknown {UNOBSERVED_FILES_ENV}={other:?}: expected full_suite or uncovered; using full_suite"
                );
                UnobservedFiles::FullSuite
            }
        }
    }
}

/// A test that can be run by itself: one test in one test binary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TestCase {
    /// Index of the test binary's command among the selection's test commands.
    pub command: usize,
    /// Full name of the test, as matched by `--exact`, or `None` to run the whole
    /// binary.
    pub name: Option<String>,
}

/// The result of running one test by itself on the unmutated tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Isolated {
    Passed,
    /// The test failed alone although the suite passes, so it's never selected, and
    /// its profile is incomplete: it might have stopped before running code that it
    /// runs in the suite.
    Failed,
    /// The test was killed, so its profile, if any, is incomplete.
    TimedOut,
    /// The test ran but wrote no profile.
    NoProfile,
}

/// One test's run on the unmutated tree.
#[derive(Debug, Clone)]
pub(crate) struct TestRun {
    pub case: TestCase,
    pub seconds: f64,
    pub result: Isolated,
}

/// Index of a function's name in [`FunctionNames`].
pub(crate) type NameId = u32;

/// The distinct mangled names of the functions that the tests of one selection
/// executed, each stored once, so that each test's executed functions are small ids.
///
/// Names are often over 100 bytes, so with thousands of tests each executing
/// thousands of functions, lists of names would take gigabytes.
#[derive(Debug, Default)]
pub(crate) struct FunctionNames {
    ids: HashMap<String, NameId>,
}

impl FunctionNames {
    /// The id of `name`, which is added if it's new.
    pub(crate) fn intern(&mut self, name: &str) -> NameId {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = NameId::try_from(self.ids.len()).expect("function count fits in u32");
        self.ids.insert(name.to_owned(), id);
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    /// Total length of the names, in bytes.
    pub(crate) fn bytes(&self) -> usize {
        self.ids.keys().map(String::len).sum()
    }
}

/// One test's run on the unmutated tree, as collected, with the functions it
/// executed.
#[derive(Debug, Clone)]
pub(crate) struct CollectedRun {
    pub run: TestRun,
    /// Ids of the names of the functions it executed.
    pub executed: Vec<NameId>,
}

/// Why a mutant runs the full suite rather than selected tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FullSuiteReason {
    /// Coverage couldn't be collected, or its test commands don't match the schema's.
    NoCoverage,
    /// The mutated code isn't in any instrumented function, like a `const` initializer.
    NotInstrumented,
    /// No code in the mutated file ran in any test, so the file's coverage might have
    /// been lost, for example from a program run with a cleared environment.
    FileNotObserved,
    /// The only tests that execute the mutated code fail when run alone.
    OnlyFailingTests,
    /// The mutated code isn't executed, but the package has doctests, which aren't
    /// instrumented.
    Doctests,
    /// The mutated code isn't executed, but some tests' profiles are missing or
    /// incomplete: they timed out, wrote no profile, or failed when run alone.
    IncompleteProfiles,
    /// The mutated code isn't executed, but tests executed functions that aren't in
    /// the coverage mapping, so their location is unknown.
    UnattributedExecutions,
}

/// How to test one mutant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Run these tests, as indexes into [`SelectionCoverage::tests`], fastest first.
    Selected(Vec<usize>),
    /// No test executes the mutated code.
    Uncovered,
    /// Run the full suite.
    FullSuite(FullSuiteReason),
}

/// Index of a function in [`SelectionCoverage`].
type FunctionId = usize;

/// Coverage of the tests for one package selection: which functions each test
/// executes, and where the functions' code is.
#[derive(Debug)]
pub(crate) struct SelectionCoverage {
    tests: Vec<TestRun>,
    /// For each source file, relative to the tree root, the extent of each function's
    /// code in it: from the start of its first region to the end of its last.
    extents: HashMap<Utf8PathBuf, Vec<(Span, FunctionId)>>,
    /// For each function, the indexes of the tests that execute it.
    tests_by_function: Vec<Vec<u32>>,
    /// Number of doctests in the selection.
    doctests: usize,
    /// Names of the executed functions that are not in the mapping, sorted.
    unattributed_functions: Vec<String>,
    unobserved_files: UnobservedFiles,
}

impl SelectionCoverage {
    /// Combine the coverage mapping with the tests' runs, whose executed functions
    /// are named in `names`. Each run's list of functions is dropped once it's
    /// recorded in the map from functions to tests.
    pub(crate) fn new(
        functions: &[MappedFunction],
        names: &FunctionNames,
        runs: Vec<CollectedRun>,
        doctests: usize,
    ) -> SelectionCoverage {
        let mut ids_by_name: HashMap<&str, Vec<FunctionId>> = HashMap::new();
        let mut extents: HashMap<Utf8PathBuf, Vec<(Span, FunctionId)>> = HashMap::new();
        for (id, function) in functions.iter().enumerate() {
            ids_by_name.entry(&function.name).or_default().push(id);
            let by_file = function.regions.iter().into_group_map_by(|(file, _)| file);
            for (file, regions) in by_file {
                let start = regions
                    .iter()
                    .map(|(_, r)| r.start)
                    .min_by_key(|p| position(*p));
                let end = regions
                    .iter()
                    .map(|(_, r)| r.end)
                    .max_by_key(|p| position(*p));
                if let (Some(start), Some(end)) = (start, end) {
                    extents
                        .entry(file.clone())
                        .or_default()
                        .push((Span { start, end }, id));
                }
            }
        }
        // The functions with each name, or none if the name isn't in the mapping.
        let mut functions_by_name: Vec<&[FunctionId]> = vec![&[]; names.len()];
        let mut unattributed = HashSet::new();
        for (name, name_id) in &names.ids {
            match ids_by_name.get(name.as_str()) {
                Some(ids) => functions_by_name[*name_id as usize] = ids,
                None => {
                    unattributed.insert(*name_id);
                }
            }
        }
        let mut tests_by_function = vec![Vec::new(); functions.len()];
        let mut tests = Vec::with_capacity(runs.len());
        let mut executed_unattributed = HashSet::new();
        for (test_index, CollectedRun { run, executed }) in runs.into_iter().enumerate() {
            let test_index = u32::try_from(test_index).expect("test count fits in u32");
            for name_id in executed {
                for id in functions_by_name[name_id as usize] {
                    tests_by_function[*id].push(test_index);
                }
                if unattributed.contains(&name_id) {
                    executed_unattributed.insert(name_id);
                }
            }
            tests.push(run);
        }
        let unattributed_functions = names
            .ids
            .iter()
            .filter(|(_, name_id)| executed_unattributed.contains(*name_id))
            .map(|(name, _)| name.clone())
            .sorted()
            .collect();
        SelectionCoverage {
            tests,
            extents,
            tests_by_function,
            doctests,
            unattributed_functions,
            unobserved_files: UnobservedFiles::FullSuite,
        }
    }

    /// Set how to test mutants in files where no code ran.
    #[must_use]
    pub(crate) fn with_unobserved_files(mut self, unobserved_files: UnobservedFiles) -> Self {
        self.unobserved_files = unobserved_files;
        self
    }

    pub(crate) fn tests(&self) -> &[TestRun] {
        &self.tests
    }

    pub(crate) fn unattributed_functions(&self) -> &[String] {
        &self.unattributed_functions
    }

    /// Decide how to test a mutant of `span` in `file`, relative to the tree root.
    pub(crate) fn plan(&self, file: &Utf8Path, span: Span) -> Plan {
        let Some(file_extents) = self.extents.get(file) else {
            return Plan::FullSuite(FullSuiteReason::NotInstrumented);
        };
        let functions: BTreeSet<FunctionId> = file_extents
            .iter()
            .filter(|(extent, _)| overlaps(*extent, span))
            .map(|(_, id)| *id)
            .collect();
        if functions.is_empty() {
            return Plan::FullSuite(FullSuiteReason::NotInstrumented);
        }
        if self.unobserved_files == UnobservedFiles::FullSuite
            && file_extents
                .iter()
                .all(|(_, id)| self.tests_by_function[*id].is_empty())
        {
            return Plan::FullSuite(FullSuiteReason::FileNotObserved);
        }
        let covering: BTreeSet<usize> = functions
            .iter()
            .flat_map(|id| self.tests_by_function[*id].iter().map(|i| *i as usize))
            .collect();
        if !covering.is_empty() {
            let mut passing: Vec<usize> = covering
                .into_iter()
                .filter(|index| self.tests[*index].result == Isolated::Passed)
                .collect();
            if passing.is_empty() {
                return Plan::FullSuite(FullSuiteReason::OnlyFailingTests);
            }
            passing.sort_by(|a, b| self.tests[*a].seconds.total_cmp(&self.tests[*b].seconds));
            return Plan::Selected(passing);
        }
        if self.doctests > 0 {
            Plan::FullSuite(FullSuiteReason::Doctests)
        } else if self.tests.iter().any(|test| {
            matches!(
                test.result,
                Isolated::Failed | Isolated::TimedOut | Isolated::NoProfile
            )
        }) {
            Plan::FullSuite(FullSuiteReason::IncompleteProfiles)
        } else if !self.unattributed_functions.is_empty() {
            Plan::FullSuite(FullSuiteReason::UnattributedExecutions)
        } else {
            Plan::Uncovered
        }
    }
}

/// True if a function's extent and a mutant's span overlap, counting both ends of
/// both as included, so that an extent touching the span counts.
fn overlaps(extent: Span, span: Span) -> bool {
    position(extent.start) <= position(span.end) && position(span.start) <= position(extent.end)
}

fn position(lc: LineColumn) -> (usize, usize) {
    (lc.line, lc.column)
}

/// Split `n` selected tests into batches run one after another: 1, 2, 4, ...
/// tests, so that the fastest test runs alone first and a mutant with many
/// covering tests needs only a few processes.
pub(crate) fn batches(n: usize) -> Vec<Range<usize>> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut size = 1;
    while start < n {
        let end = n.min(start + size);
        batches.push(start..end);
        start = end;
        size *= 2;
    }
    batches
}

/// The paths by which a tree whose root is `root` may be named: `root`, and its
/// canonical path if that's different, as Cargo reports it when `root` is reached
/// through a symlink.
///
/// This reads the filesystem, so it must be called while the tree exists.
pub(crate) fn tree_roots(root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut roots = vec![root.to_owned()];
    if let Ok(canonical) = root.canonicalize_utf8()
        && canonical != root
    {
        roots.push(canonical);
    }
    roots
}

/// True if two runs of `cargo test`, `a` in a tree named by `a_roots` and `b` in
/// one named by `b_roots` (see [`tree_roots`]), ran the same test binaries in the
/// same order, so that command indexes in one refer to the same tests in the other.
///
/// Binaries are compared by package directory relative to the tree's root, and by
/// file name without the hash.
pub(crate) fn same_test_binaries(
    a: &[ReplayCommand],
    a_roots: &[Utf8PathBuf],
    b: &[ReplayCommand],
    b_roots: &[Utf8PathBuf],
) -> bool {
    fn identity<'c>(
        command: &'c ReplayCommand,
        roots: &[Utf8PathBuf],
    ) -> (Option<&'c str>, &'c str) {
        let manifest_dir = command
            .env
            .iter()
            .find(|(key, _)| key == "CARGO_MANIFEST_DIR")
            .map(|(_, value)| {
                roots
                    .iter()
                    .find_map(|root| Utf8Path::new(value).strip_prefix(root).ok())
                    .map_or(value.as_str(), Utf8Path::as_str)
            });
        let file_name = Utf8Path::new(&command.argv[0])
            .file_name()
            .unwrap_or_default();
        let target = if runs_doctests(command) {
            "rustdoc"
        } else {
            file_name
                .rsplit_once('-')
                .map_or(file_name, |(name, _hash)| name)
        };
        (manifest_dir, target)
    }
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| identity(a, a_roots) == identity(b, b_roots))
}

/// True if the command runs doctests.
pub(crate) fn runs_doctests(command: &ReplayCommand) -> bool {
    is_rustdoc(&command.argv[0])
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;

    fn function(name: &str, file: &str, span: Span) -> MappedFunction {
        MappedFunction {
            name: name.to_owned(),
            regions: vec![(file.into(), span)],
        }
    }

    /// A test's run, and the names of the functions it executed.
    type Run = (TestRun, Vec<String>);

    fn run(name: &str, seconds: f64, result: Isolated, executed: &[&str]) -> Run {
        let run = TestRun {
            case: TestCase {
                command: 0,
                name: Some(name.to_owned()),
            },
            seconds,
            result,
        };
        (run, executed.iter().map(|s| (*s).to_owned()).collect())
    }

    /// The coverage of `runs`, with their executed functions' names interned.
    fn selection(
        functions: &[MappedFunction],
        runs: Vec<Run>,
        doctests: usize,
    ) -> SelectionCoverage {
        let mut names = FunctionNames::default();
        let runs = runs
            .into_iter()
            .map(|(run, executed)| CollectedRun {
                run,
                executed: executed.iter().map(|name| names.intern(name)).collect(),
            })
            .collect();
        SelectionCoverage::new(functions, &names, runs, doctests)
    }

    /// `add` (lines 1-3) is executed by a slow and a fast test; `unused` (lines 5-7)
    /// and `flaky` (lines 9-11) by none; nothing in `src/bin/tool.rs` runs.
    fn coverage(extra_tests: Vec<Run>, doctests: usize) -> SelectionCoverage {
        let mut tests = vec![
            run("slow", 2.0, Isolated::Passed, &["add"]),
            run("fast", 0.1, Isolated::Passed, &["add", "helper"]),
        ];
        tests.extend(extra_tests);
        selection(
            &[
                function("add", "src/lib.rs", Span::quad(1, 1, 3, 2)),
                function("unused", "src/lib.rs", Span::quad(5, 1, 7, 2)),
                function("flaky", "src/lib.rs", Span::quad(9, 1, 11, 2)),
                function("helper", "src/lib.rs", Span::quad(13, 1, 13, 20)),
                function("main", "src/bin/tool.rs", Span::quad(1, 1, 3, 2)),
            ],
            tests,
            doctests,
        )
    }

    fn lib() -> &'static Utf8Path {
        Utf8Path::new("src/lib.rs")
    }

    #[test]
    fn plan_selects_tests_executing_the_mutated_function_fastest_first() {
        let coverage = coverage(vec![], 0);
        assert_eq!(
            coverage.plan(lib(), Span::quad(2, 5, 2, 6)),
            Plan::Selected(vec![1, 0])
        );
        // A span that only touches the end of the function's region still overlaps it.
        assert_eq!(
            coverage.plan(lib(), Span::quad(3, 2, 3, 3)),
            Plan::Selected(vec![1, 0])
        );
    }

    #[test]
    fn plan_selects_tests_for_operator_between_regions_of_a_function() {
        // LLVM maps the operands of `a < b` to regions, but not always the operator.
        let coverage = selection(
            &[MappedFunction {
                name: "compare".to_owned(),
                regions: vec![
                    ("src/lib.rs".into(), Span::quad(2, 5, 2, 6)),
                    ("src/lib.rs".into(), Span::quad(2, 9, 2, 10)),
                ],
            }],
            vec![run("t", 0.1, Isolated::Passed, &["compare"])],
            0,
        );
        assert_eq!(
            coverage.plan(lib(), Span::quad(2, 7, 2, 8)),
            Plan::Selected(vec![0])
        );
    }

    #[test]
    fn plan_is_uncovered_for_code_no_test_executes() {
        assert_eq!(
            coverage(vec![], 0).plan(lib(), Span::quad(6, 5, 6, 6)),
            Plan::Uncovered
        );
    }

    #[test]
    fn plan_runs_full_suite_for_code_outside_instrumented_functions() {
        let coverage = coverage(vec![], 0);
        assert_eq!(
            coverage.plan(lib(), Span::quad(20, 1, 20, 5)),
            Plan::FullSuite(FullSuiteReason::NotInstrumented)
        );
        assert_eq!(
            coverage.plan("src/other.rs".into(), Span::quad(1, 1, 1, 5)),
            Plan::FullSuite(FullSuiteReason::NotInstrumented)
        );
    }

    #[test]
    fn plan_runs_full_suite_in_file_where_nothing_ran() {
        assert_eq!(
            coverage(vec![], 0).plan("src/bin/tool.rs".into(), Span::quad(2, 5, 2, 9)),
            Plan::FullSuite(FullSuiteReason::FileNotObserved)
        );
    }

    #[test]
    fn plan_is_uncovered_in_file_where_nothing_ran_if_unobserved_files_are_trusted() {
        let coverage = coverage(vec![], 0).with_unobserved_files(UnobservedFiles::Uncovered);
        assert_eq!(
            coverage.plan("src/bin/tool.rs".into(), Span::quad(2, 5, 2, 9)),
            Plan::Uncovered
        );
    }

    #[test]
    fn unobserved_files_parse_defaults_to_full_suite() {
        assert_eq!(UnobservedFiles::parse(None), UnobservedFiles::FullSuite);
        assert_eq!(
            UnobservedFiles::parse(Some("uncovered")),
            UnobservedFiles::Uncovered
        );
        assert_eq!(
            UnobservedFiles::parse(Some("maybe")),
            UnobservedFiles::FullSuite
        );
    }

    #[test]
    fn plan_never_selects_tests_that_fail_alone() {
        let alone_fails = run("alone_fails", 0.1, Isolated::Failed, &["flaky"]);
        assert_eq!(
            coverage(vec![alone_fails], 0).plan(lib(), Span::quad(10, 5, 10, 6)),
            Plan::FullSuite(FullSuiteReason::OnlyFailingTests)
        );
    }

    #[test]
    fn plan_runs_full_suite_for_unexecuted_code_when_coverage_is_incomplete() {
        let unexecuted = Span::quad(6, 5, 6, 6);
        assert_eq!(
            coverage(vec![], 2).plan(lib(), unexecuted),
            Plan::FullSuite(FullSuiteReason::Doctests)
        );
        assert_eq!(
            coverage(vec![run("hangs", 60.0, Isolated::TimedOut, &[])], 0).plan(lib(), unexecuted),
            Plan::FullSuite(FullSuiteReason::IncompleteProfiles)
        );
        assert_eq!(
            coverage(vec![run("no_profile", 0.1, Isolated::NoProfile, &[])], 0)
                .plan(lib(), unexecuted),
            Plan::FullSuite(FullSuiteReason::IncompleteProfiles)
        );
        // A test that fails alone stops early, perhaps before code it runs in the suite.
        assert_eq!(
            coverage(vec![run("alone_fails", 0.1, Isolated::Failed, &["add"])], 0)
                .plan(lib(), unexecuted),
            Plan::FullSuite(FullSuiteReason::IncompleteProfiles)
        );
        // A function outside the tree, like a closure generated by a dependency's
        // macro, is known, so running it doesn't make coverage incomplete.
        let mut functions = vec![MappedFunction {
            name: "external".to_owned(),
            regions: vec![],
        }];
        functions.extend([
            function("add", "src/lib.rs", Span::quad(1, 1, 3, 2)),
            function("unused", "src/lib.rs", Span::quad(5, 1, 7, 2)),
        ]);
        let external = selection(
            &functions,
            vec![run("t", 0.1, Isolated::Passed, &["add", "external"])],
            0,
        );
        assert!(external.unattributed_functions().is_empty());
        assert_eq!(external.plan(lib(), unexecuted), Plan::Uncovered);
        let child = coverage(vec![run("spawns", 0.1, Isolated::Passed, &["unmapped"])], 0);
        assert_eq!(child.unattributed_functions(), ["unmapped"]);
        assert_eq!(
            child.plan(lib(), unexecuted),
            Plan::FullSuite(FullSuiteReason::UnattributedExecutions)
        );
        // Selection of covering tests is still sound when coverage is incomplete.
        assert_eq!(
            child.plan(lib(), Span::quad(2, 5, 2, 6)),
            Plan::Selected(vec![1, 0])
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // a list of one range is intended
    fn batches_double_in_size() {
        assert!(batches(0).is_empty());
        assert_eq!(batches(1), [0..1]);
        assert_eq!(batches(6), [0..1, 1..3, 3..6]);
        assert_eq!(batches(7), [0..1, 1..3, 3..7]);
        assert_eq!(batches(8), [0..1, 1..3, 3..7, 7..8]);
    }

    fn command(manifest_dir: &str, program: &str) -> ReplayCommand {
        ReplayCommand {
            env: vec![("CARGO_MANIFEST_DIR".to_owned(), manifest_dir.to_owned())],
            argv: vec![program.to_owned()],
            cwd: manifest_dir.into(),
            idle: false,
        }
    }

    #[test]
    fn same_test_binaries_ignores_hashes_and_target_dirs() {
        let schema = [
            command("/ws/a", "/ws/target/debug/deps/a-1111"),
            command("/ws/a", "rustdoc"),
        ];
        let coverage = [
            command("/ws/a", "/ws/target/mutants-coverage/debug/deps/a-2222"),
            command("/ws/a", "/rust/bin/rustdoc"),
        ];
        let ws = [Utf8PathBuf::from("/ws")];
        assert!(same_test_binaries(&schema, &ws, &coverage, &ws));
        assert!(!same_test_binaries(&schema, &ws, &coverage[..1], &ws));
        assert!(!same_test_binaries(
            &schema[..1],
            &ws,
            &[command("/ws/b", "/ws/target/debug/deps/a-2222")],
            &ws
        ));
        assert!(!same_test_binaries(
            &schema[..1],
            &ws,
            &[command("/ws/a", "/ws/target/debug/deps/b-1111")],
            &ws
        ));
    }

    #[test]
    fn same_test_binaries_compares_package_directories_relative_to_either_root_of_each_tree() {
        // Cargo reports the canonical path of a tree whose root is given through a
        // symlink, as a temporary directory on macOS is.
        let schema_roots = [
            Utf8PathBuf::from("/tmp/schema"),
            Utf8PathBuf::from("/private/tmp/schema"),
        ];
        let coverage_roots = [
            Utf8PathBuf::from("/tmp/coverage"),
            Utf8PathBuf::from("/private/tmp/coverage"),
        ];
        let schema = [command(
            "/private/tmp/schema/a",
            "/private/tmp/schema/target/debug/deps/a-1111",
        )];
        let coverage = [command(
            "/tmp/coverage/a",
            "/tmp/coverage/target/debug/deps/a-2222",
        )];
        assert!(same_test_binaries(
            &schema,
            &schema_roots,
            &coverage,
            &coverage_roots
        ));
        let other_package = [command(
            "/tmp/coverage/b",
            "/tmp/coverage/target/debug/deps/a-2222",
        )];
        assert!(!same_test_binaries(
            &schema,
            &schema_roots,
            &other_package,
            &coverage_roots
        ));
    }

    #[test]
    fn confirm_parse_defaults_to_reached() {
        assert_eq!(Confirm::parse(None), Confirm::Reached);
        assert_eq!(Confirm::parse(Some("reached")), Confirm::Reached);
        assert_eq!(Confirm::parse(Some("passed")), Confirm::Passed);
        assert_eq!(Confirm::parse(Some("all")), Confirm::All);
        assert_eq!(Confirm::parse(Some("none")), Confirm::None);
        assert_eq!(Confirm::parse(Some("sometimes")), Confirm::Reached);
    }
}
