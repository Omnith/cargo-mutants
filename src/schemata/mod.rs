// Copyright 2026 Martin Pool

//! Experimental mutant schemata (`--schemata`): build once, select mutants at runtime.
//!
//! Instead of rewriting the source and rebuilding for every mutant, this embeds all
//! supported mutants into one program (the *schema*), each guarded by a check of a
//! mutant id read from the `CARGO_MUTANTS_SCHEMATA_ID` environment variable at
//! runtime. The schema is built once, and each mutant is tested by running the same
//! tests as the classic path with a different id.
//!
//! The stages are:
//!
//! 1. Plan: decide which mutants can be embedded (see [`plan`]); the rest *fall back*.
//! 2. Check: `cargo check --tests` the schema, dropping mutants blamed for compile
//!    errors and repeating until it's clean, at most [`MAX_CHECK_ITERATIONS`] times.
//! 3. Build: `cargo test --no-run`, with the same dropping if it fails.
//! 4. Baseline: run `cargo test -vv` with no mutant active; it must pass. Then replay
//!    the test commands it printed (see [`replay`]), except those that ran no tests,
//!    which must also pass.
//! 5. Choose how many mutants to test at once: `--jobs`, or else by timing concurrent
//!    runs of the tests (see [`jobs`]). Either way, check that concurrent runs pass.
//! 6. With `--test-selection=coverage`, or `auto` if the measured costs so far say
//!    it will save time (see [`coverage::decision`]), collect coverage of the
//!    unmutated tree in a copy of it seeded from the build directory, to select each
//!    mutant's tests (see [`coverage`]).
//! 7. Test each embedded mutant, in parallel, all in the same build directory, by
//!    replaying those commands with the mutant's id, or if they couldn't be replayed,
//!    by running `cargo test`. Missed mutants whose outcome might differ the classic
//!    way, because tests might read their file's text or ran code without the id,
//!    become fallback mutants.
//! 8. Restore the original source and test the fallback mutants the classic way.
//!
//! Timing and counts are written to `mutants.out/schemata.json`.

pub(crate) mod coverage;
mod diagnostics;
mod embed;
mod generate;
mod jobs;
mod markers;
mod plan;
mod reads;
mod replay;
mod run;

use std::cmp::max;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::read_to_string;
use std::io::Write;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use anyhow::{Context, bail};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{DependencyKind, TargetKind};
use itertools::Itertools;
use jiff::Timestamp;
use serde::Serialize;
use tracing::{debug, error, info, warn};

use self::coverage::collect::{Collected, CollectionReport, LlvmTools, collect, isolated_workers};
use self::coverage::decision::{CostInputs, Decision, decide, mutant_seconds, rebuild_seconds};
use self::coverage::{
    Confirm, FullSuiteReason, Plan, UnobservedFiles, same_test_binaries, tree_roots,
};
use self::embed::{Blame, Embedding, proven_unviable};
use self::jobs::Probe;
use self::markers::Markers;
use self::plan::FallbackReason;
use self::reads::named_files;
use self::replay::ReplayCommand;
use self::run::{Baseline, MutantTest, Pass, Runner, Selector, TestExec, Tested, Work};
use crate::build_dir::BuildDir;
use crate::console::Console;
use crate::lab::{TestsForMutant, make_jobserver, test_mutants_after_baseline};
use crate::mutant::{Genre, Mutant};
use crate::options::{Choice, Options, TestSelection, TestTool};
use crate::outcome::{LabOutcome, Phase, PhaseResult, ScenarioOutcome};
use crate::output::{OutputDir, replace_json_file};
use crate::package::PackageSelection;
use crate::path::Utf8PathSlashes;
use crate::scenario::Scenario;
use crate::timeouts::Timeouts;
use crate::workspace::Workspace;
use crate::{BaselineStrategy, Result};

/// Maximum number of `cargo check` passes that drop mutants from the schema.
///
/// Each failing pass drops at least one mutant (or everything), so the loop always
/// makes progress; the cap bounds the time spent on pathological cases, after which
/// every remaining mutant falls back to the classic path.
pub(crate) const MAX_CHECK_ITERATIONS: usize = 8;

/// Maximum number of `cargo test --no-run` passes, for errors that only appear when
/// building, like post-monomorphization errors.
pub(crate) const MAX_BUILD_ITERATIONS: usize = 3;

/// Environment variable choosing the phase used for the check-and-drop loop:
/// `build` (the default), which reuses the loop's output as the build, or `check`.
///
/// `build` compiles dependencies once, where `check` followed by a build compiles
/// them twice (metadata-only, then with codegen): on backend-core it cut the fixed
/// cost by 12-17 s. `check` may be faster when many drop passes are needed, since
/// each extra pass is then a check rather than a build.
const CHECK_PHASE_ENV: &str = "CARGO_MUTANTS_SCHEMATA_CHECK_PHASE";

/// Environment variable choosing how each mutant's tests run: `direct` (the
/// default) replays the test commands captured from the baseline `cargo test`
/// without running cargo; `cargo` runs `cargo test` for each mutant.
///
/// Concurrent `cargo test` runs in one build directory wait for each other on
/// cargo's locks while cargo checks freshness, which direct replay avoids.
const EXEC_ENV: &str = "CARGO_MUTANTS_SCHEMATA_EXEC";

/// Environment variable choosing what happens to mutants dropped from the schema
/// for compile errors that prove them unviable (see [`embed::proven_unviable`]):
/// `unviable` (the default) records them as unviable without building them again;
/// `classic` tests them the classic way like other fallback mutants.
///
/// On one crate, all 458 mutants recorded unviable this way were also unviable when
/// built the classic way.
const DROPPED_ENV: &str = "CARGO_MUTANTS_SCHEMATA_DROPPED";

/// Environment variable that, set to `build`, stops after the schema's check and
/// build passes, without testing any mutants, for diagnosing why mutants drop.
const STOP_AFTER_ENV: &str = "CARGO_MUTANTS_SCHEMATA_STOP_AFTER";

/// Said when coverage-based test selection is the default, but can't be used.
const LLVM_TOOLS_NOT_FOUND: &str = "Running all tests for each mutant: coverage-based test selection \
    needs llvm-tools for this tree's toolchain (rustup component add llvm-tools)";

/// Name of the file in `mutants.out` holding the [`Report`].
const SCHEMATA_JSON: &str = "schemata.json";

/// Settings for schemata that come from environment variables: advanced knobs for
/// troubleshooting and experiments, listed in the book, not command-line options.
///
/// Every such variable is parsed here, once per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnvSettings {
    /// The phase used for the check-and-drop loop, from [`CHECK_PHASE_ENV`].
    check_phase: Phase,
    /// Whether to replay the baseline's test commands rather than run `cargo test` for
    /// each mutant, from [`EXEC_ENV`].
    direct: bool,
    /// What happens to mutants proven unviable, from [`DROPPED_ENV`].
    dropped: Dropped,
    /// Whether to stop after the schema is built, from [`STOP_AFTER_ENV`].
    stop_after_build: bool,
    /// Which mutants coverage-based selection confirms with all the tests, from
    /// [`coverage::CONFIRM_ENV`].
    confirm: Confirm,
    /// How coverage-based selection tests mutants in files where no code ran, from
    /// [`coverage::UNOBSERVED_FILES_ENV`].
    unobserved_files: UnobservedFiles,
}

/// What happens to fallback mutants whose compile errors prove them unviable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropped {
    /// Test them the classic way, like other fallback mutants.
    Classic,
    /// Record them as unviable without building them again.
    Unviable,
}

impl Dropped {
    fn name(self) -> &'static str {
        match self {
            Dropped::Classic => "classic",
            Dropped::Unviable => "unviable",
        }
    }
}

impl EnvSettings {
    fn from_env() -> EnvSettings {
        EnvSettings::parse(|name| std::env::var(name).ok())
    }

