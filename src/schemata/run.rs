// Copyright 2026 Martin Pool

//! Run cargo against the schema build directory: check/build passes, the schema
//! baseline, and per-mutant tests in parallel.

#![warn(clippy::pedantic)]

use std::fs::read_to_string;
use std::panic::resume_unwind;
use std::sync::{Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde::Serialize;
use tracing::{debug, warn};

use super::coverage::{Confirm, FullSuiteReason, Plan, SelectionCoverage, TestCase, batches};
use super::diagnostics::compile_errors;
use super::embed::{Blame, Embedding};
use super::generate::{ID_ENV_VAR, MutantId};
use super::jobs::{Probe, next_probe};
use super::markers::Markers;
use super::plan::FallbackReason;
use super::replay::{Quoting, ReplayCommand, test_commands};
use crate::build_dir::BuildDir;
use crate::cargo::{build_dir_cargo_env, cargo_argv};
use crate::console::Console;
use crate::fail_fast::{KnownTests, KnownTestsBySelection};
use crate::interrupt::check_interrupted;
use crate::options::Options;
use crate::outcome::{Phase, PhaseResult, ScenarioOutcome, SummaryOutcome};
use crate::output::{OutputDir, ScenarioOutput};
use crate::package::PackageSelection;
use crate::process::{Env, Exit, Process, TERMINATES_DESCENDANTS};
use crate::scenario::Scenario;
use crate::timeouts::Timeouts;
use crate::{Mutant, Result};

/// Message cargo prints when it waits for another cargo process's lock.
const LOCK_WAIT_MESSAGE: &str = "Blocking waiting for file lock";

/// One check or build pass over the schema.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Pass {
    pub iteration: usize,
    pub phase: Phase,
    pub seconds: f64,
    pub success: bool,
    pub errors: usize,
    pub unattributed_errors: usize,
    pub dropped: usize,
    pub embedded_after: usize,
    /// The logs of the cargo commands the pass ran.
    #[serde(skip)]
    pub logs: Vec<Utf8PathBuf>,
}

/// The result of testing one embedded mutant.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MutantTest {
    pub id: MutantId,
    pub name: String,
    pub summary: SummaryOutcome,
    pub seconds: f64,
    /// Names of the locks cargo reported waiting for, held by other cargo processes.
    pub lock_waits: Vec<String>,
    /// Cargo compiled something, which should not happen after the schema build.
    pub rebuilt: bool,
    /// The mutant's code ran in a process where it was active.
    pub reached: bool,
    /// The tests failed although the mutant's code never ran, so they were run again
    /// with no other tests running, and this is the result of that.
    pub retested: bool,
    /// How tests were selected, with `--test-selection=coverage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<MutantSelection>,
}

/// How one mutant's tests were selected by coverage, and what happened.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct MutantSelection {
    /// `selected`, `uncovered`, or `full_suite`.
    pub plan: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_suite_reason: Option<FullSuiteReason>,
    /// Tests that execute the mutated code and pass alone.
    pub selected_tests: usize,
    /// Tests in the selection's test binaries, not counting doctests.
    pub total_tests: usize,
    /// Selected tests actually run, stopping at the first failing batch.
    pub tests_run: usize,
    pub batches_run: usize,
    pub selected_seconds: f64,
    /// Whether the selected tests all passed, if any ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_passed: Option<bool>,
    /// For an uncovered mutant, whether the schema recorded its code running in the
    /// baseline, with no mutant active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran_in_baseline: Option<bool>,
    /// The full suite ran to confirm the mutant is missed.
    pub confirmed: bool,
    pub confirm_seconds: f64,
    /// The confirmation's verdict differs from the selected tests' (or from
    /// "uncovered"): the selection missed a test that fails.
    pub verdict_changed: bool,
}

/// Coverage-based test selection, with `--test-selection=coverage`.
pub(crate) struct Selector<'a> {
    /// For each package selection, its coverage if it could be used.
    pub coverage: Vec<Option<&'a SelectionCoverage>>,
    pub confirm: Confirm,
}

/// The result of testing a mutant, and perhaps testing it again alone.
struct Attempts {
    /// Test phases of the last run, the last of which gives the outcome.
    phases: Vec<PhaseResult>,
    selection_record: Option<MutantSelection>,
    /// The mutant's code ran in the last run.
    reached: bool,
    /// The mutant was tested again because its code didn't run in the first run.
    retested: bool,
}

/// What ran when running selected tests.
struct SelectedRun {
    status: Exit,
    tests_run: usize,
    batches_run: usize,
    /// The test binaries run.
    programs: Vec<String>,
}

/// Selected tests to run for one mutant, and how.
struct SelectedTests<'a> {
    /// The test commands of the mutant's package selection.
    commands: &'a [ReplayCommand],
    coverage: &'a SelectionCoverage,
    /// The tests to run, as indexes into [`SelectionCoverage::tests`], fastest first.
    selected: &'a [usize],
    id: MutantId,
    timeout: Option<Duration>,
    stop_on_failure: Option<&'a KnownTests>,
}

/// An embedded mutant to test.
#[derive(Debug)]
pub(crate) struct Work {
    pub id: MutantId,
    pub mutant: Mutant,
    /// Index of the package selection whose tests are run.
    pub selection: usize,
    /// If set, a missed outcome with the schema can't be trusted, for this reason,
    /// and the mutant should be tested the classic way instead.
    pub classic_if_missed: Option<FallbackReason>,
    /// With coverage-based selection, which tests to run.
    pub plan: Option<Plan>,
    /// The schema recorded the mutant's site running in the baseline, with no mutant
    /// active.
    pub ran_in_baseline: bool,
}

