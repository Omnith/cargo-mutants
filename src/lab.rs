// Copyright 2021-2025 Martin Pool

//! Successively apply mutations to the source code and run cargo to check,
//! build, and test them.

#![warn(clippy::pedantic)]

use std::cmp::{max, min};
use std::panic::resume_unwind;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{thread, vec};

use itertools::Itertools;
use jiff::Timestamp;
use tracing::{debug, debug_span, error, trace, warn};

use crate::{
    BaselineStrategy, BuildDir, Console, Context, Mutant, Options, Phase, Result, Scenario,
    ScenarioOutcome,
    cargo::{cargo_argv, run_cargo},
    check_interrupted,
    fail_fast::KnownTestsBySelection,
    options::TestPackages,
    outcome::LabOutcome,
    output::OutputDir,
    package::Package,
    package::PackageSelection,
    timeouts::Timeouts,
    workspace::Workspace,
};

/// Run all possible mutation experiments.
///
/// This is called after all filtering is complete, so all the mutants here will be tested
/// or checked.
///
/// Before testing the mutants, the lab checks that the source tree passes its tests with no
/// mutations applied.
pub fn test_mutants(
    mut mutants: Vec<Mutant>,
    workspace: &Workspace,
    output_dir: OutputDir,
    options: &Options,
    console: &Console,
) -> Result<LabOutcome> {
    let start_time = Instant::now();
    console.set_debug_log(output_dir.open_debug_log()?);
    if options.shuffle {
        fastrand::shuffle(&mut mutants);
    }
    output_dir.write_mutants_list(&mutants)?;
    console.discovered_mutants(&mutants);
    if mutants.is_empty() {
        warn!("No mutants found under the active filters");
        return Ok(LabOutcome::new(Timestamp::now()));
    }
    let output_mutex = Mutex::new(output_dir);
    let baseline_build_dir = BuildDir::for_baseline(workspace, options, console)?;
    let lab = Lab {
        output_mutex,
        jobserver: make_jobserver(options)?,
        tests_for_mutant: TestsForMutant::new(options, workspace),
        options,
        console,
    };
    let mut known_tests = KnownTestsBySelection::default();
    let timeouts = match options.baseline {
        BaselineStrategy::Run => {
            let selection = baseline_selection(&mutants);
            let outcome = lab.run_baseline(&baseline_build_dir, &selection)?;
            if outcome.success() {
                known_tests.add_baseline(
                    options,
                    cargo_argv(&selection, Phase::Test, options),
                    &outcome.get_log_content()?,
                );
                Timeouts::from_baseline(&outcome, options)
            } else {
                error!(
                    "cargo {phase} failed in an unmutated tree, so no mutants were tested",
                    phase = outcome.last_phase(),
                );
                return lab
                    .output_mutex
                    .into_inner()
                    .expect("lock output_dir")
                    .finish();
            }
        }
        // Without a baseline no test names are known, so tests aren't stopped early.
        BaselineStrategy::Skip => Timeouts::without_baseline(options),
    };
    debug!(?timeouts);

    let n_workers = worker_count(options, mutants.len());
    let build_dirs = lab.ready_build_dirs(baseline_build_dir, n_workers, workspace)?;
    console.start_testing_mutants(mutants.len());
    lab.run_mutants(
        mutants,
        build_dirs,
        n_workers,
        workspace,
        timeouts,
        &known_tests,
    )?;

    let output_dir = lab
        .output_mutex
        .into_inner()
        .expect("final unlock mutants queue");
    console.lab_finished(&output_dir.lab_outcome, start_time, n_workers, options);
    let lab_outcome = output_dir.finish()?;
    if lab_outcome.total_mutants == 0 {
        // This should be unreachable as we also bail out before copying
        // the tree if no mutants are generated.
        warn!("No mutants were generated");
    } else if lab_outcome.unviable == lab_outcome.total_mutants {
        warn!(
            "No mutants were viable: perhaps there is a problem with building in a scratch directory. Look in mutants.out/log/* for more information."
        );
    }
    Ok(lab_outcome)
}