    /// Parse the settings from the environment variables that `var` looks up, using
    /// the default for any that's unset or not recognized.
    fn parse(var: impl Fn(&str) -> Option<String>) -> EnvSettings {
        let check_phase = match var(CHECK_PHASE_ENV).as_deref() {
            None | Some("build") => Phase::Build,
            Some("check") => Phase::Check,
            Some(other) => {
                warn!("Unknown {CHECK_PHASE_ENV}={other:?}: expected check or build; using build");
                Phase::Build
            }
        };
        let direct = match var(EXEC_ENV).as_deref() {
            None | Some("direct") => true,
            Some("cargo") => false,
            Some(other) => {
                warn!("Unknown {EXEC_ENV}={other:?}: expected direct or cargo; using direct");
                true
            }
        };
        let dropped = match var(DROPPED_ENV).as_deref() {
            None | Some("unviable") => Dropped::Unviable,
            Some("classic") => Dropped::Classic,
            Some(other) => {
                warn!(
                    "Unknown {DROPPED_ENV}={other:?}: expected unviable or classic; using unviable"
                );
                Dropped::Unviable
            }
        };
        let stop_after_build = match var(STOP_AFTER_ENV).as_deref() {
            None => false,
            Some("build") => true,
            Some(other) => {
                warn!("Unknown {STOP_AFTER_ENV}={other:?}: expected build; not stopping early");
                false
            }
        };
        EnvSettings {
            check_phase,
            direct,
            dropped,
            stop_after_build,
            confirm: Confirm::parse(var(coverage::CONFIRM_ENV).as_deref()),
            unobserved_files: UnobservedFiles::parse(
                var(coverage::UNOBSERVED_FILES_ENV).as_deref(),
            ),
        }
    }
}

/// The option in effect that schemata don't support, as it's given on the command
/// line, if there is one.
pub(crate) fn unsupported_option(options: &Options) -> Option<&'static str> {
    if options.test_tool() != TestTool::Cargo {
        Some("--test-tool=nextest")
    } else if options.in_place {
        Some("--in-place")
    } else if options.check_only {
        Some("--check")
    } else if options.baseline != BaselineStrategy::Run {
        Some("--baseline=skip")
    } else {
        None
    }
}

/// Decide whether to test mutants with a schema.
///
/// Schemata are used unless they're turned off, or an option they don't support is
/// in effect, in which case this logs one line saying so.
///
/// # Errors
///
/// If `--schemata`, or `--test-selection=coverage`, which needs schemata, is given on
/// the command line when schemata can't be used.
pub(crate) fn enabled(options: &Options) -> Result<bool> {
    let unsupported = unsupported_option(options);
    let enabled = match (options.schemata, unsupported) {
        (Choice { value: false, .. }, _) => false,
        (_, None) => true,
        (
            Choice {
                on_command_line: true,
                ..
            },
            Some(option),
        ) => bail!("--schemata can't be used with {option}"),
        (_, Some(option)) => {
            info!("Not using schemata, since they don't support {option}");
            false
        }
    };
    let coverage_on_command_line = Choice {
        value: TestSelection::Coverage,
        on_command_line: true,
    };
    if !enabled && options.test_selection == coverage_on_command_line {
        match unsupported.filter(|_| options.schemata.value) {
            Some(option) => bail!(
                "--test-selection=coverage requires --schemata, which can't be used with {option}"
            ),
            None => bail!("--test-selection=coverage requires --schemata"),
        }
    }
    Ok(enabled)
}

/// Counts and timings written to `mutants.out/schemata.json`.
#[derive(Debug, Default, Serialize)]
struct Report {
    cargo_mutants_version: String,
    start_time: Option<Timestamp>,
    mutants: usize,
    embedded: usize,
    fallback: usize,
    embedded_by_genre: BTreeMap<String, usize>,
    fallback_by_genre: BTreeMap<String, usize>,
    fallback_by_reason: BTreeMap<FallbackReason, usize>,
    /// Embedded mutants before any were dropped for compile errors.
    initially_embedded: usize,
    check_phase: String,
    check_passes: Vec<Pass>,
    check_seconds: f64,
    build_passes: Vec<Pass>,
    build_seconds: f64,
    /// Duration of the baseline `cargo test`.
    baseline_test_seconds: f64,
    /// Duration of replaying the baseline's test commands directly, if done.
    direct_baseline_test_seconds: Option<f64>,
    test_timeout_seconds: Option<f64>,
    jobs: usize,
    /// How each mutant's tests are run: `direct` or `cargo_test`.
    test_exec: String,
    /// Number of test commands captured from the baseline, in direct mode.
    replay_commands: usize,
    /// Number of captured test commands that ran no tests in the baseline, and so
    /// are not replayed for each mutant.
    idle_commands_skipped: usize,
    mutant_tests: Vec<MutantTest>,
    mutant_tests_wall_seconds: f64,
    /// For each lock cargo reported waiting for (like "build directory" or "package
    /// cache"), the number of mutant tests that waited for it.
    lock_waits: BTreeMap<String, usize>,
    /// Number of mutant tests where cargo recompiled something.
    unexpected_rebuilds: usize,
    fallback_mutants: Vec<FallbackMutant>,
    /// Fallback mutants whose compile errors prove them unviable.
    proven_unviable: usize,
    /// What happened to proven-unviable mutants: `classic` or `unviable`.
    dropped_mutants: &'static str,
    /// For each reason that mutants fell back, how many were tested the classic way,
    /// and the time spent on them.
    fallback_time_by_reason: BTreeMap<FallbackReason, FallbackTime>,
    fallback_wall_seconds: f64,
    wall_seconds: f64,
    /// Files that string literals in their package name, so that tests might read
    /// them: their mutants are tested again the classic way if the schema misses them.
    source_read_files: Vec<String>,
    /// Number of mutants embedded in `source_read_files` after the schema was built.
    source_read_mutants: usize,
    /// Number of mutants tested at once: `--jobs` if given, or else chosen by
    /// `jobs_probe`.
    test_jobs: usize,
    /// Without `--jobs`, timings of increasing numbers of concurrent test runs with no
    /// mutant active, used to choose `test_jobs`.
    jobs_probe: Vec<Probe>,
    /// Whether the tests pass when several copies run at once with no mutant active:
    /// `--jobs` copies, or every number probed; if not, mutants are tested one at a
    /// time. Not checked if only one job was run.
    concurrent_baseline_passed: Option<bool>,
    /// Executables that ran schema code without the mutant id, by the end of the
    /// baseline. If there are any, missed mutants are tested the classic way.
    env_cleared_executables: Vec<String>,
    /// Executables that first ran schema code without the mutant id while mutants
    /// were tested.
    env_cleared_executables_during_mutants: Vec<String>,
    /// Mutants whose tests failed although their code never ran, so were tested
    /// again with nothing else running.
    retested_unreached: usize,
    /// Number of test processes that recorded running schema code with the mutant id
    /// set to 0, in the baseline. If there are none, it's not shown that tests see
    /// the id, and every mutant is tested the classic way.
    baseline_processes: usize,
    /// Embedded mutants whose code the schema recorded running in the baseline.
    ran_in_baseline_mutants: usize,
    /// With `--test-selection=auto` and llvm-tools, the estimated costs that decided
    /// whether to collect coverage.
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage_decision: Option<Decision>,
    /// If coverage was collected, or its collection was attempted, how tests were
    /// selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    test_selection: Option<SelectionSummary>,
}