/// What happened to one embedded mutant.
#[derive(Debug)]
pub(crate) enum Tested {
    /// The outcome was recorded.
    Recorded(MutantTest),
    /// The mutant was missed, but that can't be trusted for the given reason, so it
    /// should be tested the classic way.
    NeedsClassic(MutantId, FallbackReason),
}

/// The result of the schema baseline.
#[derive(Debug)]
pub(crate) enum Baseline {
    /// Tests pass with the schema and no mutant active.
    Passed {
        result: PhaseResult,
        /// The test commands for each selection, if they were captured.
        commands: Option<Vec<Vec<ReplayCommand>>>,
        /// For each selection, the tests whose failure stops a mutant's tests, from
        /// that selection's baseline.
        known_tests: KnownTestsBySelection,
        /// The logs of the tests of each selection.
        logs: Vec<Utf8PathBuf>,
    },
    /// Tests fail with the schema and no mutant active, but pass on the unmutated
    /// tree, which has been restored: the schema changes behavior, perhaps because
    /// tests read the source files.
    SchemaChangesBehavior(Utf8PathBuf),
    /// Tests fail in the unmutated tree too, which has been restored; with the result
    /// of testing it and its log.
    Failed(PhaseResult, Utf8PathBuf),
}

/// How each mutant's tests are run.
#[derive(Debug, Clone)]
pub(crate) enum TestExec {
    /// Run `cargo test`, exactly as the classic path does.
    CargoTest,
    /// Replay the test commands captured from the baseline `cargo test`, for each
    /// package selection, without running cargo.
    Direct(Vec<Vec<ReplayCommand>>),
}

impl TestExec {
    fn commands(&self, selection: usize) -> Option<&[ReplayCommand]> {
        match self {
            TestExec::CargoTest => None,
            TestExec::Direct(commands) => Some(&commands[selection]),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            TestExec::CargoTest => "cargo_test",
            TestExec::Direct(_) => "direct",
        }
    }
}

/// Runs cargo in the one build directory shared by all schema steps.
pub(crate) struct Runner<'a> {
    pub build_dir: &'a BuildDir,
    pub jobserver: Option<&'a jobserver::Client>,
    pub output: &'a Mutex<OutputDir>,
    pub options: &'a Options,
    pub console: &'a Console,
    pub markers: &'a Markers,
    /// Held shared while testing a mutant, and exclusively to test one with nothing
    /// else running.
    pub exclusive: RwLock<()>,
}

