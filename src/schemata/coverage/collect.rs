// Copyright 2026 Martin Pool

//! Collect per-test coverage of the unmutated tree.
//!
//! This runs in a copy of the tree of its own, whose target directory is seeded from
//! the schema's build directory once the schema is built. Dependencies are not
//! instrumented, so they're reused, and only the workspace's crates are rebuilt; the
//! schema's own build is not touched.
//!
//! For each package selection:
//!
//! 1. Run `cargo test -vv -- --list --format terse`, with cargo-mutants itself as
//!    `RUSTC_WORKSPACE_WRAPPER` adding `-C instrument-coverage` to the workspace's
//!    crates only. This builds the instrumented tests, and prints each test command
//!    with its environment and the tests it contains.
//! 2. Run each test alone, in parallel, with `LLVM_PROFILE_FILE` pointing into a
//!    directory of its own, so that programs it runs, like `CARGO_BIN_EXE_*`
//!    binaries, write their profiles there too. Convert its profiles to text with
//!    `llvm-profdata` and read which functions ran.
//! 3. Run `llvm-cov export` on the test binaries and the binaries they run, to find
//!    where each function's code is.
//!
//! Doctests are listed but not run: they aren't instrumented.

#![warn(clippy::pedantic)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::path::PathBuf;
use std::process::{Command, exit};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde::Serialize;
use tracing::{debug, info, warn};

use super::listing::{LIST_ARGS, listed_tests};
use super::llvm::{executed_functions, instrumented_rustc_args, mapped_functions};
use super::{
    CollectedRun, FunctionNames, Isolated, NameId, SelectionCoverage, TestCase, TestRun,
    UnobservedFiles, runs_doctests, tree_roots,
};
use crate::Result;
use crate::cargo::cargo_argv;
use crate::interrupt::check_interrupted;
use crate::outcome::Phase;
use crate::package::PackageSelection;
use crate::process::{Env, Exit, Process};
use crate::schemata::replay::{Quoting, ReplayCommand, test_commands_with_output};
use crate::schemata::run::Runner;
use crate::schemata::run::capture_argv;

/// Environment variable that makes cargo-mutants act as a rustc wrapper that
/// instruments crates for coverage. Its value is the `RUSTC_WORKSPACE_WRAPPER` to
/// run in turn, or empty.
pub(crate) const WRAPPER_ENV: &str = "CARGO_MUTANTS_COVERAGE_RUSTC_WRAPPER";

/// Timeout for running one test alone, unless `--timeout` is given.
///
/// A test that takes longer is treated as having incomplete coverage, which only
/// makes selection more conservative.
const ISOLATED_TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum number of unattributed function names in the report.
const UNATTRIBUTED_EXAMPLES: usize = 20;

/// If cargo-mutants was started as a rustc wrapper for coverage, run rustc and exit.
pub(crate) fn run_as_rustc_wrapper_if_requested() {
    let Some(inner) = env::var_os(WRAPPER_ENV) else {
        return;
    };
    let args = instrumented_rustc_args(env::args_os().skip(1).collect());
    let mut command = if inner.is_empty() {
        let Some((rustc, args)) = args.split_first() else {
            eprintln!("cargo-mutants coverage wrapper: no rustc command given");
            exit(1);
        };
        let mut command = Command::new(rustc);
        command.args(args);
        command
    } else {
        let mut command = Command::new(inner);
        command.args(&args);
        command
    };
    match command.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(err) => {
            eprintln!("cargo-mutants coverage wrapper: failed to run {args:?}: {err}");
            exit(1);
        }
    }
}

/// Paths of `llvm-profdata` and `llvm-cov`.
#[derive(Debug, Clone)]
pub(crate) struct LlvmTools {
    profdata: PathBuf,
    cov: PathBuf,
}

impl LlvmTools {
    /// Find the tools: from `LLVM_PROFDATA` and `LLVM_COV`, as used by
    /// `cargo-llvm-cov`, or else from the `llvm-tools` rustup component of the
    /// toolchain that builds the tree in `workspace_root`.
    ///
    /// They must match the LLVM version of rustc, so a system LLVM is not used unless
    /// it's named explicitly.
    pub(crate) fn find(workspace_root: &Utf8Path) -> Result<LlvmTools> {
        LlvmTools::find_with(workspace_root, |name| env::var_os(name), rustc_stdout)
    }