/// Summary of coverage-based test selection, in `schemata.json`.
#[derive(Debug, Serialize)]
struct SelectionSummary {
    confirm: Confirm,
    unobserved_files: UnobservedFiles,
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<CollectionReport>,
    /// Why coverage couldn't be collected, if it couldn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    collection_error: Option<String>,
    /// Package selections whose coverage matches their test commands, and those
    /// without usable coverage.
    selections_with_coverage: usize,
    selections_without_coverage: usize,
    /// Embedded mutants by plan: `selected`, `uncovered`, or `full_suite`.
    plans: BTreeMap<&'static str, usize>,
    full_suite_reasons: BTreeMap<FullSuiteReason, usize>,
    /// Means over mutants with selected tests.
    mean_selected_tests: f64,
    mean_total_tests: f64,
    mean_tests_run: f64,
    selected_seconds: f64,
    confirmations: usize,
    confirm_seconds: f64,
    /// Confirmations whose verdict differs from the selected tests' (or from
    /// "uncovered").
    verdict_changes: usize,
    verdict_changed_mutants: Vec<String>,
    /// Uncovered mutants whose code the schema recorded running in the baseline: no
    /// test's coverage showed it, perhaps because the process that ran it was killed.
    uncovered_ran_in_baseline: usize,
}

impl SelectionSummary {
    fn new(confirm: Confirm, unobserved_files: UnobservedFiles) -> SelectionSummary {
        SelectionSummary {
            confirm,
            unobserved_files,
            collection: None,
            collection_error: None,
            selections_with_coverage: 0,
            selections_without_coverage: 0,
            plans: BTreeMap::new(),
            full_suite_reasons: BTreeMap::new(),
            mean_selected_tests: 0.0,
            mean_total_tests: 0.0,
            mean_tests_run: 0.0,
            selected_seconds: 0.0,
            confirmations: 0,
            confirm_seconds: 0.0,
            verdict_changes: 0,
            verdict_changed_mutants: Vec::new(),
            uncovered_ran_in_baseline: 0,
        }
    }

    /// Summarize how each mutant's tests were selected.
    #[allow(clippy::cast_precision_loss)] // counts are far below 2^52
    fn add_mutant_tests(&mut self, mutant_tests: &[MutantTest]) {
        let records = mutant_tests
            .iter()
            .filter_map(|t| Some((t, t.selection.as_ref()?)))
            .collect_vec();
        self.plans = records
            .iter()
            .map(|(_, r)| r.plan)
            .counts()
            .into_iter()
            .collect();
        self.full_suite_reasons = records
            .iter()
            .filter_map(|(_, r)| r.full_suite_reason)
            .counts()
            .into_iter()
            .collect();
        let selected = records
            .iter()
            .filter(|(_, r)| r.plan == "selected")
            .map(|(_, r)| r)
            .collect_vec();
        let mean = |f: &dyn Fn(&run::MutantSelection) -> usize| {
            if selected.is_empty() {
                0.0
            } else {
                selected.iter().map(|r| f(r)).sum::<usize>() as f64 / selected.len() as f64
            }
        };
        self.mean_selected_tests = mean(&|r| r.selected_tests);
        self.mean_total_tests = mean(&|r| r.total_tests);
        self.mean_tests_run = mean(&|r| r.tests_run);
        self.selected_seconds = records.iter().map(|(_, r)| r.selected_seconds).sum();
        self.confirmations = records.iter().filter(|(_, r)| r.confirmed).count();
        self.confirm_seconds = records.iter().map(|(_, r)| r.confirm_seconds).sum();
        self.verdict_changed_mutants = records
            .iter()
            .filter(|(_, r)| r.verdict_changed)
            .map(|(t, _)| t.name.clone())
            .collect();
        self.verdict_changes = self.verdict_changed_mutants.len();
        self.uncovered_ran_in_baseline = records
            .iter()
            .filter(|(_, r)| r.ran_in_baseline == Some(true))
            .count();
    }
}

#[derive(Debug, Serialize)]
struct FallbackMutant {
    name: String,
    genre: Genre,
    reason: FallbackReason,
    /// The distinct compile errors that dropped this mutant from the schema.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blame: Vec<Blame>,
    /// The errors prove the mutant unviable, without building it classically.
    proven_unviable: bool,
}

/// The fallback mutants with one reason that were tested the classic way.
#[derive(Debug, Default, Clone, Copy, Serialize)]
struct FallbackTime {
    count: usize,
    /// Sum of the durations of all their phases.
    seconds: f64,
}

impl Report {
    /// Update the counts of embedded and fallback mutants.
    fn count(&mut self, embedding: &Embedding) {
        let genre_name = |m: &Mutant| format!("{:?}", m.genre);
        self.embedded_by_genre = embedding
            .embedded()
            .map(|(_, m)| genre_name(m))
            .counts()
            .into_iter()
            .collect();
        self.fallback_by_genre = embedding
            .fallback()
            .map(|(m, _)| genre_name(m))
            .counts()
            .into_iter()
            .collect();
        self.fallback_by_reason = embedding
            .fallback()
            .map(|(_, reason)| reason)
            .counts()
            .into_iter()
            .collect();
        self.embedded = embedding.embedded().count();
        self.fallback = embedding.fallback().count();
        self.fallback_mutants = embedding
            .fallback_blamed()
            .map(|(m, reason, blame)| FallbackMutant {
                name: m.name(true),
                genre: m.genre.clone(),
                reason,
                blame: blame.to_vec(),
                proven_unviable: proven_unviable(reason, blame),
            })
            .collect();
        self.proven_unviable = self
            .fallback_mutants
            .iter()
            .filter(|m| m.proven_unviable)
            .count();
    }

    /// Write `schemata.json`, atomically replacing the previous version.
    ///
    /// It's only a report, so failing to write it is a warning, not a reason to stop
    /// testing or to lose the mutants' outcomes.
    fn write(&self, output_dir: &Utf8Path) {
        if let Err(err) = replace_json_file(output_dir, SCHEMATA_JSON, self) {
            warn!("Failed to write {SCHEMATA_JSON}: {err:#}");
        }
    }

    /// Emit the one-off cost of checking and building the schema as a structured event.
    ///
    /// This is a fixed cost shared by all the embedded mutants, which have only a test
    /// phase, so it's emitted just before the other `timing.*` events in `debug.log`.
    fn trace_schema_build(&self) {
        debug!(
            check_secs = self.check_seconds,
            check_passes = self.check_passes.len(),
            build_secs = self.build_seconds,
            build_passes = self.build_passes.len(),
            total_secs = self.check_seconds + self.build_seconds,
            "timing.schema_build"
        );
    }

    /// Record the time spent testing fallback mutants the classic way, by reason, and
    /// emit it as `timing.schema_fallback` events with `reason`, `count`, and
    /// `total_secs`, one for each reason.
    fn record_fallback_time(&mut self, embedding: &Embedding, lab_outcome: &LabOutcome) {
        self.fallback_time_by_reason = fallback_time_by_reason(embedding, lab_outcome);
        for (reason, time) in &self.fallback_time_by_reason {
            debug!(
                ?reason,
                count = time.count,
                total_secs = time.seconds,
                "timing.schema_fallback"
            );
        }
    }
}

/// Sum the time spent testing each fallback mutant the classic way, by reason.
///
/// Only mutants with an outcome in `lab_outcome` are counted, so those not tested
/// because the run was interrupted are left out.
fn fallback_time_by_reason(
    embedding: &Embedding,
    lab_outcome: &LabOutcome,
) -> BTreeMap<FallbackReason, FallbackTime> {
    let reasons: HashMap<String, FallbackReason> = embedding
        .fallback()
        .map(|(mutant, reason)| (mutant.name(true), reason))
        .collect();
    let mut times = BTreeMap::<FallbackReason, FallbackTime>::new();
    for scenario_outcome in &lab_outcome.outcomes {
        let Some(reason) = scenario_outcome
            .scenario
            .mutant()
            .and_then(|mutant| reasons.get(&mutant.name(true)))
        else {
            continue;
        };
        let time = times.entry(*reason).or_default();
        time.count += 1;
        time.seconds += scenario_outcome
            .phase_results()
            .iter()
            .map(|pr| pr.duration.as_secs_f64())
            .sum::<f64>();
    }
    times
}