/// Make `n` build dirs whose `target/` is seeded from the baseline build dir, in parallel.
///
/// Returns an empty list if the baseline has no target dir that can be copied, in which case
/// each worker copies its own build dir from the source when it starts.
///
/// Seeding only saves time, so if some dirs can't be made (for example because the disk is
/// full) this warns and returns the ones that were made; other workers copy the source as
/// they would without seeding. Interruptions are still returned as errors.
fn seeded_build_dirs(
    baseline_build_dir: &BuildDir,
    n: usize,
    workspace: &Workspace,
    options: &Options,
    console: &Console,
) -> Result<Vec<BuildDir>> {
    let Some(seed_target) = baseline_build_dir.target_dir_for_seeding() else {
        return Ok(Vec::new());
    };
    let seed_target = &seed_target;
    thread::scope(|scope| {
        let threads = (0..n)
            .map(|_| {
                scope.spawn(|| {
                    BuildDir::copy_seeded(workspace.root(), seed_target, options, console)
                })
            })
            .collect_vec();
        let mut build_dirs = Vec::new();
        for thread in threads {
            match thread.join().unwrap_or_else(|panic| resume_unwind(panic)) {
                Ok(build_dir) => build_dirs.push(build_dir),
                Err(err) => {
                    check_interrupted()?;
                    warn!(
                        "Failed to seed a build directory from the baseline, so it will be built from scratch: {err:#}"
                    );
                }
            }
        }
        Ok(build_dirs)
    })
}

/// The number of workers, each with its own build dir, to test `n_mutants` mutants.
pub(crate) fn worker_count(options: &Options, n_mutants: usize) -> usize {
    max(1, min(options.jobs.unwrap_or(1), n_mutants))
}

/// Test mutants the classic way, when a baseline has already been run elsewhere.
///
/// `build_dir` holds the unmutated tree, ideally already built. Further workers use build
/// dirs seeded from its `target/` (see [`Options::seed_target`]), or copy the workspace.
/// Outcomes are added to `output_dir`, which is returned with the number of workers used.
///
/// A mutant's tests stop at the first failure of one of the `known_tests` of its
/// package selection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn test_mutants_after_baseline(
    mutants: Vec<Mutant>,
    workspace: &Workspace,
    output_dir: OutputDir,
    build_dir: BuildDir,
    timeouts: Timeouts,
    known_tests: &KnownTestsBySelection,
    options: &Options,
    console: &Console,
) -> Result<(OutputDir, usize)> {
    let lab = Lab {
        output_mutex: Mutex::new(output_dir),
        jobserver: make_jobserver(options)?,
        tests_for_mutant: TestsForMutant::new(options, workspace),
        options,
        console,
    };
    let n_workers = worker_count(options, mutants.len());
    let build_dirs = lab.ready_build_dirs(build_dir, n_workers, workspace)?;
    lab.run_mutants(
        mutants,
        build_dirs,
        n_workers,
        workspace,
        timeouts,
        known_tests,
    )?;
    let output_dir = lab.output_mutex.into_inner().expect("unlock output dir");
    Ok((output_dir, n_workers))
}

/// The packages whose tests the baseline runs: all those with mutants.
fn baseline_selection(mutants: &[Mutant]) -> PackageSelection {
    PackageSelection::Explicit(
        mutants
            .iter()
            .map(|m| Arc::clone(&m.source_file.package))
            .sorted_by_key(|p| p.name.clone())
            .unique()
            .collect_vec(),
    )
}

/// Start a jobserver, if the options ask for one.
pub(crate) fn make_jobserver(options: &Options) -> Result<Option<jobserver::Client>> {
    options
        .jobserver
        .then(|| {
            let n_tasks = options.jobserver_tasks.unwrap_or_else(num_cpus::get);
            debug!(n_tasks, "starting jobserver");
            jobserver::Client::new(n_tasks)
        })
        .transpose()
        .context("Start jobserver")
}

#[mutants::skip] // it's a little hard to observe that the threads were collected?
fn join_threads(threads: Vec<thread::ScopedJoinHandle<'_, Result<()>>>) -> Result<()> {
    // The errors potentially returned from `join` are a special `std::thread::Result`
    // that does not implement error, indicating that the thread panicked.
    // Probably the most useful thing is to `resume_unwind` it.
    // Inside that, there's an actual Mutants error indicating a non-panic error.
    // Most likely, this would be "interrupted" but it might be some IO error
    // etc. In that case, print them all and return the first.
    let errors = threads
        .into_iter()
        .filter_map(|thread| match thread.join() {
            Err(panic) => resume_unwind(panic),
            Ok(Ok(())) => None,
            Ok(Err(err)) => {
                // To avoid console spam don't print "interrupted" errors for each thread,
                // since that should have been printed by check_interrupted but do return them.
                if err.to_string() != "interrupted" {
                    error!("Worker thread failed: {:?}", err);
                }
                Some(err)
            }
        })
        .collect_vec();
    if let Some(first_err) = errors.into_iter().next() {
        Err(first_err)
    } else {
        Ok(())
    }
}