impl Runner<'_> {
    /// The environment variables to set for cargo, and for the test commands it runs,
    /// in the build directory: every schema step gets them from here.
    ///
    /// Like the classic path, this builds into the build directory's own `target/`,
    /// whatever `CARGO_TARGET_DIR` or `build.target-dir` say.
    pub(crate) fn cargo_env(&self) -> Vec<(String, String)> {
        build_dir_cargo_env(self.build_dir, self.options)
    }

    /// Overwrite files in the build directory.
    pub(crate) fn write_files(&self, files: &[(Utf8PathBuf, String)]) -> Result<()> {
        for (path, text) in files {
            self.build_dir.overwrite_file(path, text)?;
        }
        Ok(())
    }

    /// Run one cargo command, logging to a log named `log_name`.
    ///
    /// Returns the phase result and the full log text.
    pub(crate) fn run_step(
        &self,
        phase: Phase,
        argv: Vec<String>,
        env: &[(String, String)],
        timeout: Option<Duration>,
        log_name: &str,
    ) -> Result<(PhaseResult, String, Utf8PathBuf)> {
        let mut log = self
            .output
            .lock()
            .expect("lock output dir")
            .start_log(log_name)?;
        let start = Instant::now();
        let env = Env {
            set: env.to_vec(),
            remove: Vec::new(),
        };
        let process_status = Process::run(
            &argv,
            &env,
            self.build_dir.path(),
            timeout,
            self.jobserver,
            &mut log,
            self.console,
            None,
        )?;
        check_interrupted()?;
        let log_path = log.output_dir.join(log.log_path());
        let text = read_to_string(&log_path)?;
        Ok((
            PhaseResult {
                phase,
                duration: start.elapsed(),
                process_status,
                argv,
            },
            text,
            log_path,
        ))
    }

    /// Render the schema, run `phase` for each selection, and drop mutants blamed for
    /// compile errors, until it compiles or `max_iterations` is reached.
    ///
    /// Returns the passes run. If compilation can't be made clean, every remaining
    /// mutant falls back.
    pub(crate) fn drop_until_clean(
        &self,
        embedding: &mut Embedding,
        phase: Phase,
        selections: &[PackageSelection],
        max_iterations: usize,
    ) -> Result<Vec<Pass>> {
        let mut passes = Vec::new();
        for iteration in 1..=max_iterations {
            if embedding.embedded().next().is_none() {
                return Ok(passes);
            }
            self.write_files(&embedding.render())?;
            let start = Instant::now();
            let mut failure = None;
            let mut logs = Vec::new();
            for selection in selections {
                let mut argv = cargo_argv(selection, phase, self.options);
                argv.push("--message-format=json".to_owned());
                let (result, log, log_path) = self.run_step(
                    phase,
                    argv,
                    &self.cargo_env(),
                    self.options.build_timeout,
                    &format!("schemata-{phase}-{iteration}"),
                )?;
                logs.push(log_path.clone());
                if !result.is_success() {
                    failure = Some((result, log, log_path));
                    break;
                }
            }
            let Some((result, log, log_path)) = failure else {
                let pass = Pass {
                    iteration,
                    phase,
                    seconds: start.elapsed().as_secs_f64(),
                    success: true,
                    errors: 0,
                    unattributed_errors: 0,
                    dropped: 0,
                    embedded_after: embedding.embedded().count(),
                    logs,
                };
                debug!(
                    ?phase,
                    iteration,
                    seconds = pass.seconds,
                    embedded = pass.embedded_after,
                    "schemata.compile.clean"
                );
                passes.push(pass);
                return Ok(passes);
            };
            let errors = compile_errors(&log);
            if errors.is_empty() {
                anyhow::bail!(
                    "cargo {phase} of the schema failed ({status:?}) without reporting compile errors; see {log_path}",
                    status = result.process_status
                );
            }
            let summary = embedding.drop_for_errors(&errors, self.build_dir.path());
            let pass = Pass {
                iteration,
                phase,
                seconds: start.elapsed().as_secs_f64(),
                success: false,
                errors: errors.len(),
                unattributed_errors: summary.unattributed.len(),
                dropped: summary.dropped,
                embedded_after: embedding.embedded().count(),
                logs,
            };
            debug!(
                ?phase,
                iteration,
                seconds = pass.seconds,
                errors = pass.errors,
                unattributed = pass.unattributed_errors,
                dropped = pass.dropped,
                embedded = pass.embedded_after,
                "schemata.compile.dropped"
            );
            passes.push(pass);
            if summary.dropped == 0 {
                warn!(
                    errors = ?summary.unattributed,
                    "schema compile errors could not be attributed; all mutants fall back"
                );
                embedding.fall_back_all(FallbackReason::UnattributedCompileError);
                return Ok(passes);
            }
        }
        warn!(
            max_iterations,
            "schema still fails to compile; all remaining mutants fall back"
        );
        embedding.fall_back_all(FallbackReason::CheckIterationsExhausted);
        Ok(passes)
    }

    /// Run `cargo test` with no mutant active, for each selection.
    ///
    /// If the tests fail, this restores `originals` and tests the unmutated tree, to
    /// tell whether the schema or the tree is at fault.
    ///
    /// If `capture_commands`, cargo is run with `-vv`, and this also returns the test
    /// commands it ran for each selection, if they could all be parsed.
    ///
    /// The names of the tests that passed for each selection are learned from its
    /// output, to stop a mutant's tests when one of them fails.
    pub(crate) fn baseline(
        &self,
        selections: &[PackageSelection],
        originals: &[(Utf8PathBuf, String)],
        capture_commands: bool,
    ) -> Result<Baseline> {
        let mut total = Duration::ZERO;
        let mut argv = Vec::new();
        let mut captured = Some(Vec::new());
        let mut known_tests = KnownTestsBySelection::default();
        let mut logs = Vec::new();
        for (i, selection) in selections.iter().enumerate() {
            let selection_argv = cargo_argv(selection, Phase::Test, self.options);
            let selection_argv = if capture_commands {
                capture_argv(selection_argv)
            } else {
                selection_argv
            };
            let (result, log, log_path) = self.run_step(
                Phase::Test,
                selection_argv,
                &self.test_env(0),
                self.options.test_timeout,
                "schemata-baseline",
            )?;
            total += result.duration;
            if i == 0 {
                argv.clone_from(&result.argv);
            }
            if !result.is_success() {
                self.write_files(originals)?;
                let (original, _, original_log) = self.run_step(
                    Phase::Test,
                    cargo_argv(selection, Phase::Test, self.options),
                    &self.cargo_env(),
                    self.options.test_timeout,
                    "schemata-baseline-original",
                )?;
                if original.is_success() {
                    return Ok(Baseline::SchemaChangesBehavior(log_path));
                }
                return Ok(Baseline::Failed(original, original_log));
            }
            known_tests.add_baseline(
                self.options,
                cargo_argv(selection, Phase::Test, self.options),
                &log,
            );
            logs.push(log_path.clone());
            let commands = if capture_commands {
                test_commands(&log, self.build_dir.path(), Quoting::host())
                    .inspect_err(|err| {
                        warn!(%log_path, "could not capture test commands from cargo output: {err:#}");
                    })
                    .ok()
            } else {
                None
            };
            captured = captured.zip(commands).map(|(mut all, commands)| {
                all.push(commands);
                all
            });
        }
        let phase_result = PhaseResult {
            phase: Phase::Test,
            duration: total,
            process_status: Exit::Success,
            argv,
        };
        Ok(Baseline::Passed {
            result: phase_result,
            commands: captured.filter(|_| capture_commands),
            known_tests,
            logs,
        })
    }

    /// Replay captured test commands with no mutant active.
    ///
    /// Returns the total duration if they all pass, or `None` if any fail, in which
    /// case direct execution can't be trusted to match `cargo test`.
    pub(crate) fn replay_baseline(
        &self,
        commands: &[Vec<ReplayCommand>],
    ) -> Result<Option<Duration>> {
        let mut log = self
            .output
            .lock()
            .expect("lock output dir")
            .start_log("schemata-baseline-direct")?;
        let start = Instant::now();
        for selection_commands in commands {
            let status = match self.replay(
                selection_commands,
                0,
                self.options.test_timeout,
                &mut log,
                None,
            ) {
                Ok(status) => status,
                Err(err) => {
                    // A command that can't be started, perhaps because it was misparsed,
                    // means cargo test must be used instead; but an interruption stops.
                    check_interrupted()?;
                    warn!("could not run replayed test commands: {err:#}");
                    return Ok(None);
                }
            };
            if !status.is_success() {
                warn!(
                    log = %log.output_dir.join(log.log_path()),
                    ?status,
                    "replaying test commands fails with no mutant active"
                );
                return Ok(None);
            }
        }
        Ok(Some(start.elapsed()))
    }

    /// Run the tests with no mutant active in `jobs` concurrent workers, as mutants
    /// will be tested, to find out whether tests interfere with each other when they
    /// share the build directory, for example by writing the same files.
    ///
    /// Returns true if they all pass.
    pub(crate) fn concurrent_baseline(
        &self,
        selections: &[PackageSelection],
        exec: &TestExec,
        jobs: usize,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        let passed = thread::scope(|scope| -> Result<bool> {
            let workers = (0..jobs)
                .map(|_| {
                    scope.spawn(move || -> Result<bool> {
                        let mut log = self
                            .output
                            .lock()
                            .expect("lock output dir")
                            .start_log("schemata-baseline-concurrent")?;
                        for (i, selection) in selections.iter().enumerate() {
                            let (_, status) = self.run_tests(
                                0,
                                selection,
                                exec.commands(i),
                                timeout,
                                &mut log,
                                None,
                            )?;
                            if !status.is_success() {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    })
                })
                .collect_vec();
            let mut passed = true;
            for worker in workers {
                match worker.join() {
                    Err(panic) => resume_unwind(panic),
                    Ok(result) => passed &= result?,
                }
            }
            Ok(passed)
        })?;
        Ok(passed)
    }

    /// Choose how many mutants to test at once by timing 1, 2, 4, ... concurrent
    /// runs of the tests with no mutant active, with [`Self::concurrent_baseline`],
    /// up to `max_jobs` (see [`super::jobs`]).
    ///
    /// Returns the probes run; [`super::jobs::chosen`] gives the number of jobs.
    pub(crate) fn probe_jobs(
        &self,
        selections: &[PackageSelection],
        exec: &TestExec,
        max_jobs: usize,
        timeout: Option<Duration>,
    ) -> Result<Vec<Probe>> {
        let mut probes = Vec::new();
        while let Some(jobs) = next_probe(&probes, max_jobs) {
            let start = Instant::now();
            let passed = self.concurrent_baseline(selections, exec, jobs, timeout)?;
            let probe = Probe {
                jobs,
                seconds: start.elapsed().as_secs_f64(),
                passed,
            };
            debug!(jobs, seconds = probe.seconds, passed, "schemata.jobs.probe");
            probes.push(probe);
        }
        Ok(probes)
    }

    /// True if `cargo test` stops at the first test command that fails, as it does
    /// unless `--no-fail-fast` is given.
    fn cargo_fail_fast(&self) -> bool {
        !self
            .options
            .additional_cargo_test_args
            .iter()
            .any(|arg| arg == "--no-fail-fast")
    }

    /// Run test commands in order with the given mutant id, stopping at the first
    /// failure unless `--no-fail-fast` was given to cargo test, within an overall
    /// timeout.
    ///
    /// Commands that ran no tests in the baseline ([`ReplayCommand::idle`]) are
    /// skipped. Each command is killed as soon as any of `stop_on_failure` fail.
    fn replay(
        &self,
        commands: &[ReplayCommand],
        id: MutantId,
        timeout: Option<Duration>,
        log: &mut ScenarioOutput,
        stop_on_failure: Option<&KnownTests>,
    ) -> Result<Exit> {
        let fail_fast = self.cargo_fail_fast();
        let start = Instant::now();
        let mut overall = Exit::Success;
        for command in replayed(commands) {
            let remaining = timeout.map(|t| t.saturating_sub(start.elapsed()));
            if remaining == Some(Duration::ZERO) {
                return Ok(Exit::Timeout);
            }
            match self.run_test_command(
                command,
                &command.argv,
                id,
                remaining,
                log,
                stop_on_failure,
            )? {
                Exit::Success => continue,
                Exit::Timeout => return Ok(Exit::Timeout),
                failure => overall = failure,
            }
            if fail_fast {
                break;
            }
        }
        Ok(overall)
    }

    /// Run `argv` with the environment and directory of a test command and the given
    /// mutant id.
    ///
    /// Returns success, failure, or timeout: a test binary killed by a signal is a
    /// failure, as `cargo test` reports it; and so is one that was stopped because
    /// one of `stop_on_failure` failed.
    fn run_test_command(
        &self,
        command: &ReplayCommand,
        argv: &[String],
        id: MutantId,
        timeout: Option<Duration>,
        log: &mut ScenarioOutput,
        stop_on_failure: Option<&KnownTests>,
    ) -> Result<Exit> {
        let mut env = self.cargo_env();
        env.extend(command.env.iter().cloned());
        env.push((ID_ENV_VAR.to_owned(), id.to_string()));
        let env = Env {
            set: env,
            remove: Vec::new(),
        };
        let status = Process::run(
            argv,
            &env,
            &command.cwd,
            timeout,
            None,
            log,
            self.console,
            stop_on_failure,
        )?;
        check_interrupted()?;
        Ok(match status {
            Exit::Success | Exit::Timeout | Exit::Failure(_) => status,
            #[cfg(unix)]
            other @ Exit::Signalled(_) => {
                log.message(&format!("test command ended with {other:?}: a failure"))?;
                Exit::Failure(101)
            }
            other @ Exit::Other => {
                log.message(&format!("test command ended with {other:?}: a failure"))?;
                Exit::Failure(101)
            }
        })
    }

    /// Run selected tests, in batches of increasing size, each batch with one
    /// process per test binary, stopping at the first failure, within an overall
    /// timeout.
    ///
    /// If a batch times out, the selected tests that all the tests, run the usual way,
    /// could see fail before that timeout also run, within one more timeout; see
    /// [`Self::run_after_timeout`].
    fn run_selected(&self, tests: &SelectedTests, log: &mut ScenarioOutput) -> Result<SelectedRun> {
        let start = Instant::now();
        let mut run = SelectedRun {
            status: Exit::Success,
            tests_run: 0,
            batches_run: 0,
            programs: Vec::new(),
        };
        for batch in batches(tests.selected.len()) {
            run.batches_run += 1;
            let rest = &tests.selected[batch.end..];
            for (command, cases) in cases_by_command(tests.coverage, &tests.selected[batch]) {
                let remaining = tests.timeout.map(|t| t.saturating_sub(start.elapsed()));
                if remaining == Some(Duration::ZERO) {
                    run.status = Exit::Timeout;
                    return Ok(run);
                }
                run.status = self.run_cases(tests, command, &cases, remaining, log, &mut run)?;
                if run.status == Exit::Timeout {
                    run.status = self.run_after_timeout(tests, rest, command, log, &mut run)?;
                }
                if !run.status.is_success() {
                    return Ok(run);
                }
            }
        }
        Ok(run)
    }

    /// Run test cases of the test command with index `command`, within `timeout`,
    /// recording that they ran in `run`, and return how the command ended.
    fn run_cases(
        &self,
        tests: &SelectedTests,
        command: usize,
        cases: &[&TestCase],
        timeout: Option<Duration>,
        log: &mut ScenarioOutput,
        run: &mut SelectedRun,
    ) -> Result<Exit> {
        let command = &tests.commands[command];
        let mut argv = command.argv.clone();
        // A case without a name is a whole binary, so it runs without filters.
        let names: Option<Vec<String>> = cases.iter().map(|case| case.name.clone()).collect();
        if let Some(names) = names {
            argv.push("--exact".to_owned());
            argv.extend(names);
        }
        run.tests_run += cases.len();
        if !run.programs.contains(&command.argv[0]) {
            run.programs.push(command.argv[0].clone());
        }
        self.run_test_command(
            command,
            &argv,
            tests.id,
            timeout,
            log,
            tests.stop_on_failure,
        )
    }

    /// After selected tests of the test command with index `timed_out` time out, run
    /// those of the selected tests not run yet, `rest`, that could fail before the
    /// timeout when all the tests run the usual way, within one more timeout. Return
    /// the outcome: a failure if one of them fails, or else still a timeout.
    ///
    /// Those are the tests of earlier commands, since cargo runs the commands in
    /// order and stops at the first failure (unless `--no-fail-fast` is given); and
    /// those of the same command if a failure stops the tests, since they run at
    /// the same time as the test that hangs. Tests of later commands would never run
    /// the usual way, so they don't run here either.
    fn run_after_timeout(
        &self,
        tests: &SelectedTests,
        rest: &[usize],
        timed_out: usize,
        log: &mut ScenarioOutput,
        run: &mut SelectedRun,
    ) -> Result<Exit> {
        let fail_fast = self.cargo_fail_fast();
        let candidates = rest
            .iter()
            .copied()
            .filter(|index| {
                let command = tests.coverage.tests()[*index].case.command;
                (command < timed_out && fail_fast)
                    || (command == timed_out && tests.stop_on_failure.is_some())
            })
            .collect_vec();
        if candidates.is_empty() {
            return Ok(Exit::Timeout);
        }
        log.message(&format!(
            "selected tests timed out: running {} more selected tests that could fail first when all the tests run",
            candidates.len()
        ))?;
        debug!(
            id = tests.id,
            tests = candidates.len(),
            "schemata.coverage.after_timeout"
        );
        let start = Instant::now();
        for (command, cases) in cases_by_command(tests.coverage, &candidates) {
            let remaining = tests.timeout.map(|t| t.saturating_sub(start.elapsed()));
            if remaining == Some(Duration::ZERO) {
                break;
            }
            match self.run_cases(tests, command, &cases, remaining, log, run)? {
                Exit::Success => (),
                Exit::Timeout => break,
                failure => return Ok(failure),
            }
        }
        Ok(Exit::Timeout)
    }

    /// Test a mutant according to its coverage plan, confirming a pass with all the
    /// tests if `confirm` says so.
    ///
    /// Returns the test phases run, the last of which gives the outcome, and a record
    /// of the selection.
    #[allow(clippy::too_many_arguments)]
    fn test_with_plan(
        &self,
        id: MutantId,
        selection: &PackageSelection,
        commands: Option<&[ReplayCommand]>,
        coverage: Option<&SelectionCoverage>,
        plan: &Plan,
        ran_in_baseline: bool,
        confirm: Confirm,
        timeout: Option<Duration>,
        log: &mut ScenarioOutput,
        stop_on_failure: Option<&KnownTests>,
    ) -> Result<(Vec<PhaseResult>, MutantSelection)> {
        let mut record = MutantSelection {
            total_tests: coverage.map_or(0, |c| c.tests().len()),
            ..MutantSelection::default()
        };
        let mut phases = Vec::new();
        let first = match plan {
            Plan::FullSuite(reason) => {
                record.plan = "full_suite";
                record.full_suite_reason = Some(*reason);
                log.message(&format!("running all tests: {reason:?}"))?;
                let start = Instant::now();
                let (argv, status) =
                    self.run_tests(id, selection, commands, timeout, log, stop_on_failure)?;
                phases.push(test_phase(start.elapsed(), status, argv));
                return Ok((phases, record));
            }
            Plan::Uncovered => {
                record.plan = "uncovered";
                record.ran_in_baseline = Some(ran_in_baseline);
                phases.push(test_phase(Duration::ZERO, Exit::Success, Vec::new()));
                let ran = if ran_in_baseline {
                    "but it ran in the baseline"
                } else {
                    "nor did it run in the baseline"
                };
                if !confirm.confirms_uncovered(ran_in_baseline) {
                    log.message(&format!(
                        "no test executes the mutated code, {ran}: reported missed without running tests"
                    ))?;
                    return Ok((phases, record));
                }
                log.message(&format!(
                    "no test's coverage shows the mutated code running, {ran}"
                ))?;
                Exit::Success
            }
            Plan::Selected(selected) => {
                record.plan = "selected";
                record.selected_tests = selected.len();
                log.message(&format!(
                    "running {} of {} tests that execute the mutated code",
                    selected.len(),
                    record.total_tests
                ))?;
                let coverage = coverage.expect("tests are selected from coverage");
                let commands = commands.expect("tests are selected from replayed commands");
                let start = Instant::now();
                let tests = SelectedTests {
                    commands,
                    coverage,
                    selected,
                    id,
                    timeout,
                    stop_on_failure,
                };
                let run = self.run_selected(&tests, log)?;
                record.selected_seconds = start.elapsed().as_secs_f64();
                record.tests_run = run.tests_run;
                record.batches_run = run.batches_run;
                record.selected_passed = Some(run.status.is_success());
                phases.push(test_phase(start.elapsed(), run.status, run.programs));
                if confirm == Confirm::None {
                    return Ok((phases, record));
                }
                run.status
            }
        };
        if !first.is_success() {
            return Ok((phases, record));
        }
        log.message("confirming with all tests")?;
        let start = Instant::now();
        let (argv, status) =
            self.run_tests(id, selection, commands, timeout, log, stop_on_failure)?;
        record.confirmed = true;
        record.confirm_seconds = start.elapsed().as_secs_f64();
        record.verdict_changed = !status.is_success();
        if record.verdict_changed {
            debug!(
                id,
                ?status,
                plan = record.plan,
                "schemata.coverage.verdict_changed"
            );
        }
        phases.push(test_phase(start.elapsed(), status, argv));
        Ok((phases, record))
    }

    /// Test a mutant: according to its coverage plan if there is one, or else by
    /// running all its tests.
    ///
    /// Returns the test phases run, the last of which gives the outcome, and a record
    /// of the selection, if any.
    #[allow(clippy::too_many_arguments)]
    fn test_mutant(
        &self,
        id: MutantId,
        selection: &PackageSelection,
        commands: Option<&[ReplayCommand]>,
        selector: Option<(Option<&SelectionCoverage>, Confirm)>,
        plan: Option<&Plan>,
        ran_in_baseline: bool,
        timeout: Option<Duration>,
        log: &mut ScenarioOutput,
        stop_on_failure: Option<&KnownTests>,
    ) -> Result<(Vec<PhaseResult>, Option<MutantSelection>)> {
        if let (Some(plan), Some((coverage, confirm))) = (plan, selector) {
            let (phases, record) = self.test_with_plan(
                id,
                selection,
                commands,
                coverage,
                plan,
                ran_in_baseline,
                confirm,
                timeout,
                log,
                stop_on_failure,
            )?;
            Ok((phases, Some(record)))
        } else {
            let start = Instant::now();
            let (argv, status) =
                self.run_tests(id, selection, commands, timeout, log, stop_on_failure)?;
            Ok((vec![test_phase(start.elapsed(), status, argv)], None))
        }
    }

    /// Run the tests of `selection` with mutant `id` active: by replaying `commands`
    /// if given, or else with `cargo test`.
    ///
    /// Returns what was run, for the record, and how it ended. The tests are stopped
    /// as soon as any of `stop_on_failure` fail.
    fn run_tests(
        &self,
        id: MutantId,
        selection: &PackageSelection,
        commands: Option<&[ReplayCommand]>,
        timeout: Option<Duration>,
        log: &mut ScenarioOutput,
        stop_on_failure: Option<&KnownTests>,
    ) -> Result<(Vec<String>, Exit)> {
        if let Some(commands) = commands {
            let status = self.replay(commands, id, timeout, log, stop_on_failure)?;
            // Record the programs that were replayed.
            let programs = replayed(commands).map(|c| c.argv[0].clone()).collect();
            Ok((programs, status))
        } else {
            let argv = cargo_argv(selection, Phase::Test, self.options);
            let status = Process::run(
                &argv,
                &Env {
                    set: self.test_env(id),
                    remove: Vec::new(),
                },
                self.build_dir.path(),
                timeout,
                self.jobserver,
                log,
                self.console,
                // Killing cargo on Windows would leave the test binary running.
                stop_on_failure.filter(|_| TERMINATES_DESCENDANTS),
            )?;
            check_interrupted()?;
            Ok((argv, status))
        }
    }

    fn test_env(&self, id: MutantId) -> Vec<(String, String)> {
        let mut env = self.cargo_env();
        env.push((ID_ENV_VAR.to_owned(), id.to_string()));
        env
    }

    /// Test each embedded mutant by running its tests with the mutant's id, using up
    /// to `jobs` concurrent workers that all share the one build directory.
    ///
    /// Each work item gives the index in `selections` of the packages to test.
    ///
    /// Items with [`Work::classic_if_missed`] set that are missed are not recorded,
    /// but returned as [`Tested::NeedsClassic`] to be tested the classic way.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn test_embedded(
        &self,
        work: Vec<Work>,
        selections: &[PackageSelection],
        exec: &TestExec,
        selector: Option<&Selector>,
        timeouts: Timeouts,
        jobs: usize,
        known_tests: &KnownTestsBySelection,
    ) -> Result<Vec<Tested>> {
        let n_workers = jobs.clamp(1, work.len().max(1));
        let queue = &Mutex::new(work.into_iter());
        let results = &Mutex::new(Vec::new());
        thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..n_workers)
                .map(|worker| {
                    scope.spawn(move || -> Result<()> {
                        loop {
                            let Some(item) = queue.lock().expect("lock work queue").next() else {
                                return Ok(());
                            };
                            let selection = item.selection;
                            let result = self.test_one(
                                worker,
                                item,
                                &selections[selection],
                                exec.commands(selection),
                                selector.map(|s| (s.coverage[selection], s.confirm)),
                                timeouts,
                                known_tests.get(&cargo_argv(
                                    &selections[selection],
                                    Phase::Test,
                                    self.options,
                                )),
                            )?;
                            results.lock().expect("lock results").push(result);
                        }
                    })
                })
                .collect();
            let mut first_error = None;
            for worker in workers {
                match worker.join() {
                    Err(panic) => resume_unwind(panic),
                    Ok(Err(err)) => {
                        first_error.get_or_insert(err);
                    }
                    Ok(Ok(())) => (),
                }
            }
            first_error.map_or(Ok(()), Err)
        })?;
        let mut results = results.lock().expect("lock results").split_off(0);
        results.sort_by_key(|r| match r {
            Tested::Recorded(test) => test.id,
            Tested::NeedsClassic(id, _) => *id,
        });
        Ok(results)
    }

    /// Record a mutant as unviable because of the compile errors that dropped it from
    /// the schema in `phase`, without building it again.
    pub(crate) fn record_unviable(
        &self,
        mutant: &Mutant,
        phase: Phase,
        blame: &[Blame],
    ) -> Result<()> {
        let scenario = Scenario::Mutant(mutant.clone());
        let mut output = self.output.lock().expect("lock output dir");
        let mut scenario_output = output.start_scenario(&scenario)?;
        scenario_output.write_diff(&mutant.diff(&mutant.mutated_code()))?;
        for b in blame {
            scenario_output.message(&format!(
                "unviable: schema {phase} reported error[{code}] in this mutant's arm at {location}: {message}",
                code = b.code.as_deref().unwrap_or("-"),
                location = b.location.as_deref().unwrap_or("-"),
                message = b.message,
            ))?;
        }
        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        outcome.add_phase_result(PhaseResult {
            phase,
            duration: Duration::ZERO,
            process_status: Exit::Failure(101),
            argv: Vec::new(),
        });
        output.add_scenario_outcome(&outcome)?;
        drop(output);
        debug!(
            mutant = mutant.name(true),
            codes = ?blame.iter().map(|b| b.code.as_deref()).collect_vec(),
            "schemata.mutant.unviable_from_check"
        );
        self.console.scenario_finished(
            &self.build_dir.path().join("schemata-unviable"),
            &scenario,
            &outcome,
            self.options,
        );
        Ok(())
    }

    /// Abandon a mutant that the schema missed, when that can't be trusted for
    /// `reason`, so that it is tested the classic way instead.
    fn hand_over_to_classic(
        &self,
        id: MutantId,
        mutant: &Mutant,
        reason: FallbackReason,
        duration: Duration,
        console_key: &Utf8Path,
        scenario_output: &mut ScenarioOutput,
    ) -> Result<()> {
        scenario_output.message(&format!(
            "missed, but that can't be trusted ({reason:?}): testing it the classic way instead"
        ))?;
        debug!(
            id,
            name = mutant.name(true),
            ?reason,
            seconds = duration.as_secs_f64(),
            "schemata.mutant.classic_if_missed"
        );
        self.console.scenario_abandoned(console_key);
        Ok(())
    }

    /// Test a mutant with `run_once` while other mutants may be tested; if its tests
    /// fail but its code never ran, the failure can't have been caused by it: perhaps
    /// a flaky test, or tests of other mutants writing the same files. Then test it
    /// again with nothing else running, and use that result.
    fn retest_if_unreached(
        &self,
        id: MutantId,
        mutant: &Mutant,
        log: &mut ScenarioOutput,
        run_once: impl Fn(&mut ScenarioOutput) -> Result<(Vec<PhaseResult>, Option<MutantSelection>)>,
    ) -> Result<Attempts> {
        let (mut phases, mut selection_record) = {
            let _shared = self.exclusive.read().expect("lock exclusive");
            run_once(log)?
        };
        let process_status = phases
            .last()
            .expect("a mutant's test has at least one phase")
            .process_status;
        let mut reached = self.markers.reached(id);
        let retested = !process_status.is_success() && !reached;
        if retested {
            log.message(&format!(
                "tests failed ({process_status:?}) but mutant {id} never ran, so the failure isn't caused by it: testing again with nothing else running"
            ))?;
            debug!(
                id,
                name = mutant.name(true),
                ?process_status,
                "schemata.mutant.retest_unreached"
            );
            let _alone = self.exclusive.write().expect("lock exclusive");
            (phases, selection_record) = run_once(log)?;
            reached = self.markers.reached(id);
        }
        Ok(Attempts {
            phases,
            selection_record,
            reached,
            retested,
        })
    }

    /// Start the log of testing mutant `id` of `scenario` by `worker`, and show it on
    /// the console, returning the log and the key the console knows it by.
    fn start_mutant(
        &self,
        worker: usize,
        id: MutantId,
        scenario: &Scenario,
    ) -> Result<(ScenarioOutput, Utf8PathBuf)> {
        let mutant = scenario.mutant().expect("scenario has a mutant");
        let mut scenario_output = self
            .output
            .lock()
            .expect("lock output dir")
            .start_scenario(scenario)?;
        scenario_output.write_diff(&mutant.diff(&mutant.mutated_code()))?;
        scenario_output.message(&format!("schemata mutant id {id}"))?;
        // The console tracks work by directory, so give each worker its own key.
        let console_key = self
            .build_dir
            .path()
            .join(format!("schemata-worker-{worker}"));
        self.console
            .scenario_started(&console_key, scenario, scenario_output.open_log_read()?);
        self.console
            .scenario_phase_started(&console_key, Phase::Test);
        Ok((scenario_output, console_key))
    }

    /// Test one mutant, with `cargo test` or, if `commands` are given, by replaying them.
    ///
    /// If the tests fail but the mutant's code never ran, the failure can't have been
    /// caused by the mutant: perhaps a flaky test, or tests of other mutants writing
    /// the same files. Then the tests are run again with nothing else running, and
    /// that result is used.
    ///
    /// Each run of the tests stops as soon as any of `stop_on_failure` fail.
    #[allow(clippy::too_many_arguments)]
    fn test_one(
        &self,
        worker: usize,
        work: Work,
        selection: &PackageSelection,
        commands: Option<&[ReplayCommand]>,
        selector: Option<(Option<&SelectionCoverage>, Confirm)>,
        timeouts: Timeouts,
        stop_on_failure: Option<&KnownTests>,
    ) -> Result<Tested> {
        let Work {
            id,
            mutant,
            classic_if_missed,
            plan,
            ran_in_baseline,
            ..
        } = work;
        let scenario = Scenario::Mutant(mutant);
        let mutant = scenario.mutant().expect("scenario has a mutant");
        let (mut scenario_output, console_key) = self.start_mutant(worker, id, &scenario)?;
        let start = Instant::now();
        let run_once = |log: &mut ScenarioOutput| {
            self.test_mutant(
                id,
                selection,
                commands,
                selector,
                plan.as_ref(),
                ran_in_baseline,
                timeouts.test,
                log,
                stop_on_failure,
            )
        };
        let Attempts {
            phases,
            selection_record,
            reached,
            retested,
        } = self.retest_if_unreached(id, mutant, &mut scenario_output, run_once)?;
        let process_status = phases
            .last()
            .expect("a mutant's test has at least one phase")
            .process_status;
        let duration = start.elapsed();
        self.console
            .scenario_phase_finished(&console_key, Phase::Test);
        if let Some(reason) = classic_if_missed
            && process_status.is_success()
        {
            self.hand_over_to_classic(
                id,
                mutant,
                reason,
                duration,
                &console_key,
                &mut scenario_output,
            )?;
            return Ok(Tested::NeedsClassic(id, reason));
        }
        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        for phase in phases {
            outcome.add_phase_result(phase);
        }
        self.output
            .lock()
            .expect("lock output dir")
            .add_scenario_outcome(&outcome)?;
        self.console
            .scenario_finished(&console_key, &scenario, &outcome, self.options);
        let log = read_to_string(outcome_log_path(&scenario_output))?;
        let result = MutantTest {
            id,
            name: mutant.name(true),
            summary: outcome.summary(),
            seconds: duration.as_secs_f64(),
            lock_waits: lock_waits(&log),
            rebuilt: log
                .lines()
                .any(|line| line.trim_start().starts_with("Compiling ")),
            reached,
            retested,
            selection: selection_record,
        };
        debug!(
            id,
            name = result.name,
            summary = ?result.summary,
            seconds = result.seconds,
            lock_waits = ?result.lock_waits,
            rebuilt = result.rebuilt,
            reached,
            retested,
            "schemata.mutant.done"
        );
        Ok(Tested::Recorded(result))
    }
}