/// Test mutants using a schema, falling back to the classic path for mutants that
/// can't be embedded.
pub(crate) fn test_mutants(
    mut mutants: Vec<Mutant>,
    workspace: &Workspace,
    output_dir: OutputDir,
    options: &Options,
    console: &Console,
) -> Result<LabOutcome> {
    let start_time = Instant::now();
    if let Some(option) = unsupported_option(options) {
        bail!("--schemata can't be used with {option}");
    }
    let env_settings = EnvSettings::from_env();
    let test_selection = options.test_selection;
    let llvm_tools = match test_selection.value {
        TestSelection::All => None,
        TestSelection::Coverage | TestSelection::Auto => match LlvmTools::find(workspace.root()) {
            Ok(tools) => Some(tools),
            Err(err)
                if test_selection.on_command_line
                    && test_selection.value == TestSelection::Coverage =>
            {
                return Err(err);
            }
            Err(err) => {
                debug!("LLVM tools not found: {err:#}");
                info!("{LLVM_TOOLS_NOT_FOUND}");
                None
            }
        },
    };
    let EnvSettings {
        confirm,
        unobserved_files,
        ..
    } = env_settings;
    // As the classic way does. Embedded mutants are numbered, and so tested, in this
    // order, and fallback mutants are tested in it too.
    if options.shuffle {
        fastrand::shuffle(&mut mutants);
    }
    output_dir.write_mutants_list(&mutants)?;
    console.discovered_mutants(&mutants);
    if mutants.is_empty() {
        warn!("No mutants found under the active filters");
        return Ok(LabOutcome::new(Timestamp::now()));
    }
    let output_path = output_dir.path().to_owned();
    let mut report = Report {
        cargo_mutants_version: crate::VERSION.to_owned(),
        start_time: Some(Timestamp::now()),
        mutants: mutants.len(),
        jobs: options.jobs.unwrap_or(1),
        ..Report::default()
    };
    let tests_for_mutant = TestsForMutant::new(options, workspace);
    let markers = Markers::new()?;
    let packages = plan_packages(workspace, &mutants)?;
    let mut embedding =
        Embedding::new(mutants, packages.roots, &packages.fallbacks, markers.path())?;
    report.source_read_files = packages.read_files.iter().cloned().collect();
    if !report.source_read_files.is_empty() {
        debug!(
            files = ?report.source_read_files,
            "schemata.source_read_by_tests"
        );
    }
    report.count(&embedding);
    report.initially_embedded = report.embedded;
    debug!(
        mutants = report.mutants,
        embedded = report.embedded,
        fallback = report.fallback,
        "schemata.plan.done"
    );
    report.write(&output_path);

    let build_dir = BuildDir::copy_from(workspace.root(), options, console)?;
    let output_mutex = Mutex::new(output_dir);
    let jobserver = make_jobserver(options)?;
    let runner = Runner {
        build_dir: &build_dir,
        jobserver: jobserver.as_ref(),
        output: &output_mutex,
        options,
        console,
        markers: &markers,
        exclusive: RwLock::new(()),
    };
    let embedded_packages = PackageSelection::Explicit(
        embedding
            .embedded()
            .map(|(_, m)| m.source_file.package.clone())
            .unique_by(|p| p.name.clone())
            .collect(),
    );
    let check_phase = env_settings.check_phase;
    report.check_phase = check_phase.to_string();
    report.check_passes = runner.drop_until_clean(
        &mut embedding,
        check_phase,
        &[embedded_packages],
        MAX_CHECK_ITERATIONS,
    )?;
    report.check_seconds = report.check_passes.iter().map(|p| p.seconds).sum();
    report.count(&embedding);
    report.write(&output_path);

    // Build each distinct test selection separately, so that each later `cargo test`
    // finds its own feature-unified build up to date.
    let selections = distinct_selections(&embedding, &tests_for_mutant, options);
    report.build_passes = runner.drop_until_clean(
        &mut embedding,
        Phase::Build,
        &selections,
        MAX_BUILD_ITERATIONS,
    )?;
    report.build_seconds = report.build_passes.iter().map(|p| p.seconds).sum();
    report.count(&embedding);
    report.write(&output_path);
    debug!(
        check_seconds = report.check_seconds,
        build_seconds = report.build_seconds,
        embedded = report.embedded,
        fallback = report.fallback,
        proven_unviable = report.proven_unviable,
        "schemata.build.done"
    );

    report.dropped_mutants = env_settings.dropped.name();
    let originals = embedding.originals();
    if env_settings.stop_after_build {
        info!("Stopped after building the schema, as {STOP_AFTER_ENV}=build requested");
        runner.write_files(&originals)?;
        report.wall_seconds = start_time.elapsed().as_secs_f64();
        report.write(&output_path);
        return output_mutex
            .into_inner()
            .expect("unlock output dir")
            .finish();
    }
    report.write(&output_path);
    if embedding.embedded().next().is_none() {
        warn!("No mutants could be embedded in the schema; testing all of them the classic way");
        runner.write_files(&originals)?;
        drop(build_dir);
        return test_all_classically(
            &embedding,
            workspace,
            output_mutex,
            options,
            console,
            &mut report,
            start_time,
        );
    }

    let want_direct = env_settings.direct;
    // The schema's check and build are the baseline's build phase.
    let build_phase_result = PhaseResult {
        phase: Phase::Build,
        duration: std::time::Duration::from_secs_f64(report.check_seconds + report.build_seconds),
        process_status: crate::process::Exit::Success,
        argv: Vec::new(),
    };
    let (baseline, captured, known_tests, baseline_logs) = match runner.baseline(
        &selections,
        &originals,
        want_direct,
    )? {
        Baseline::Passed {
            result,
            commands,
            known_tests,
            logs,
        } => (result, commands, known_tests, logs),
        Baseline::Failed(original, log_path) => {
            let mut output_dir = output_mutex.into_inner().expect("unlock output dir");
            let outcome = record_baseline(
                &mut output_dir,
                [build_phase_result, original],
                baseline_build_logs(&report),
                &[log_path],
            )?;
            console.scenario_finished(build_dir.path(), &Scenario::Baseline, &outcome, options);
            error!("cargo test failed in an unmutated tree, so no mutants were tested");
            drop(build_dir);
            report.trace_schema_build();
            report.wall_seconds = start_time.elapsed().as_secs_f64();
            report.write(&output_path);
            return output_dir.finish();
        }
        Baseline::SchemaChangesBehavior(log_path) => {
            warn!(
                %log_path,
                "Tests fail with the schema and no mutant active, but pass on the unmutated tree, \
                perhaps because they read the source files; testing all mutants the classic way"
            );
            embedding.fall_back_all(FallbackReason::SchemaChangesBehavior);
            report.count(&embedding);
            drop(build_dir);
            return test_all_classically(
                &embedding,
                workspace,
                output_mutex,
                options,
                console,
                &mut report,
                start_time,
            );
        }
    };
    report.baseline_test_seconds = baseline.duration.as_secs_f64();
    let exec = match captured {
        Some(commands) => {
            report.replay_commands = commands.iter().map(Vec::len).sum();
            report.idle_commands_skipped = commands.iter().flatten().filter(|c| c.idle).count();
            if let Some(duration) = runner.replay_baseline(&commands)? {
                report.direct_baseline_test_seconds = Some(duration.as_secs_f64());
                TestExec::Direct(commands)
            } else {
                warn!(
                    "Replayed tests fail with no mutant active; running cargo test for each mutant instead"
                );
                TestExec::CargoTest
            }
        }
        None if want_direct => {
            warn!("Could not capture test commands; running cargo test for each mutant instead");
            TestExec::CargoTest
        }
        None => TestExec::CargoTest,
    };
    exec.name().clone_into(&mut report.test_exec);
    let baseline_outcome = record_baseline(
        &mut output_mutex.lock().expect("lock output dir"),
        [build_phase_result, baseline],
        baseline_build_logs(&report),
        &baseline_logs,
    )?;
    console.scenario_finished(
        build_dir.path(),
        &Scenario::Baseline,
        &baseline_outcome,
        options,
    );
    // Like other mutants, those proven unviable only get outcomes once the baseline
    // passes. If it isn't run here, the classic way tests them.
    if env_settings.dropped == Dropped::Unviable {
        for (mutant, blame) in embedding.take_proven_unviable() {
            runner.record_unviable(&mutant, check_phase, &blame)?;
        }
    }
    let timeouts = Timeouts::from_baseline(&baseline_outcome, options);
    report.test_timeout_seconds = timeouts.test.map(|t| t.as_secs_f64());
    debug!(
        seconds = report.baseline_test_seconds,
        direct_seconds = report.direct_baseline_test_seconds,
        timeout = report.test_timeout_seconds,
        test_exec = report.test_exec,
        "schemata.baseline.done"
    );
    // Each test process that runs schema code records that it saw the mutant id. If
    // none did, the tests might not see it, as in a sandbox, and then every mutant
    // would look missed.
    report.baseline_processes = markers.ran_in_baseline()?.processes;
    debug!(
        processes = report.baseline_processes,
        "schemata.baseline.markers"
    );
    if report.baseline_processes == 0 {
        warn!(
            "No test process recorded running the tree's code with the mutant id, perhaps \
            because the tests run in a sandbox; testing all mutants the classic way"
        );
        embedding.fall_back_all(FallbackReason::MarkersNotRecorded);
        report.count(&embedding);
    }

    let selection_keys = selections
        .iter()
        .map(|s| crate::cargo::cargo_argv(s, Phase::Test, options))
        .collect_vec();
    // Tests might read the text of these files: with the schema it's the same for
    // every mutant, so a caught mutant was caught by its behavior, as it would be the
    // classic way; but the classic way the mutated text is read, which might make
    // tests fail, so a missed mutant is tested again that way.
    let mut work = embedding
        .embedded()
        .map(|(id, mutant)| {
            let key =
                crate::cargo::cargo_argv(&tests_for_mutant.selection(mutant), Phase::Test, options);
            let selection = selection_keys
                .iter()
                .position(|k| *k == key)
                .expect("mutant's selection was built");
            let classic_if_missed = packages
                .read_files
                .contains(&mutant.source_file.tree_relative_path.to_slash_path())
                .then_some(FallbackReason::SourceReadByTestsMissedRetest);
            Work {
                id,
                mutant: mutant.clone(),
                selection,
                classic_if_missed,
                // Known once coverage is collected, if it is.
                plan: None,
                // Known once the baseline's runs are finished.
                ran_in_baseline: false,
            }
        })
        .collect_vec();
    report.source_read_mutants = work
        .iter()
        .filter(|w| w.classic_if_missed.is_some())
        .count();
    if report.source_read_mutants > 0 {
        debug!(
            mutants = report.source_read_mutants,
            "schemata.source_read_by_tests.embedded"
        );
    }
    // The selections come first for embedded mutants, so this many cover all of theirs.
    let embedded_selections = work.iter().map(|w| w.selection + 1).max().unwrap_or(0);

    // All jobs share the build directory, so check that the tests pass when that
    // many run at once, before relying on it. Without --jobs, the number is chosen by
    // running increasing numbers of copies at once, which checks the same.
    let embedded_selections = &selections[..embedded_selections];
    // A timed run of the tests in as many copies at once as `--jobs` says.
    let mut jobs_run = None;
    let concurrent_passed = if let Some(jobs) = options.jobs {
        report.test_jobs = jobs.clamp(1, work.len().max(1));
        if report.test_jobs > 1 {
            let start = Instant::now();
            let passed = runner.concurrent_baseline(
                embedded_selections,
                &exec,
                report.test_jobs,
                timeouts.test,
            )?;
            jobs_run = Some(Probe {
                jobs: report.test_jobs,
                seconds: start.elapsed().as_secs_f64(),
                passed,
            });
            Some(passed)
        } else {
            None
        }
    } else {
        let max_jobs = jobs::max_jobs(num_cpus::get(), work.len());
        if max_jobs > 1 {
            report.jobs_probe =
                runner.probe_jobs(embedded_selections, &exec, max_jobs, timeouts.test)?;
        }
        report.test_jobs = jobs::chosen(&report.jobs_probe);
        report
            .jobs_probe
            .iter()
            .any(|probe| probe.jobs > 1)
            .then(|| report.jobs_probe.iter().all(|probe| probe.passed))
    };
    report.concurrent_baseline_passed = concurrent_passed;
    if concurrent_passed == Some(false) {
        warn!(
            jobs = report
                .jobs_probe
                .last()
                .map_or(report.test_jobs, |p| p.jobs),
            "Tests fail when several copies run at once in the shared build directory, \
            so they may interfere with each other; testing mutants one at a time"
        );
        report.test_jobs = 1;
    }
    debug!(
        test_jobs = report.test_jobs,
        probed = !report.jobs_probe.is_empty(),
        "schemata.jobs.chosen"
    );

    // Processes that ran schema code without the id, like a binary run by a test that
    // cleared its environment, or a build script, ran the unmutated code; so a mutant
    // that's missed might have been caught by them. Those are tested the classic way.
    report.env_cleared_executables = markers.env_cleared_executables()?.into_iter().collect();
    if !report.env_cleared_executables.is_empty() {
        warn!(
            executables = ?report.env_cleared_executables,
            "Some tests run code of the tree without the mutant id, perhaps by clearing the environment; \
            missed mutants will be tested the classic way"
        );
        for item in &mut work {
            item.classic_if_missed
                .get_or_insert(FallbackReason::EnvironmentCleared);
        }
    }

    // Every run so far had no mutant active, including those that chose the jobs.
    let baseline_runs = markers.ran_in_baseline()?;
    for item in &mut work {
        item.ran_in_baseline = baseline_runs.ran.contains(&item.id);
    }
    report.ran_in_baseline_mutants = work.iter().filter(|w| w.ran_in_baseline).count();
    debug!(
        processes = baseline_runs.processes,
        mutants = report.ran_in_baseline_mutants,
        "schemata.baseline.ran"
    );

    // Coverage is collected only now, once the costs it's weighed against are known:
    // how long the tests and the build take, and how many mutants there are.
    let replay_commands = match &exec {
        TestExec::Direct(commands) => Some(commands),
        TestExec::CargoTest => None,
    };
    let collect_coverage = match (&llvm_tools, replay_commands, test_selection.value) {
        (None, _, _) | (_, _, TestSelection::All) => false,
        (Some(_), None, _) => {
            warn!("Coverage-based test selection needs replayed test commands; running all tests");
            false
        }
        (Some(_), Some(_), TestSelection::Coverage) => true,
        (Some(_), Some(_), TestSelection::Auto) => {
            let decision = decide(cost_inputs(
                &report,
                work.len(),
                jobs_run.as_ref(),
                embedded_selections.len(),
                &baseline_logs,
            )?);
            decision.trace();
            report.coverage_decision = Some(decision);
            decision.collect
        }
    };
    let collected = match (&llvm_tools, collect_coverage) {
        (Some(tools), true) => {
            let mut summary = SelectionSummary::new(confirm, unobserved_files);
            let collected = collect_in_copy(
                &runner,
                workspace,
                embedded_selections,
                tools,
                &mut summary,
                unobserved_files,
            )?;
            report.test_selection = Some(summary);
            Some(collected)
        }
        _ => None,
    };
    report.write(&output_path);
    let build_roots = tree_roots(build_dir.path());
    let selector = collected
        .as_ref()
        .zip(replay_commands)
        .map(|(collected, commands)| Selector {
            coverage: selection_keys
                .iter()
                .zip(commands)
                .map(|(key, commands)| usable_coverage(collected, key, commands, &build_roots))
                .collect(),
            confirm,
        });
    if let Some(selector) = &selector {
        for item in &mut work {
            item.plan = Some(match selector.coverage[item.selection] {
                Some(coverage) => coverage.plan(
                    &item.mutant.source_file.tree_relative_path,
                    item.mutant.span,
                ),
                None => Plan::FullSuite(FullSuiteReason::NoCoverage),
            });
        }
        if let Some(summary) = report.test_selection.as_mut() {
            summary.selections_with_coverage = selector.coverage.iter().flatten().count();
            summary.selections_without_coverage =
                selector.coverage.len() - summary.selections_with_coverage;
        }
    }

    if selector.is_some() {
        let plans = work
            .iter()
            .filter_map(|w| w.plan.as_ref())
            .map(|plan| match plan {
                Plan::Selected(_) => "selected",
                Plan::Uncovered => "uncovered",
                Plan::FullSuite(_) => "full_suite",
            })
            .counts();
        debug!(?plans, "schemata.coverage.plan.done");
    }

    console.start_testing_mutants(work.len());
    let tests_start = Instant::now();
    for tested in runner.test_embedded(
        work,
        &selections,
        &exec,
        selector.as_ref(),
        timeouts,
        report.test_jobs,
        &known_tests,
    )? {
        match tested {
            Tested::Recorded(test) => report.mutant_tests.push(test),
            Tested::NeedsClassic(id, reason) => embedding.fall_back(id, reason),
        }
    }
    report.mutant_tests_wall_seconds = tests_start.elapsed().as_secs_f64();
    report.count(&embedding);
    report.lock_waits = report
        .mutant_tests
        .iter()
        .flat_map(|t| t.lock_waits.iter().cloned())
        .counts()
        .into_iter()
        .collect();
    report.unexpected_rebuilds = report.mutant_tests.iter().filter(|t| t.rebuilt).count();
    report.retested_unreached = report.mutant_tests.iter().filter(|t| t.retested).count();
    if let Some(summary) = report.test_selection.as_mut() {
        summary.add_mutant_tests(&report.mutant_tests);
        debug!(
            plans = ?summary.plans,
            full_suite_reasons = ?summary.full_suite_reasons,
            mean_selected_tests = summary.mean_selected_tests,
            mean_total_tests = summary.mean_total_tests,
            confirmations = summary.confirmations,
            verdict_changes = summary.verdict_changes,
            "schemata.coverage.done"
        );
        write_uncovered_list(&output_path, &report.mutant_tests)?;
    }
    report.env_cleared_executables_during_mutants = markers
        .env_cleared_executables()?
        .into_iter()
        .filter(|exe| !report.env_cleared_executables.contains(exe))
        .collect();
    if !report.env_cleared_executables_during_mutants.is_empty() {
        warn!(
            executables = ?report.env_cleared_executables_during_mutants,
            "While mutants were tested, some tests ran code of the tree without the mutant id, \
            so some outcomes might be wrong"
        );
    }
    debug!(
        mutants = report.mutant_tests.len(),
        wall_seconds = report.mutant_tests_wall_seconds,
        lock_waits = ?report.lock_waits,
        unexpected_rebuilds = report.unexpected_rebuilds,
        retested_unreached = report.retested_unreached,
        "schemata.mutants.done"
    );
    report.write(&output_path);

    runner.write_files(&originals)?;
    let fallback = embedding.classic_fallback().cloned().collect_vec();
    let mut output_dir = output_mutex.into_inner().expect("unlock output dir");
    // Embedded mutants were tested with `test_jobs` workers, which might be fewer than
    // `--jobs` if tests interfere with each other.
    let mut workers = report.test_jobs;
    if !fallback.is_empty() {
        debug!(mutants = fallback.len(), "schemata.fallback.start");
        let fallback_start = Instant::now();
        let fallback_workers;
        (output_dir, fallback_workers) = test_mutants_after_baseline(
            fallback,
            workspace,
            output_dir,
            build_dir,
            timeouts,
            &known_tests,
            options,
            console,
        )?;
        workers = max(workers, fallback_workers);
        report.fallback_wall_seconds = fallback_start.elapsed().as_secs_f64();
        debug!(
            wall_seconds = report.fallback_wall_seconds,
            "schemata.fallback.done"
        );
    }
    report.trace_schema_build();
    report.record_fallback_time(&embedding, &output_dir.lab_outcome);
    report.wall_seconds = start_time.elapsed().as_secs_f64();
    report.write(&output_path);
    debug!(wall_seconds = report.wall_seconds, "schemata.done");
    console.lab_finished(&output_dir.lab_outcome, start_time, workers, options);
    output_dir.finish()
}