    /// Find the tools, looking up environment variables with `var`, and running rustc
    /// with `rustc(program, dir, args)`, which returns its output.
    ///
    /// rustc is run in `workspace_root`, where a rustup proxy uses the same toolchain
    /// as Cargo does to build the tree, following any `rust-toolchain.toml` there.
    fn find_with(
        workspace_root: &Utf8Path,
        var: impl Fn(&str) -> Option<OsString>,
        rustc: impl Fn(&OsStr, &Utf8Path, &[&str]) -> Result<String>,
    ) -> Result<LlvmTools> {
        if let (Some(profdata), Some(cov)) = (var("LLVM_PROFDATA"), var("LLVM_COV")) {
            return Ok(LlvmTools {
                profdata: profdata.into(),
                cov: cov.into(),
            });
        }
        let program = var("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let sysroot = rustc(&program, workspace_root, &["--print", "sysroot"])?;
        // `--print host-tuple` is newer than the toolchains some trees pin.
        let version = rustc(&program, workspace_root, &["-vV"])?;
        let host = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("no host in rustc -vV")?;
        let bin = PathBuf::from(sysroot.trim())
            .join("lib/rustlib")
            .join(host.trim())
            .join("bin");
        let tools = LlvmTools {
            profdata: bin.join(format!("llvm-profdata{}", env::consts::EXE_SUFFIX)),
            cov: bin.join(format!("llvm-cov{}", env::consts::EXE_SUFFIX)),
        };
        if !tools.profdata.is_file() || !tools.cov.is_file() {
            bail!(
                "--test-selection=coverage needs llvm-profdata and llvm-cov: install them with \
                `rustup component add llvm-tools`, or set LLVM_PROFDATA and LLVM_COV to tools \
                matching rustc's LLVM version (not found in {})",
                bin.display()
            );
        }
        Ok(tools)
    }

    /// Run `llvm-profdata merge` with the given arguments.
    fn merge(&self, args: &[&OsStr]) -> Result<()> {
        let output = Command::new(&self.profdata)
            .arg("merge")
            .args(args)
            .output()
            .with_context(|| format!("run {}", self.profdata.display()))?;
        ensure!(
            output.status.success(),
            "llvm-profdata merge failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}

/// Run `program`, which is rustc, with `args` in `dir`, and return its output.
fn rustc_stdout(program: &OsStr, dir: &Utf8Path, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("run {} {}", program.display(), args.join(" ")))?;
    ensure!(
        output.status.success(),
        "{} {} failed: {}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Coverage collected for one package selection.
#[derive(Debug)]
pub(crate) struct Collected {
    /// The `cargo test` command line of the selection, identifying it.
    pub key: Vec<String>,
    /// The test commands of the instrumented build, without the listing arguments.
    pub commands: Vec<ReplayCommand>,
    /// The paths of the tree where the commands ran (see [`tree_roots`]).
    pub roots: Vec<Utf8PathBuf>,
    pub coverage: SelectionCoverage,
}

/// Counts and timings of coverage collection, for `schemata.json`.
#[derive(Debug, Default, Clone, Serialize)]
pub(crate) struct CollectionReport {
    /// Copying the tree to collect coverage in, seeding its target directory.
    pub copy_seconds: f64,
    /// Building the instrumented tests and listing them.
    pub build_and_list_seconds: f64,
    /// Running each test alone and reading its profile.
    pub run_tests_seconds: f64,
    /// Merging the profiles and mapping functions to source with `llvm-cov export`.
    pub export_seconds: f64,
    pub total_seconds: f64,
    pub workers: usize,
    pub tests: usize,
    pub doctests: usize,
    /// Sum of the tests' durations when run alone.
    pub tests_sum_seconds: f64,
    /// Sum over the tests of the number of functions each executed.
    pub executed_entries: usize,
    /// Number of distinct names of executed functions, and their total length in
    /// bytes: each is stored once, and tests refer to them by 4-byte ids.
    pub executed_names: usize,
    pub executed_name_bytes: usize,
    pub isolated_results: std::collections::BTreeMap<Isolated, usize>,
    pub objects: usize,
    pub mapped_functions: usize,
    pub unattributed_functions: usize,
    /// Some of their names, to help find where they come from.
    pub unattributed_examples: Vec<String>,
}

impl CollectionReport {
    /// Count the runs of one selection's tests, which executed functions in `names`.
    fn add_runs(&mut self, runs: &[CollectedRun], names: &FunctionNames) {
        self.tests += runs.len();
        self.tests_sum_seconds += runs.iter().map(|c| c.run.seconds).sum::<f64>();
        self.executed_entries += runs.iter().map(|c| c.executed.len()).sum::<usize>();
        self.executed_names += names.len();
        self.executed_name_bytes += names.bytes();
        for collected in runs {
            *self
                .isolated_results
                .entry(collected.run.result)
                .or_default() += 1;
        }
    }
}

/// The number of tests run alone at once while collecting coverage.
pub(crate) fn isolated_workers() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

/// Collect coverage for each selection, in the runner's build directory, which must
/// hold the unmutated source.
pub(crate) fn collect(
    runner: &Runner,
    selections: &[PackageSelection],
    tools: &LlvmTools,
    unobserved_files: UnobservedFiles,
) -> Result<(Vec<Collected>, CollectionReport)> {
    let start = Instant::now();
    let build_dir = runner.build_dir.path();
    let scratch = tempfile::Builder::new()
        .prefix("cargo-mutants-coverage-")
        .tempdir()
        .context("create coverage scratch directory")?;
    let scratch_dir = Utf8Path::from_path(scratch.path())
        .ok_or_else(|| anyhow!("coverage scratch directory path is not UTF-8"))?
        .to_owned();
    let _scratch = if runner.options.leak_dirs {
        info!(path = %scratch_dir, "keeping coverage scratch directory");
        let _ = scratch.keep();
        None
    } else {
        Some(scratch)
    };
    let mut report = CollectionReport {
        workers: isolated_workers(),
        ..CollectionReport::default()
    };
    let roots = tree_roots(build_dir);
    let mut collected = Vec::new();
    for (index, selection) in selections.iter().enumerate() {
        let selection_dir = scratch_dir.join(format!("selection-{index}"));
        let phase_start = Instant::now();
        let Listing {
            key,
            commands,
            listed,
        } = list_tests(runner, selection, &selection_dir)?;
        report.build_and_list_seconds += phase_start.elapsed().as_secs_f64();
        let doctests = commands
            .iter()
            .zip(&listed)
            .filter(|(command, _)| runs_doctests(command))
            .map(|(_, tests)| tests.len())
            .sum();
        let cases = test_cases(&commands, listed);
        let phase_start = Instant::now();
        let names = Mutex::new(FunctionNames::default());
        let runs = run_tests(
            runner,
            tools,
            &commands,
            cases,
            &selection_dir,
            report.workers,
            &names,
        )?;
        let names = names.into_inner().expect("unlock function names");
        report.run_tests_seconds += phase_start.elapsed().as_secs_f64();
        let phase_start = Instant::now();
        let objects = objects(&commands);
        let functions = map_functions(tools, &selection_dir, &objects, build_dir)?;
        report.export_seconds += phase_start.elapsed().as_secs_f64();
        report.objects += objects.len();
        report.mapped_functions += functions.len();
        report.doctests += doctests;
        report.add_runs(&runs, &names);
        let coverage = SelectionCoverage::new(&functions, &names, runs, doctests)
            .with_unobserved_files(unobserved_files);
        let unattributed = coverage.unattributed_functions();
        report.unattributed_functions += unattributed.len();
        report.unattributed_examples.extend(
            unattributed
                .iter()
                .take(UNATTRIBUTED_EXAMPLES.saturating_sub(report.unattributed_examples.len()))
                .cloned(),
        );
        collected.push(Collected {
            key,
            commands,
            roots: roots.clone(),
            coverage,
        });
    }
    report.total_seconds = start.elapsed().as_secs_f64();
    debug!(
        seconds = report.total_seconds,
        build_and_list_seconds = report.build_and_list_seconds,
        run_tests_seconds = report.run_tests_seconds,
        export_seconds = report.export_seconds,
        tests = report.tests,
        doctests = report.doctests,
        results = ?report.isolated_results,
        executed_entries = report.executed_entries,
        executed_names = report.executed_names,
        executed_name_bytes = report.executed_name_bytes,
        mapped_functions = report.mapped_functions,
        unattributed_functions = report.unattributed_functions,
        "schemata.coverage.collect.done"
    );
    Ok((collected, report))
}

/// The tests of one selection, as listed by the instrumented build.
struct Listing {
    /// The selection's `cargo test` command line.
    key: Vec<String>,
    /// Its test commands, without the listing arguments.
    commands: Vec<ReplayCommand>,
    /// The tests each command lists.
    listed: Vec<Vec<String>>,
}

/// Build the instrumented tests for one selection and list them.
fn list_tests(
    runner: &Runner,
    selection: &PackageSelection,
    selection_dir: &Utf8Path,
) -> Result<Listing> {
    let options = runner.options;
    let key = cargo_argv(selection, Phase::Test, options);
    // Print the test commands and the executables built, as for the schema baseline.
    let mut argv = capture_argv(key.clone());
    if !argv.iter().any(|arg| arg == "--") {
        argv.push("--".to_owned());
    }
    argv.extend(LIST_ARGS.map(str::to_owned));
    // Programs run while building and listing, like instrumented build scripts,
    // write profiles here, where they're ignored.
    let build_profiles = selection_dir.join("build-profiles");
    create_dir_all(&build_profiles).with_context(|| format!("create {build_profiles}"))?;
    let current_exe = env::current_exe().context("find cargo-mutants executable")?;
    let mut env = runner.cargo_env();
    env.extend([
        (
            "RUSTC_WORKSPACE_WRAPPER".to_owned(),
            current_exe.to_string_lossy().into_owned(),
        ),
        (
            WRAPPER_ENV.to_owned(),
            env::var("RUSTC_WORKSPACE_WRAPPER").unwrap_or_default(),
        ),
        (
            "LLVM_PROFILE_FILE".to_owned(),
            build_profiles.join("%p.profraw").to_string(),
        ),
    ]);
    let (result, log, log_path) = runner.run_step(
        Phase::Test,
        argv,
        &env,
        options.build_timeout,
        "schemata-coverage-list",
    )?;
    ensure!(
        result.is_success(),
        "building and listing instrumented tests failed; see {log_path}"
    );
    let (commands, listed): (Vec<ReplayCommand>, Vec<Vec<String>>) =
        test_commands_with_output(&log, runner.build_dir.path(), Quoting::host())
            .with_context(|| format!("parse test commands from {log_path}"))?
            .into_iter()
            .map(|(command, output)| (command, listed_tests(output)))
            .unzip();
    let commands = commands
        .into_iter()
        .map(|mut command| {
            if !runs_doctests(&command) {
                ensure!(
                    command.argv.ends_with(&LIST_ARGS.map(str::to_owned)),
                    "test command doesn't end with the listing arguments: {:?}",
                    command.argv
                );
                command.argv.truncate(command.argv.len() - LIST_ARGS.len());
            }
            Ok(command)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Listing {
        key,
        commands,
        listed,
    })
}

/// The tests to run alone, from the tests each command listed.
///
/// A test binary that lists no tests might have a custom harness that doesn't
/// support listing, so it's run as a whole, as one test. Doctests aren't run.
fn test_cases(commands: &[ReplayCommand], listed: Vec<Vec<String>>) -> Vec<TestCase> {
    commands
        .iter()
        .zip(listed)
        .enumerate()
        .filter(|(_, (command, _))| !runs_doctests(command))
        .flat_map(|(command, (_, names))| {
            let names = if names.is_empty() {
                vec![None]
            } else {
                names.into_iter().map(Some).collect()
            };
            names
                .into_iter()
                .map(move |name| TestCase { command, name })
        })
        .collect()
}

/// Run each test alone, in parallel, and read which functions it executed, adding
/// their names to `names`.
fn run_tests(
    runner: &Runner,
    tools: &LlvmTools,
    commands: &[ReplayCommand],
    cases: Vec<TestCase>,
    selection_dir: &Utf8Path,
    workers: usize,
    names: &Mutex<FunctionNames>,
) -> Result<Vec<CollectedRun>> {
    let n_cases = cases.len();
    let queue = &Mutex::new(cases.into_iter().enumerate());
    let runs = &Mutex::new(Vec::with_capacity(n_cases));
    let texts_dir = selection_dir.join("texts");
    create_dir_all(&texts_dir).with_context(|| format!("create {texts_dir}"))?;
    let timeout = runner.options.test_timeout.unwrap_or(ISOLATED_TEST_TIMEOUT);
    thread::scope(|scope| -> Result<()> {
        let handles = (0..workers.clamp(1, n_cases.max(1)))
            .map(|_| {
                let texts_dir = &texts_dir;
                scope.spawn(move || -> Result<()> {
                    let mut log = runner
                        .output
                        .lock()
                        .expect("lock output dir")
                        .start_log("schemata-coverage-tests")?;
                    loop {
                        let Some((index, case)) = queue.lock().expect("lock queue").next() else {
                            return Ok(());
                        };
                        let command = &commands[case.command];
                        let profile_dir = selection_dir.join(format!("profiles-{index}"));
                        create_dir_all(&profile_dir)?;
                        let mut env = runner.cargo_env();
                        env.extend(command.env.iter().cloned());
                        env.push((
                            "LLVM_PROFILE_FILE".to_owned(),
                            profile_dir.join("%p.profraw").to_string(),
                        ));
                        let mut argv = command.argv.clone();
                        if let Some(name) = &case.name {
                            argv.extend(["--exact".to_owned(), name.clone()]);
                        }
                        let start = Instant::now();
                        let exit = Process::run(
                            &argv,
                            &Env {
                                set: env,
                                remove: Vec::new(),
                            },
                            &command.cwd,
                            Some(timeout),
                            None,
                            &mut log,
                            runner.console,
                            None,
                        )?;
                        let seconds = start.elapsed().as_secs_f64();
                        check_interrupted()?;
                        let text_path = texts_dir.join(format!("{index}.proftext"));
                        let executed = profile_functions(tools, &profile_dir, &text_path, names)?;
                        remove_dir_all(&profile_dir)?;
                        let result = match (exit, &executed) {
                            (Exit::Timeout, _) => Isolated::TimedOut,
                            (_, None) => Isolated::NoProfile,
                            (Exit::Success, Some(_)) => Isolated::Passed,
                            (_, Some(_)) => Isolated::Failed,
                        };
                        if result != Isolated::Passed {
                            debug!(
                                name = case.name,
                                ?exit,
                                ?result,
                                "schemata.coverage.test.not_passed"
                            );
                        }
                        runs.lock().expect("lock runs").push((
                            index,
                            CollectedRun {
                                run: TestRun {
                                    case,
                                    seconds,
                                    result,
                                },
                                executed: executed.unwrap_or_default(),
                            },
                        ));
                    }
                })
            })
            .collect_vec();
        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(Err(err)) => {
                    first_error.get_or_insert(err);
                }
                Ok(Ok(())) => (),
            }
        }
        first_error.map_or(Ok(()), Err)
    })?;
    let mut runs = runs.lock().expect("lock runs").split_off(0);
    runs.sort_by_key(|(index, _)| *index);
    Ok(runs.into_iter().map(|(_, run)| run).collect())
}

/// Convert the profiles in `profile_dir` to text at `text_path` and return the ids
/// in `names` of the functions that ran, or `None` if there are no profiles.
fn profile_functions(
    tools: &LlvmTools,
    profile_dir: &Utf8Path,
    text_path: &Utf8Path,
    names: &Mutex<FunctionNames>,
) -> Result<Option<Vec<NameId>>> {
    let raw_profiles: Vec<Utf8PathBuf> = read_dir(profile_dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|path| Utf8PathBuf::from_path_buf(path).ok())
        .filter(|path| path.extension() == Some("profraw"))
        .collect();
    if raw_profiles.is_empty() {
        return Ok(None);
    }
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "-sparse".as_ref(),
        "--text".as_ref(),
        "-o".as_ref(),
        text_path.as_ref(),
    ];
    args.extend(raw_profiles.iter().map(|path| path.as_os_str()));
    tools.merge(&args)?;
    let text = read_to_string(text_path).with_context(|| format!("read {text_path}"))?;
    let executed = executed_functions(&text);
    let mut names = names.lock().expect("lock function names");
    Ok(Some(
        executed
            .into_iter()
            .map(|name| names.intern(name))
            .collect(),
    ))
}

/// The instrumented programs that tests run: the test binaries, and the binaries
/// they are given in `CARGO_BIN_EXE_*` variables.
fn objects(commands: &[ReplayCommand]) -> Vec<Utf8PathBuf> {
    commands
        .iter()
        .filter(|command| !runs_doctests(command))
        .flat_map(|command| {
            let bins = command
                .env
                .iter()
                .filter(|(key, _)| key.starts_with("CARGO_BIN_EXE_"))
                .map(|(_, path)| path.as_str());
            std::iter::once(command.argv[0].as_str()).chain(bins)
        })
        .map(Utf8PathBuf::from)
        .filter(|path| path.is_file())
        .unique()
        .collect()
}

/// Merge the tests' profiles, and map the functions in `objects` to source code
/// under `root`, with `llvm-cov export`.
fn map_functions(
    tools: &LlvmTools,
    selection_dir: &Utf8Path,
    objects: &[Utf8PathBuf],
    root: &Utf8Path,
) -> Result<Vec<super::llvm::MappedFunction>> {
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    // Profiles written while listing the tests include every function of each test
    // binary, which gives llvm-cov their hashes even if no test ran them.
    let inputs: Vec<Utf8PathBuf> = [
        selection_dir.join("texts"),
        selection_dir.join("build-profiles"),
    ]
    .iter()
    .filter_map(|dir| read_dir(dir).ok())
    .flatten()
    .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.ok()?.path()).ok())
    .collect();
    let input_list = selection_dir.join("profiles.txt");
    write(&input_list, inputs.iter().join("\n")).with_context(|| format!("write {input_list}"))?;
    let merged = selection_dir.join("merged.profdata");
    tools.merge(&[
        "-o".as_ref(),
        merged.as_ref(),
        "-f".as_ref(),
        input_list.as_ref(),
    ])?;
    let mut command = Command::new(&tools.cov);
    command
        .args(["export", "-format=text", "-skip-expansions"])
        .arg(format!("-instr-profile={merged}"));
    for object in objects {
        command.arg("-object").arg(object);
    }
    let output = command
        .output()
        .with_context(|| format!("run {}", tools.cov.display()))?;
    if !output.status.success() {
        bail!(
            "llvm-cov export failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        debug!(%stderr, "llvm-cov export warnings");
    }
    let roots = tree_roots(root);
    let json = String::from_utf8(output.stdout).context("llvm-cov export output is not UTF-8")?;
    let functions = mapped_functions(&json, &roots).context("parse llvm-cov export output")?;
    if functions.is_empty() {
        warn!("llvm-cov export mapped no functions in the tree");
    }
    Ok(functions)
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use pretty_assertions::assert_eq;

    use super::*;

    /// Make a fake sysroot whose `llvm-tools` component for `host` has the given tools.
    fn fake_sysroot(host: &str, tools: &[&str]) -> tempfile::TempDir {
        let sysroot = tempfile::tempdir().unwrap();
        let bin = sysroot.path().join("lib/rustlib").join(host).join("bin");
        create_dir_all(&bin).unwrap();
        for tool in tools {
            write(bin.join(format!("{tool}{}", env::consts::EXE_SUFFIX)), "").unwrap();
        }
        sysroot
    }

    /// A fake `rustc` that reports `sysroot` when run in `workspace`, and a sysroot
    /// without tools anywhere else, as rustup would for a `rust-toolchain.toml` there.
    fn fake_rustc<'a>(
        workspace: &'a Utf8Path,
        sysroot: &'a Path,
        elsewhere: &'a Path,
    ) -> impl Fn(&OsStr, &Utf8Path, &[&str]) -> Result<String> + 'a {
        move |program, cwd, args| {
            assert_eq!(program, "rustc");
            let sysroot = if cwd == workspace { sysroot } else { elsewhere };
            match args {
                ["--print", "sysroot"] => Ok(format!("{}\n", sysroot.display())),
                ["-vV"] => {
                    Ok("rustc 1.97.1\nbinary: rustc\nhost: x-host\nrelease: 1.97.1\n".into())
                }
                _ => panic!("unexpected rustc args {args:?}"),
            }
        }
    }