/// Common context across all scenarios, threads, and build dirs.
struct Lab<'a> {
    output_mutex: Mutex<OutputDir>,
    jobserver: Option<jobserver::Client>,
    tests_for_mutant: TestsForMutant,
    options: &'a Options,
    console: &'a Console,
}

impl Lab<'_> {
    /// The build dirs for `n_workers` workers to start with: `build_dir_0`, plus build dirs
    /// seeded from its `target/` if that's enabled and there's more than one worker.
    ///
    /// Workers that don't get one of these copy the workspace when they start.
    ///
    /// `build_dir_0` must not be mutated or built while this runs, so that the seeded dirs
    /// copy a complete and consistent target dir.
    fn ready_build_dirs(
        &self,
        build_dir_0: BuildDir,
        n_workers: usize,
        workspace: &Workspace,
    ) -> Result<Vec<BuildDir>> {
        let options = self.options;
        // Only seed from a target dir that the baseline built.
        let seeded = if options.seed_target
            && !options.in_place
            && options.baseline == BaselineStrategy::Run
            && n_workers > 1
        {
            seeded_build_dirs(
                &build_dir_0,
                n_workers - 1,
                workspace,
                options,
                self.console,
            )?
        } else {
            Vec::new()
        };
        Ok(std::iter::once(build_dir_0).chain(seeded).collect_vec())
    }

    /// Test all the mutants on `n_workers` threads, each dedicated to one build dir.
    ///
    /// Workers take a build dir from `build_dirs` if any are left, or otherwise copy the
    /// workspace.
    ///
    /// A worker that fails, while it copies the workspace or while it tests a mutant,
    /// empties the queue first, so that the other workers stop after the mutant each
    /// holds, rather than testing every remaining mutant before the run fails.
    fn run_mutants(
        &self,
        mutants: Vec<Mutant>,
        build_dirs: Vec<BuildDir>,
        n_workers: usize,
        workspace: &Workspace,
        timeouts: Timeouts,
        known_tests: &KnownTestsBySelection,
    ) -> Result<()> {
        let ready_build_dirs = Mutex::new(build_dirs);
        // Each thread tries to take a scenario to test off the queue, and then exits when
        // there are no more left.
        let work_queue = &Mutex::new(mutants.into_iter());
        thread::scope(|scope| -> crate::Result<()> {
            let mut threads = Vec::new();
            for _i_thread in 0..n_workers {
                threads.push(scope.spawn(|| -> crate::Result<()> {
                    trace!(thread_id = ?thread::current().id(), "start thread");
                    let ready_build_dir = ready_build_dirs.lock().expect("lock build dirs").pop(); // separate for lock
                    let result = match ready_build_dir {
                        Some(d) => Ok(d),
                        None => BuildDir::copy_from(workspace.root(), self.options, self.console),
                    }
                    .and_then(|build_dir| {
                        self.make_worker(&build_dir, known_tests)
                            .run_queue(work_queue, timeouts)
                    });
                    if result.is_err() {
                        *work_queue.lock().expect("lock pending work queue") =
                            Vec::new().into_iter();
                    }
                    result
                }));
            }
            join_threads(threads)
        })
    }

    /// Run the baseline scenario, which is the same as running `cargo test` of `selection`
    /// on the unmutated tree.
    ///
    /// The outcome says whether it succeeded, and so whether mutants can be tested.
    fn run_baseline(
        &self,
        build_dir: &BuildDir,
        selection: &PackageSelection,
    ) -> Result<ScenarioOutcome> {
        let no_known_tests = KnownTestsBySelection::default();
        self.make_worker(build_dir, &no_known_tests)
            .run_one_scenario(
                &Scenario::Baseline,
                selection,
                Timeouts::for_baseline(self.options),
            )
    }

    fn make_worker<'a>(
        &'a self,
        build_dir: &'a BuildDir,
        known_tests: &'a KnownTestsBySelection,
    ) -> Worker<'a> {
        Worker {
            build_dir,
            known_tests,
            output_mutex: &self.output_mutex,
            jobserver: self.jobserver.as_ref(),
            tests_for_mutant: &self.tests_for_mutant,
            options: self.options,
            console: self.console,
        }
    }
}