/// Test every mutant the classic way, when none are embedded in the schema, through
/// the lab, which runs its own baseline.
///
/// The build directory must already have been restored and dropped.
fn test_all_classically(
    embedding: &Embedding,
    workspace: &Workspace,
    output_mutex: Mutex<OutputDir>,
    options: &Options,
    console: &Console,
    report: &mut Report,
    start_time: Instant,
) -> Result<LabOutcome> {
    let output_dir = output_mutex.into_inner().expect("unlock output dir");
    let output_path = output_dir.path().to_owned();
    let fallback = embedding.classic_fallback().cloned().collect_vec();
    report.trace_schema_build();
    let outcome = crate::lab::test_mutants(fallback, workspace, output_dir, options, console);
    if let Ok(lab_outcome) = &outcome {
        report.record_fallback_time(embedding, lab_outcome);
    }
    report.wall_seconds = start_time.elapsed().as_secs_f64();
    report.write(&output_path);
    outcome
}

/// Collect coverage of the unmutated tree for `selections`, recording how it went in
/// `summary`.
///
/// It's collected in a copy of the source tree whose target directory is seeded from
/// the schema's build directory, unless `--seed-target=false`, so that the
/// dependencies are reused and the schema's build is left as it is.
///
/// If coverage can't be collected, this warns and returns none, so that every
/// mutant runs all the tests.
fn collect_in_copy(
    runner: &Runner,
    workspace: &Workspace,
    selections: &[PackageSelection],
    tools: &LlvmTools,
    summary: &mut SelectionSummary,
    unobserved_files: UnobservedFiles,
) -> Result<Vec<Collected>> {
    debug!(
        selections = selections.len(),
        "schemata.coverage.collect.start"
    );
    let copy_and_collect = || -> Result<(Vec<Collected>, CollectionReport)> {
        let start = Instant::now();
        let options = runner.options;
        let seed = runner
            .build_dir
            .target_dir_for_seeding()
            .filter(|_| options.seed_target);
        let build_dir = match seed {
            Some(seed) => BuildDir::copy_seeded(workspace.root(), &seed, options, runner.console)?,
            None => BuildDir::copy_from(workspace.root(), options, runner.console)?,
        };
        let copy_seconds = start.elapsed().as_secs_f64();
        let coverage_runner = Runner {
            build_dir: &build_dir,
            exclusive: RwLock::new(()),
            ..*runner
        };
        let (collected, mut collection) =
            collect(&coverage_runner, selections, tools, unobserved_files)?;
        collection.copy_seconds = copy_seconds;
        collection.total_seconds += copy_seconds;
        Ok((collected, collection))
    };
    match copy_and_collect() {
        Ok((collected, collection)) => {
            summary.collection = Some(collection);
            Ok(collected)
        }
        Err(err) => {
            crate::interrupt::check_interrupted()?;
            warn!("Could not collect coverage; running all tests for every mutant: {err:#}");
            summary.collection_error = Some(format!("{err:#}"));
            Ok(Vec::new())
        }
    }
}