    #[test]
    fn llvm_tools_find_with_uses_llvm_tools_of_rustc_run_in_workspace() {
        let workspace = Utf8Path::new("/ws");
        let sysroot = fake_sysroot("x-host", &["llvm-profdata", "llvm-cov"]);
        let other_sysroot = fake_sysroot("x-host", &[]);
        let tools = LlvmTools::find_with(
            workspace,
            |_| None,
            fake_rustc(workspace, sysroot.path(), other_sysroot.path()),
        )
        .unwrap();
        let bin = sysroot.path().join("lib/rustlib/x-host/bin");
        assert_eq!(
            tools.profdata,
            bin.join(format!("llvm-profdata{}", env::consts::EXE_SUFFIX))
        );
        assert_eq!(
            tools.cov,
            bin.join(format!("llvm-cov{}", env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn llvm_tools_find_with_fails_suggesting_rustup_component_when_llvm_cov_is_missing() {
        let workspace = Utf8Path::new("/ws");
        let sysroot = fake_sysroot("x-host", &["llvm-profdata"]);
        let err = LlvmTools::find_with(
            workspace,
            |_| None,
            fake_rustc(workspace, sysroot.path(), sysroot.path()),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("rustup component add llvm-tools"),
            "{err}"
        );
    }

    #[test]
    fn llvm_tools_find_with_prefers_llvm_profdata_and_llvm_cov_variables() {
        let vars = |name: &str| match name {
            "LLVM_PROFDATA" => Some(OsString::from("/llvm/profdata")),
            "LLVM_COV" => Some(OsString::from("/llvm/cov")),
            _ => None,
        };
        let tools = LlvmTools::find_with(Utf8Path::new("/ws"), vars, |_, _, _| {
            panic!("rustc isn't run when both variables are set")
        })
        .unwrap();
        assert_eq!(tools.profdata, Path::new("/llvm/profdata"));
        assert_eq!(tools.cov, Path::new("/llvm/cov"));
    }

    #[test]
    fn llvm_tools_find_with_runs_rustc_named_by_rustc_variable() {
        let vars = |name: &str| (name == "RUSTC").then(|| OsString::from("/custom/rustc"));
        let err = LlvmTools::find_with(Utf8Path::new("/ws"), vars, |program, _, _| {
            assert_eq!(program, "/custom/rustc");
            bail!("custom rustc ran")
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("custom rustc ran"), "{err:#}");
    }

    fn command(program: &str) -> ReplayCommand {
        ReplayCommand {
            env: Vec::new(),
            argv: vec![program.to_owned()],
            cwd: "/ws".into(),
            idle: false,
        }
    }

    #[test]
    fn test_cases_runs_binaries_that_list_no_tests_as_a_whole() {
        let commands = [
            command("/ws/target/debug/deps/lib-1"),
            command("/ws/target/debug/deps/custom_harness-2"),
            command("rustdoc"),
        ];
        let listed = vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec![],
            vec!["src/lib.rs - f (line 1)".to_owned()],
        ];
        assert_eq!(
            test_cases(&commands, listed),
            [
                TestCase {
                    command: 0,
                    name: Some("a".to_owned())
                },
                TestCase {
                    command: 0,
                    name: Some("b".to_owned())
                },
                TestCase {
                    command: 1,
                    name: None
                },
            ]
        );
    }
}