/// Add the options that make `cargo test` print the test commands it runs, with
/// their environment, and the test executables it built.
///
/// They go just after cargo's own `--verbose`, before any `--` that starts the
/// arguments for the test binaries.
pub(crate) fn capture_argv(mut argv: Vec<String>) -> Vec<String> {
    let at = argv
        .iter()
        .position(|arg| arg == "--verbose")
        .map_or(argv.len(), |i| i + 1);
    argv.splice(
        at..at,
        ["--verbose".to_owned(), "--message-format=json".to_owned()],
    );
    argv
}

/// The test cases of the selected tests `indexes` of `coverage`, grouped by the
/// index of their test command, in the order of the commands.
fn cases_by_command<'c>(
    coverage: &'c SelectionCoverage,
    indexes: &[usize],
) -> Vec<(usize, Vec<&'c TestCase>)> {
    indexes
        .iter()
        .map(|index| &coverage.tests()[*index].case)
        .into_group_map_by(|case| case.command)
        .into_iter()
        .sorted_by_key(|(command, _)| *command)
        .collect()
}

/// The commands that are replayed to run all the tests: those that ran any tests in
/// the baseline.
fn replayed(commands: &[ReplayCommand]) -> impl Iterator<Item = &ReplayCommand> {
    commands.iter().filter(|command| !command.idle)
}