/// The measured costs that decide whether to collect coverage for `mutants`
/// embedded mutants, once the baseline, which wrote `baseline_logs`, has passed and
/// the number of mutants to test at once is chosen: by probing, or with `--jobs`,
/// timed in `jobs_run`.
fn cost_inputs(
    report: &Report,
    mutants: usize,
    jobs_run: Option<&Probe>,
    selections: usize,
    baseline_logs: &[Utf8PathBuf],
) -> Result<CostInputs> {
    let suite_seconds = report
        .direct_baseline_test_seconds
        .unwrap_or(report.baseline_test_seconds);
    let concurrent = report
        .jobs_probe
        .iter()
        .chain(jobs_run)
        .find(|run| run.jobs == report.test_jobs && run.passed);
    let tests = baseline_logs.iter().try_fold(0, |sum, log| {
        let text = read_to_string(log).with_context(|| format!("read {log}"))?;
        anyhow::Ok(sum + crate::fail_fast::passed_tests(&text))
    })?;
    Ok(CostInputs {
        mutants,
        mutant_seconds: mutant_seconds(concurrent, suite_seconds, selections),
        rebuild_seconds: rebuild_seconds(&report.check_passes, &report.build_passes),
        tests,
        suite_seconds,
        workers: isolated_workers(),
    })
}