/// A worker owns one build directory and runs a single thread of testing.
///
/// It consumes jobs from an input queue and runs them until the queue is empty,
/// appending output to the output directory.
struct Worker<'a> {
    build_dir: &'a BuildDir,
    /// Tests whose failure stops a mutant's tests, for each package selection.
    known_tests: &'a KnownTestsBySelection,
    output_mutex: &'a Mutex<OutputDir>,
    jobserver: Option<&'a jobserver::Client>,
    tests_for_mutant: &'a TestsForMutant,
    options: &'a Options,
    console: &'a Console,
}

impl Worker<'_> {
    /// Run until the input queue is empty, or until a mutant's scenario fails.
    fn run_queue(
        mut self,
        work_queue: &Mutex<vec::IntoIter<Mutant>>,
        timeouts: Timeouts,
    ) -> Result<()> {
        let _span = debug_span!("worker thread", build_dir = ?self.build_dir.path()).entered();
        loop {
            // Not a `for` statement so that we don't hold the lock
            // for the whole iteration.
            let Some(mutant) = work_queue.lock().expect("Lock pending work queue").next() else {
                return Ok(());
            };
            let _span = debug_span!("mutant", name = mutant.name(false)).entered();
            let test_packages = self.tests_for_mutant.selection(&mutant);
            self.run_one_scenario(&Scenario::Mutant(mutant), &test_packages, timeouts)?;
        }
    }

    fn run_one_scenario(
        &mut self,
        scenario: &Scenario,
        test_packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<ScenarioOutcome> {
        let mut scenario_output = self
            .output_mutex
            .lock()
            .expect("lock output_dir to start scenario")
            .start_scenario(scenario)?;
        let dir = self.build_dir.path();
        self.console
            .scenario_started(dir, scenario, scenario_output.open_log_read()?);
        debug!(?test_packages);
        let stop_on_failure =
            self.known_tests
                .get(&cargo_argv(test_packages, Phase::Test, self.options));

        if let Some(mutant) = scenario.mutant() {
            let mutated_code = mutant.mutated_code();
            let diff = scenario.mutant().unwrap().diff(&mutated_code);
            scenario_output.write_diff(&diff)?;
            mutant.apply(self.build_dir, &mutated_code)?;
        }

        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        for &phase in self.options.phases() {
            self.console.scenario_phase_started(dir, phase);
            let timeout = match phase {
                Phase::Test => timeouts.test,
                Phase::Build | Phase::Check => timeouts.build,
            };
            match run_cargo(
                self.build_dir,
                self.jobserver,
                test_packages,
                phase,
                timeout,
                &mut scenario_output,
                self.options,
                self.console,
                stop_on_failure,
            ) {
                Ok(phase_result) => {
                    let success = phase_result.is_success(); // so we can move it away
                    outcome.add_phase_result(phase_result);
                    self.console.scenario_phase_finished(dir, phase);
                    if !success {
                        break;
                    }
                }
                Err(err) => {
                    error!(?err, ?phase, "scenario execution internal error");
                    // Some unexpected internal error that stops the program.
                    if let Some(mutant) = scenario.mutant() {
                        mutant.revert(self.build_dir)?;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(mutant) = scenario.mutant() {
            mutant.revert(self.build_dir)?;
        }
        self.output_mutex
            .lock()
            .expect("lock output dir to add outcome")
            .add_scenario_outcome(&outcome)?;
        debug!(outcome = ?outcome.summary());
        self.console
            .scenario_finished(dir, scenario, &outcome, self.options);

        Ok(outcome)
    }
}

/// Which packages to test
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestsForMutant {
    /// Test all packages in the workspace
    Workspace,
    /// Test only the package that was mutated
    Mutated,
    /// Test specific packages
    Explicit(Vec<Arc<Package>>),
}

impl TestsForMutant {
    pub(crate) fn new(options: &Options, workspace: &Workspace) -> Self {
        match options.test_package {
            TestPackages::Workspace => TestsForMutant::Workspace,
            TestPackages::Mutated => TestsForMutant::Mutated,
            TestPackages::Named(ref package_names) => {
                TestsForMutant::Explicit(workspace.packages_by_name(package_names))
            }
        }
    }

    /// The packages whose tests should run for a mutant.
    pub(crate) fn selection(&self, mutant: &Mutant) -> PackageSelection {
        match self {
            TestsForMutant::Workspace => PackageSelection::All,
            TestsForMutant::Mutated => {
                PackageSelection::Explicit(vec![mutant.source_file.package.clone()])
            }
            TestsForMutant::Explicit(packages) => PackageSelection::Explicit(packages.clone()),
        }
    }
}