fn test_phase(duration: Duration, process_status: Exit, argv: Vec<String>) -> PhaseResult {
    PhaseResult {
        phase: Phase::Test,
        duration,
        process_status,
        argv,
    }
}

fn outcome_log_path(scenario_output: &crate::output::ScenarioOutput) -> Utf8PathBuf {
    scenario_output.output_dir.join(scenario_output.log_path())
}

/// The distinct names of the locks that cargo reported waiting for, like "build directory".
fn lock_waits(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix(LOCK_WAIT_MESSAGE)
                .map(|rest| rest.trim_start_matches(" on ").trim().to_owned())
        })
        .sorted()
        .dedup()
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn capture_argv_adds_options_before_test_binary_arguments() {
        let argv = [
            "cargo",
            "test",
            "--verbose",
            "--package=a",
            "--",
            "--nocapture",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            capture_argv(argv),
            [
                "cargo",
                "test",
                "--verbose",
                "--verbose",
                "--message-format=json",
                "--package=a",
                "--",
                "--nocapture"
            ]
        );
    }

    #[test]
    fn lock_waits_lists_each_lock_once() {
        let log = "\
            *** header\n    Blocking waiting for file lock on package cache\n\
            Blocking waiting for file lock on build directory\n\
            Blocking waiting for file lock on package cache\n\
            running 3 tests\n";
        assert_eq!(lock_waits(log), ["build directory", "package cache"]);
        assert!(lock_waits("running 3 tests\n").is_empty());
    }
}