/// The coverage collected for the selection with cargo test command line `key`, if
/// its test binaries match `commands`, those replayed for the schema, in the tree
/// named by `roots`.
fn usable_coverage<'a>(
    collected: &'a [Collected],
    key: &[String],
    commands: &[ReplayCommand],
    roots: &[Utf8PathBuf],
) -> Option<&'a coverage::SelectionCoverage> {
    let found = collected.iter().find(|c| c.key == key)?;
    if same_test_binaries(&found.commands, &found.roots, commands, roots) {
        Some(&found.coverage)
    } else {
        warn!(
            ?key,
            "The instrumented build has different test binaries from the schema; running all tests"
        );
        None
    }
}

/// Write `uncovered.txt`, listing the mutants reported missed because no test
/// executes their code, without running tests.
fn write_uncovered_list(output_dir: &Utf8Path, mutant_tests: &[MutantTest]) -> Result<()> {
    let path = output_dir.join("uncovered.txt");
    let mut text = String::new();
    for test in mutant_tests.iter().filter(|t| {
        t.selection.as_ref().is_some_and(|s| s.plan == "uncovered")
            && t.summary == crate::outcome::SummaryOutcome::MissedMutant
    }) {
        text.push_str(&test.name);
        text.push('\n');
    }
    std::fs::write(&path, text).with_context(|| format!("write {path}"))
}

/// The logs of the schema build that was the baseline's build phase.
fn baseline_build_logs(report: &Report) -> &[Utf8PathBuf] {
    report
        .build_passes
        .last()
        .or(report.check_passes.last())
        .map_or(&[], |pass| &pass.logs)
}

/// Record the outcome of the baseline, made of `phase_results`, in `output_dir`.
///
/// Like the classic baseline's log, its log shows how it was built, with the commands
/// and results from `build_logs` (but not the compiler's messages, which are JSON),
/// and all that the tests printed, from `test_logs`.
fn record_baseline(
    output_dir: &mut OutputDir,
    phase_results: [PhaseResult; 2],
    build_logs: &[Utf8PathBuf],
    test_logs: &[Utf8PathBuf],
) -> Result<ScenarioOutcome> {
    let mut scenario_output = output_dir.start_scenario(&Scenario::Baseline)?;
    for build_log in build_logs {
        let text = read_to_string(build_log).with_context(|| format!("read {build_log}"))?;
        for line in text.lines().filter(|line| line.starts_with("*** ")) {
            writeln!(scenario_output.log_file, "{line}").context("write baseline log")?;
        }
    }
    for test_log in test_logs {
        let text = std::fs::read(test_log).with_context(|| format!("read {test_log}"))?;
        scenario_output
            .log_file
            .write_all(&text)
            .context("write baseline log")?;
    }
    let mut outcome = ScenarioOutcome::new(&scenario_output, Scenario::Baseline);
    for phase_result in phase_results {
        outcome.add_phase_result(phase_result);
    }
    output_dir.add_scenario_outcome(&outcome)?;
    Ok(outcome)
}

/// The distinct package selections that the tests of all the mutants use, those of
/// embedded mutants first.
///
/// The baseline builds and tests all of them, so that the tests of fallback mutants
/// are also known to pass in the unmutated tree, and the timeouts allow for them.
fn distinct_selections(
    embedding: &Embedding,
    tests_for_mutant: &TestsForMutant,
    options: &Options,
) -> Vec<PackageSelection> {
    embedding
        .embedded()
        .map(|(_, m)| m)
        .chain(embedding.fallback().map(|(m, _)| m))
        .map(|m| tests_for_mutant.selection(m))
        .unique_by(|selection| crate::cargo::cargo_argv(selection, Phase::Test, options))
        .collect()
}

/// What the schema does with each mutated package.
#[derive(Debug, Default)]
struct PackagePlan {
    /// Crate roots that get the helper module, with their text, by tree-relative path.
    roots: BTreeMap<Utf8PathBuf, String>,
    /// Packages none of whose mutants can be embedded, and why.
    fallbacks: HashMap<String, FallbackReason>,
    /// Tree-relative paths, with `/` separators, of files that string literals in
    /// their package name, so that tests might read them as text.
    read_files: BTreeSet<String>,
}

/// Find the crate roots of the mutated packages, the packages that can't have
/// embedded mutants, and the files that tests might read.
///
/// Every target except build scripts gets the helper module, because a mutated file
/// might be included in any of them. Proc-macro packages, and the packages they or
/// build scripts depend on, run at compile time, so can't have embedded mutants.
///
/// Files that tests might read, including crate roots, are embedded like any other,
/// since tests read the same schema text whichever mutant is active; the baseline
/// checks that they pass with it.
fn plan_packages(workspace: &Workspace, mutants: &[Mutant]) -> Result<PackagePlan> {
    let mutated: HashSet<&str> = mutants
        .iter()
        .map(|m| m.source_file.package.name.as_str())
        .collect();
    let mut plan = PackagePlan::default();
    let metadata = workspace.metadata();
    let workspace_root = &metadata.workspace_root;
    let compile_time = compile_time_packages(metadata);
    // The directory of each package that can have embedded mutants.
    let mut package_dirs = Vec::new();
    for package in metadata.workspace_packages() {
        if !mutated.contains(package.name.as_str()) {
            continue;
        }
        if package
            .targets
            .iter()
            .any(|t| t.kind.contains(&TargetKind::ProcMacro))
        {
            plan.fallbacks
                .insert(package.name.to_string(), FallbackReason::ProcMacroCrate);
            continue;
        }
        if compile_time.contains(package.name.as_str()) {
            plan.fallbacks.insert(
                package.name.to_string(),
                FallbackReason::CompileTimeDependency,
            );
            continue;
        }
        for target in &package.targets {
            if target.kind.contains(&TargetKind::CustomBuild) {
                continue;
            }
            let Ok(relative) = target.src_path.strip_prefix(workspace_root) else {
                continue;
            };
            let text = read_to_string(&target.src_path)
                .with_context(|| format!("read crate root {}", target.src_path))?;
            plan.roots.insert(relative.to_owned(), text);
        }
        package_dirs.push(
            package
                .manifest_path
                .parent()
                .expect("manifest has a parent directory"),
        );
    }
    // A test in one package might read the source of another. Crate roots without
    // mutants are listed too, since they get the helper module.
    let candidates: BTreeSet<String> = mutants
        .iter()
        .map(|m| m.source_file.tree_relative_path.to_slash_path())
        .chain(plan.roots.keys().map(|root| root.to_slash_path()))
        .collect();
    for package_dir in package_dirs {
        let relative_dir = package_dir
            .strip_prefix(workspace_root)
            .unwrap_or(package_dir)
            .to_slash_path();
        plan.read_files.extend(named_files(
            &rust_sources(package_dir, workspace_root)?,
            &relative_dir,
            &candidates,
        ));
    }
    Ok(plan)
}

/// Names of the workspace packages whose code can run at compile time, where the
/// schema's mutant id can't be seen: the dependencies of build scripts and of
/// proc-macros in the workspace, and their dependencies in turn.
///
/// This uses the dependencies declared by workspace packages, since the metadata is
/// read without the resolved graph; a workspace package can't be reached through a
/// package from outside the workspace.
fn compile_time_packages(metadata: &cargo_metadata::Metadata) -> HashSet<String> {
    let packages: HashMap<&str, &cargo_metadata::Package> = metadata
        .workspace_packages()
        .into_iter()
        .map(|package| (package.name.as_str(), package))
        .collect();
    let mut queue = Vec::new();
    for package in packages.values() {
        let proc_macro = package
            .targets
            .iter()
            .any(|t| t.kind.contains(&TargetKind::ProcMacro));
        queue.extend(
            package
                .dependencies
                .iter()
                .filter(|dep| {
                    dep.kind == DependencyKind::Build
                        || (proc_macro && dep.kind == DependencyKind::Normal)
                })
                .map(|dep| dep.name.as_str()),
        );
    }
    let mut found = HashSet::new();
    while let Some(name) = queue.pop() {
        let Some(package) = packages.get(name) else {
            continue; // Not in the workspace, so not mutated.
        };
        if found.insert(name.to_owned()) {
            queue.extend(
                package
                    .dependencies
                    .iter()
                    .filter(|dep| dep.kind != DependencyKind::Development)
                    .map(|dep| dep.name.as_str()),
            );
        }
    }
    found
}

/// Read the Rust source files under `dir`, skipping hidden and `target` directories,
/// returning their paths relative to `root`, with `/` separators, and their text.
fn rust_sources(dir: &Utf8Path, root: &Utf8Path) -> Result<Vec<(String, String)>> {
    let mut sources = Vec::new();
    let walk = ignore::WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(true)
        .filter_entry(|entry| entry.file_name() != "target")
        .build();
    for entry in walk {
        let entry = entry?;
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };
        if path.extension() != Some("rs") || !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // Files that aren't UTF-8 can't be Rust source.
        if let Ok(text) = read_to_string(path) {
            let relative = path.strip_prefix(root).unwrap_or(path);
            sources.push((relative.to_slash_path(), text));
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod test {
    use clap::CommandFactory;

    use super::*;
    use crate::Args;

    #[test]
    fn schemata_and_test_selection_options_are_shown_in_help() {
        let command = Args::command();
        for id in ["schemata", "no_schemata", "test_selection"] {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .expect("argument exists");
            assert!(!arg.is_hide_set(), "{id} is hidden");
        }
    }

    #[test]
    fn enabled_is_true_by_default() {
        assert!(enabled(&Options::from_arg_strs(["mutants"])).unwrap());
    }

    #[test]
    fn enabled_is_false_with_no_schemata() {
        assert!(!enabled(&Options::from_arg_strs(["mutants", "--no-schemata"])).unwrap());
        assert!(
            !enabled(&Options::from_arg_strs_and_config(
                ["mutants"],
                "schemata = false"
            ))
            .unwrap()
        );
    }

    /// Every option that schemata don't support, as given on the command line.
    const UNSUPPORTED_OPTIONS: [&str; 4] = [
        "--test-tool=nextest",
        "--in-place",
        "--check",
        "--baseline=skip",
    ];

    #[test]
    fn enabled_is_false_without_error_for_unsupported_options_by_default() {
        for option in UNSUPPORTED_OPTIONS {
            let options = Options::from_arg_strs(["mutants", option]);
            assert_eq!(unsupported_option(&options), Some(option));
            assert!(!enabled(&options).unwrap(), "{option}");
            let options = Options::from_arg_strs_and_config(["mutants", option], "schemata = true");
            assert!(
                !enabled(&options).unwrap(),
                "{option} with schemata in config"
            );
        }
    }

    #[test]
    fn enabled_is_an_error_for_schemata_on_command_line_with_unsupported_options() {
        for option in UNSUPPORTED_OPTIONS {
            let err = enabled(&Options::from_arg_strs(["mutants", "--schemata", option]))
                .unwrap_err()
                .to_string();
            assert_eq!(err, format!("--schemata can't be used with {option}"));
        }
    }

    #[test]
    fn enabled_is_an_error_for_test_selection_coverage_on_command_line_without_schemata() {
        let err = enabled(&Options::from_arg_strs([
            "mutants",
            "--no-schemata",
            "--test-selection=coverage",
        ]))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "--test-selection=coverage requires --schemata"
        );
        let err = enabled(&Options::from_arg_strs([
            "mutants",
            "--in-place",
            "--test-selection=coverage",
        ]))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "--test-selection=coverage requires --schemata, which can't be used with --in-place"
        );
        // A default or configured selection doesn't stop classic testing.
        for options in [
            Options::from_arg_strs(["mutants", "--no-schemata"]),
            Options::from_arg_strs_and_config(
                ["mutants", "--no-schemata"],
                r#"test_selection = "coverage""#,
            ),
        ] {
            assert!(!enabled(&options).unwrap());
        }
    }

    /// Look up environment variables in `vars`.
    fn env_vars<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    const DEFAULT_ENV_SETTINGS: EnvSettings = EnvSettings {
        check_phase: Phase::Build,
        direct: true,
        dropped: Dropped::Unviable,
        stop_after_build: false,
        confirm: Confirm::Reached,
        unobserved_files: UnobservedFiles::FullSuite,
    };

    #[test]
    fn env_settings_parse_defaults_to_build_phase_direct_replay_unviable_dropped_and_no_stop() {
        assert_eq!(EnvSettings::parse(env_vars(&[])), DEFAULT_ENV_SETTINGS);
        assert_eq!(
            EnvSettings::parse(env_vars(&[
                (CHECK_PHASE_ENV, "build"),
                (EXEC_ENV, "direct"),
                (DROPPED_ENV, "unviable"),
            ])),
            DEFAULT_ENV_SETTINGS
        );
        assert_eq!(
            EnvSettings::parse(env_vars(&[
                (CHECK_PHASE_ENV, "bogus"),
                (EXEC_ENV, "bogus"),
                (DROPPED_ENV, "bogus"),
                (STOP_AFTER_ENV, "bogus"),
                (coverage::CONFIRM_ENV, "bogus"),
                (coverage::UNOBSERVED_FILES_ENV, "bogus"),
            ])),
            DEFAULT_ENV_SETTINGS
        );
    }

    #[test]
    fn env_settings_parse_accepts_every_non_default_value() {
        assert_eq!(
            EnvSettings::parse(env_vars(&[
                (CHECK_PHASE_ENV, "check"),
                (EXEC_ENV, "cargo"),
                (DROPPED_ENV, "classic"),
                (STOP_AFTER_ENV, "build"),
                (coverage::CONFIRM_ENV, "passed"),
                (coverage::UNOBSERVED_FILES_ENV, "uncovered"),
            ])),
            EnvSettings {
                check_phase: Phase::Check,
                direct: false,
                dropped: Dropped::Classic,
                stop_after_build: true,
                confirm: Confirm::Passed,
                unobserved_files: UnobservedFiles::Uncovered,
            }
        );
    }
}
