// Copyright 2021-2025 Martin Pool

//! Run Cargo as a subprocess, including timeouts and propagating signals.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::io::{Read, Seek, SeekFrom};
use std::iter::once;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use camino::Utf8Path;
use nextest_metadata::NextestExitCode;
use regex::Regex;
use serde_json::Value;
use tracing::{debug, debug_span, info, warn};

use crate::Result;
use crate::build_dir::BuildDir;
use crate::console::Console;
use crate::fail_fast::KnownTests;
use crate::interrupt::check_interrupted;
use crate::options::{Options, TestTool};
use crate::outcome::{Phase, PhaseResult};
use crate::output::ScenarioOutput;
use crate::package::PackageSelection;
use crate::process::{Env, Exit, Process, TERMINATES_DESCENDANTS};

// Allowed nextest codes (those will be considered a mutation caught / ignored without a warning)
const NEXTEST_ALLOWED_CODES: &[i32] = &[
    NextestExitCode::NO_TESTS_RUN,
    NextestExitCode::TEST_RUN_FAILED,
    NextestExitCode::BUILD_FAILED,
];

/// Run cargo build, check, or test.
///
/// When testing, if any of `stop_on_failure` fail, the tests are stopped.
///
/// A check or build that fails because the disk is full is an error, not a result: as a
/// result it would make the mutant unviable, and a full disk would hide a missed mutant.
#[allow(clippy::too_many_arguments)] // I agree it's a lot but I'm not sure wrapping in a struct would be better.
pub fn run_cargo(
    build_dir: &BuildDir,
    jobserver: Option<&jobserver::Client>,
    packages: &PackageSelection,
    phase: Phase,
    timeout: Option<Duration>,
    scenario_output: &mut ScenarioOutput,
    options: &Options,
    console: &Console,
    stop_on_failure: Option<&KnownTests>,
) -> Result<PhaseResult> {
    let _span = debug_span!("run", ?phase).entered();
    let start = Instant::now();
    let argv = cargo_argv(packages, phase, options);
    // the log holds earlier phases too: only what this phase appends is read
    let log_start = scenario_output
        .log_file
        .metadata()
        .context("read the log's length")?
        .len();
    let process_status = Process::run(
        &argv,
        &build_dir_cargo_env(build_dir, options),
        build_dir.path(),
        timeout,
        jobserver,
        scenario_output,
        console,
        stop_on_failure.filter(|_| phase == Phase::Test && TERMINATES_DESCENDANTS),
    )?;
    check_interrupted()?;
    debug!(?process_status, elapsed = ?start.elapsed());
    let log_path = scenario_output.output_dir.join(scenario_output.log_path());
    stop_if_disk_full(
        phase,
        process_status,
        || {
            let mut log = scenario_output.open_log_read()?;
            log.seek(SeekFrom::Start(log_start))
                .context("seek to this phase's output in the log")?;
            let mut text = Vec::new();
            log.read_to_end(&mut text)
                .context("read this phase's output from the log")?;
            Ok(String::from_utf8_lossy(&text).into_owned())
        },
        &log_path,
    )?;
    if let Exit::Failure(code) = process_status
        && argv[1] == "nextest"
        && !NEXTEST_ALLOWED_CODES.contains(&code)
    {
        // Nextest returns detailed exit codes. I think we should still treat any non-zero result as
        // just an error, but we can at least warn if it's unexpected.
        warn!(%code, "nextest process exited with unexpected code (allowed: {NEXTEST_ALLOWED_CODES:?})");
    }
    Ok(PhaseResult {
        phase,
        duration: start.elapsed(),
        process_status,
        argv,
    })
}

/// Stop with an error if `phase` is a check or build that failed, and its output says
/// the disk was full. Other phases and other failures are results, not errors.
///
/// This is the one place that decides which phases are checked. `output` reads the
/// phase's output, and is called only for a phase that is checked.
pub(crate) fn stop_if_disk_full<T: AsRef<str>>(
    phase: Phase,
    process_status: Exit,
    output: impl FnOnce() -> Result<T>,
    log_path: &Utf8Path,
) -> Result<()> {
    if !matches!(phase, Phase::Check | Phase::Build) || process_status.is_success() {
        return Ok(());
    }
    if let Some(line) = ran_out_of_disk(output()?.as_ref()) {
        bail!("the disk is full: cargo {phase} failed; see {log_path}, which says: {line}");
    }
    Ok(())
}

/// What a line says when the disk is full, on the platform cargo-mutants runs on. A line
/// says so when it holds every part of one entry.
///
/// Each platform has its own: an error code names a full disk on one platform only.
/// Linux's `(os error 112)` is `EHOSTDOWN`, and Windows' `(os error 28)` is
/// `ERROR_OUT_OF_PAPER`.
const DISK_FULL: &[&[&str]] = if cfg!(windows) {
    // `ERROR_DISK_FULL`, whose text is localized and whose code is not
    &[
        &["There is not enough space on the disk"],
        &["(os error 112)"],
    ]
} else if cfg!(unix) {
    &[
        // the text and the code of `ENOSPC`
        &["No space left on device"],
        &["(os error 28)"],
        // the macOS linker gives only the error number
        &["ld:", "errno=28"],
    ]
} else {
    &[]
};

/// The first line of `text`, written by a failed cargo command, that says the disk was
/// full, trimmed. `None` if no line says so.
///
/// rustc, the archiver, the linker and cargo itself print the operating system's message
/// for the error, which [`DISK_FULL`] holds for this platform. Measured by filling a disk
/// image: `docs/work/fallback-build-cost/design.md`, Measured 13.
pub(crate) fn ran_out_of_disk(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        match line
            .starts_with('{')
            .then(|| serde_json::from_str::<Value>(line).ok())
            .flatten()
        {
            Some(value) => compiler_message_ran_out_of_disk(&value),
            None => plain_line_ran_out_of_disk(line),
        }
    })
}

/// The line of a JSON compiler message's own text, or a child's, that says the disk was
/// full. Its `rendered` text and its spans quote source, so they are never read. Any other
/// JSON line never counts.
///
/// A child can hold several lines: rustc puts the linker's whole output in one.
fn compiler_message_ran_out_of_disk(value: &Value) -> Option<String> {
    if value["reason"] != "compiler-message" {
        return None;
    }
    let diagnostic = &value["message"];
    once(diagnostic)
        .chain(diagnostic["children"].as_array().into_iter().flatten())
        .filter_map(|message| message["message"].as_str())
        .flat_map(str::lines)
        .find(|line| says_disk_full(line))
        .map(|line| line.trim().to_owned())
}

/// A line of plain output, without its colors, if it says the disk was full and no
/// compiler is quoting source on it.
fn plain_line_ran_out_of_disk(line: &str) -> Option<String> {
    let line = without_ansi_escapes(line);
    let text = without_build_script_prefix(&line);
    (!quotes_source(text) && says_disk_full(text)).then(|| line.trim().to_owned())
}

/// The prefix cargo puts on a warning that a build script prints.
static BUILD_SCRIPT_WARNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^warning: [A-Za-z0-9_-]+@[0-9][0-9A-Za-z.+-]*: ").expect("the pattern is valid")
});

/// `line` without the prefix cargo puts on a build script's warning,
/// `warning: <package>@<version>: `.
///
/// cargo replays a build script's warnings on every later build. cc-rs forwards a C
/// compiler's diagnostics as warnings, quoted source included, so a gutter line can follow
/// the prefix.
fn without_build_script_prefix(line: &str) -> &str {
    BUILD_SCRIPT_WARNING
        .find(line)
        .map_or(line, |prefix| &line[prefix.end()..])
}

/// True if `line` holds every part of one entry of [`DISK_FULL`].
fn says_disk_full(line: &str) -> bool {
    DISK_FULL
        .iter()
        .any(|parts| parts.iter().all(|part| line.contains(part)))
}

/// True if rustc is quoting source on `line`: a gutter line that starts with `|`, or a
/// numbered source or suggestion line, such as `30 |`, `4 -` or `4 +`.
fn quotes_source(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('|') {
        return true;
    }
    let after_digits = line.trim_start_matches(|c: char| c.is_ascii_digit());
    let after_spaces = after_digits.trim_start_matches(' ');
    if after_digits.len() == line.len() || after_spaces.len() == after_digits.len() {
        return false;
    }
    let mut rest = after_spaces.chars();
    matches!(rest.next(), Some('|' | '-' | '+' | '~')) && matches!(rest.next(), None | Some(' '))
}

/// `line` without the escape sequences that color it. Cargo colors its output when
/// `CARGO_TERM_COLOR=always`, as CI jobs often set, and then a quoted source line starts
/// with an escape sequence rather than its line number.
fn without_ansi_escapes(line: &str) -> Cow<'_, str> {
    if !line.contains('\x1b') {
        return Cow::Borrowed(line);
    }
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
        } else if chars.next_if_eq(&'[').is_some() {
            // a control sequence ends with a byte from `@` to `~`
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(plain)
}

/// Environment variables set for every cargo invocation.
pub(crate) fn cargo_env(options: &Options) -> Vec<(String, String)> {
    let mut env = vec![
        // The tests might use Insta <https://insta.rs>, and we don't want it to write
        // updates to the source tree, and we *certainly* don't want it to write
        // updates and then let the test pass.
        ("INSTA_UPDATE".to_owned(), "no".to_owned()),
        ("INSTA_FORCE_PASS".to_owned(), "0".to_owned()),
    ];
    if let Some(encoded_rustflags) = encoded_rustflags(options) {
        debug!(?encoded_rustflags);
        env.push(("CARGO_ENCODED_RUSTFLAGS".to_owned(), encoded_rustflags));
    }
    env
}

/// Variables that switch incremental compilation for every cargo command, each with the
/// one value that turns it on. Cargo reads `CARGO_INCREMENTAL` first, then
/// `build.incremental`, whose environment form is `CARGO_BUILD_INCREMENTAL`, then the
/// profile. Measured in `docs/work/fallback-build-cost/design.md`, Measured 10.
const INCREMENTAL_SWITCHES: [(&str, &str); 2] = [
    ("CARGO_INCREMENTAL", "1"),
    ("CARGO_BUILD_INCREMENTAL", "true"),
];

/// The incremental switches in `var` that a scratch build dir removes: those set to
/// anything that turns incremental off. None in place.
///
/// A switch that turns incremental on stays, because removing it would turn incremental
/// off for a profile that says `incremental = false` (Measured 14).
///
/// `var` reads one variable, so that tests don't change the process environment.
pub(crate) fn incremental_switches_to_remove(
    options: &Options,
    var: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    if options.in_place {
        return Vec::new();
    }
    INCREMENTAL_SWITCHES
        .iter()
        .filter(|(name, on)| var(name).is_some_and(|value| value != *on))
        .map(|(name, _)| (*name).to_owned())
        .collect()
}

/// Variables that a scratch build dir overrides with its own value.
const OVERRIDDEN_IN_BUILD_DIRS: [&str; 2] = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"];

/// What a run in scratch build dirs takes from cargo-mutants' environment and changes:
/// each variable that was set, with the value it had.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EnvOverrides {
    /// Set, and replaced by the build dir's own value.
    pub overridden: BTreeMap<String, String>,
    /// Set, and removed, so that cargo config and the profile decide.
    pub removed: BTreeMap<String, String>,
}

/// The variables in `var` that scratch build dirs override or remove; none in place.
///
/// `var` reads one variable, so that tests don't change the process environment.
pub(crate) fn env_overrides(
    options: &Options,
    var: impl Fn(&str) -> Option<String>,
) -> EnvOverrides {
    if options.in_place {
        return EnvOverrides::default();
    }
    let with_value = |name: &str| var(name).map(|value| (name.to_owned(), value));
    EnvOverrides {
        overridden: OVERRIDDEN_IN_BUILD_DIRS
            .iter()
            .filter_map(|name| with_value(name))
            .collect(),
        // the same decision as the removal itself, so the report and the removal agree
        removed: incremental_switches_to_remove(options, &var)
            .iter()
            .filter_map(|name| with_value(name))
            .collect(),
    }
}

/// Report, once per run, what the build dirs change in cargo-mutants' environment.
///
/// `build_dir_cargo_env` runs for every process, including every replayed test, so it
/// reports nothing itself.
pub(crate) fn report_env_overrides(overrides: &EnvOverrides) {
    if overrides.overridden.is_empty() && overrides.removed.is_empty() {
        return;
    }
    debug!(
        overridden = ?overrides.overridden,
        removed = ?overrides.removed,
        "build_dirs.env_overrides"
    );
    if !overrides.removed.is_empty() {
        let removed = overrides
            .removed
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" and ");
        info!(
            "Removed {removed} in scratch build dirs, so cargo config and the profile decide \
            incremental compilation there. To keep it off, set `incremental = false` in the \
            profile, CARGO_PROFILE_<NAME>_INCREMENTAL=false, or `build.incremental = false` \
            in cargo config"
        );
    }
}

/// Environment changes for cargo run in `build_dir`.
///
/// `set` holds those of [`cargo_env`], and `CARGO_TARGET_DIR` naming the build dir's own
/// `target/`, unless mutants are tested in place. A target dir that the user sets in the
/// environment or in cargo config would otherwise be shared by all the build dirs, so that
/// concurrent jobs would build into it at once and test each other's mutants.
/// `CARGO_TARGET_DIR` takes precedence over both.
///
/// `remove` holds the switches [`incremental_switches_to_remove`] names. A scratch build
/// dir rebuilds the mutated package once per mutant, so a switch that a shell or a CI job
/// sets to save disk would make every one of those builds start from nothing.
///
/// In place, there's only one build dir, which is the user's own tree, so their settings
/// are kept.
pub(crate) fn build_dir_cargo_env(build_dir: &BuildDir, options: &Options) -> Env {
    let mut set = cargo_env(options);
    if !options.in_place {
        let target_dir = build_dir.path().join("target");
        set.push(("CARGO_TARGET_DIR".to_owned(), target_dir.into_string()));
    }
    Env {
        set,
        remove: incremental_switches_to_remove(options, |name| env::var(name).ok()),
    }
}

/// Return the name of the cargo binary.
pub fn cargo_bin() -> String {
    // When run as a Cargo subcommand, which is the usual/intended case,
    // $CARGO tells us the right way to call back into it, so that we get
    // the matching toolchain etc.
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// Make up the argv for a cargo check/build/test invocation, including argv[0] as the
/// cargo binary itself.
// (This is split out so it's easier to test.)
pub(crate) fn cargo_argv(
    packages: &PackageSelection,
    phase: Phase,
    options: &Options,
) -> Vec<String> {
    let mut cargo_args = vec![cargo_bin()];
    match phase {
        Phase::Test => match &options.test_tool() {
            TestTool::Cargo => cargo_args.push("test".to_string()),
            TestTool::Nextest => {
                cargo_args.push("nextest".to_string());
                cargo_args.push("run".to_string());
            }
        },
        Phase::Build => {
            match &options.test_tool() {
                TestTool::Cargo => {
                    // These invocations default to the test profile, and might
                    // have other differences? Generally we want to do everything
                    // to make the tests build, but not actually run them.
                    // See <https://github.com/sourcefrog/cargo-mutants/issues/237>.
                    cargo_args.push("test".to_string());
                    cargo_args.push("--no-run".to_string());
                }
                TestTool::Nextest => {
                    cargo_args.push("nextest".to_string());
                    cargo_args.push("run".to_string());
                    cargo_args.push("--no-run".to_string());
                }
            }
        }
        Phase::Check => {
            cargo_args.push("check".to_string());
            cargo_args.push("--tests".to_string());
        }
    }
    if let Some(profile) = &options.profile {
        match options.test_tool() {
            TestTool::Cargo => {
                cargo_args.push(format!("--profile={profile}"));
            }
            TestTool::Nextest => {
                cargo_args.push(format!("--cargo-profile={profile}"));
            }
        }
    }
    cargo_args.push("--verbose".to_string());
    match packages {
        PackageSelection::All => {
            cargo_args.push("--workspace".to_string());
        }
        PackageSelection::Explicit(packages) => {
            cargo_args.extend(
                packages
                    .iter()
                    .map(|p| format!("--package={}", p.version_qualified_name())),
            );
        }
    }
    if options.no_default_features {
        cargo_args.push("--no-default-features".to_owned());
    }
    if options.all_features {
        cargo_args.push("--all-features".to_owned());
    }
    // N.B. it can make sense to have --all-features and also explicit features from non-default packages.
    cargo_args.extend(options.features.iter().map(|f| format!("--features={f}")));
    cargo_args.extend(options.additional_cargo_args.iter().cloned());
    if phase == Phase::Test {
        cargo_args.extend(options.additional_cargo_test_args.iter().cloned());
    }
    cargo_args
}

/// Return adjusted `CARGO_ENCODED_RUSTFLAGS`, including any changes to cap-lints.
///
/// It seems we have to set this in the environment because Cargo doesn't expose
/// a way to pass it in as an option from all commands?
///
/// This does not currently read config files; it's too complicated.
///
/// See <https://doc.rust-lang.org/cargo/reference/environment-variables.html>
/// <https://doc.rust-lang.org/rustc/lints/levels.html#capping-lints>
fn encoded_rustflags(options: &Options) -> Option<String> {
    let cap_lints_arg = "--cap-lints=warn";
    let separator = "\x1f";
    if !options.cap_lints {
        None
    } else if let Ok(encoded) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        if encoded.is_empty() {
            Some(cap_lints_arg.to_owned())
        } else {
            Some(encoded + separator + cap_lints_arg)
        }
    } else if let Ok(rustflags) = env::var("RUSTFLAGS") {
        if rustflags.is_empty() {
            Some(cap_lints_arg.to_owned())
        } else {
            Some(
                rustflags
                    .split(' ')
                    .filter(|s| !s.is_empty())
                    .chain(once("--cap-lints=warn"))
                    .collect::<Vec<&str>>()
                    .join(separator),
            )
        }
    } else {
        Some(cap_lints_arg.to_owned())
    }
}

#[cfg(test)]
mod test {
    use clap::Parser;
    use indoc::indoc;
    use pretty_assertions::assert_eq;
    use rusty_fork::rusty_fork_test;
    use serde_json::json;

    use crate::{
        Args,
        test_util::{single_threaded_remove_env_var, single_threaded_set_env_var},
    };

    use super::*;

    #[test]
    fn build_dir_cargo_env_sets_cargo_target_dir_to_own_target_except_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let build_dir = BuildDir::in_place(tmp.path().try_into().unwrap()).unwrap();
        let target_dir = |options: &Options| {
            build_dir_cargo_env(&build_dir, options)
                .set
                .into_iter()
                .find(|(name, _)| name == "CARGO_TARGET_DIR")
                .map(|(_, value)| value)
        };
        assert_eq!(
            target_dir(&Options::default()),
            Some(build_dir.path().join("target").into_string())
        );
        let in_place = Options {
            in_place: true,
            ..Options::default()
        };
        assert_eq!(target_dir(&in_place), None);
    }

    #[test]
    fn incremental_switches_that_turn_incremental_off_are_removed_except_in_place() {
        let off = |name: &str| match name {
            "CARGO_INCREMENTAL" => Some("0".to_owned()),
            "CARGO_BUILD_INCREMENTAL" => Some("false".to_owned()),
            _ => None,
        };
        assert_eq!(
            incremental_switches_to_remove(&Options::default(), off),
            ["CARGO_INCREMENTAL", "CARGO_BUILD_INCREMENTAL"]
        );
        let in_place = Options {
            in_place: true,
            ..Options::default()
        };
        assert_eq!(
            incremental_switches_to_remove(&in_place, off),
            Vec::<String>::new()
        );
    }

    /// A switch that turns incremental on stays: removing it would turn incremental off for a
    /// profile that says `incremental = false`.
    #[test]
    fn incremental_switches_that_turn_incremental_on_or_are_unset_are_kept() {
        let on = |name: &str| match name {
            "CARGO_INCREMENTAL" => Some("1".to_owned()),
            "CARGO_BUILD_INCREMENTAL" => Some("true".to_owned()),
            _ => None,
        };
        assert_eq!(
            incremental_switches_to_remove(&Options::default(), on),
            Vec::<String>::new()
        );
        assert_eq!(
            incremental_switches_to_remove(&Options::default(), |_| None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn env_overrides_names_each_variable_a_scratch_build_dir_overrides_or_removes() {
        let vars = [("CARGO_TARGET_DIR", "/shared"), ("CARGO_INCREMENTAL", "0")];
        let var = |name: &str| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, value)| (*value).to_owned())
        };
        let overrides = env_overrides(&Options::default(), var);
        assert_eq!(
            overrides.overridden,
            BTreeMap::from([("CARGO_TARGET_DIR".to_owned(), "/shared".to_owned())])
        );
        assert_eq!(
            overrides.removed,
            BTreeMap::from([("CARGO_INCREMENTAL".to_owned(), "0".to_owned())])
        );
    }

    #[test]
    fn env_overrides_is_empty_in_place_or_when_nothing_is_set() {
        let set = |name: &str| Some(format!("value of {name}"));
        let in_place = Options {
            in_place: true,
            ..Options::default()
        };
        assert_eq!(env_overrides(&in_place, set), EnvOverrides::default());
        assert_eq!(
            env_overrides(&Options::default(), |_| None),
            EnvOverrides::default()
        );
    }

    #[test]
    #[cfg(unix)]
    fn ran_out_of_disk_matches_enospc_by_its_text_or_its_code() {
        assert!(
            ran_out_of_disk(
                "error: could not write output to /t/deps/x.rcgu.o: No space left on device\n"
            )
            .is_some()
        );
        assert!(ran_out_of_disk("error: failed to write /t/check (os error 28)\n").is_some());
    }

    /// The text of `ERROR_DISK_FULL` is localized. Its code is not.
    #[test]
    #[cfg(windows)]
    fn ran_out_of_disk_matches_error_disk_full_by_its_text_or_its_code() {
        assert!(
            ran_out_of_disk(
                "error: failed to write /t/x: There is not enough space on the disk.\n"
            )
            .is_some()
        );
        assert!(
            ran_out_of_disk(
                "error: failed to write /t/x: Espace insuffisant sur le disque. (os error 112)\n"
            )
            .is_some()
        );
    }

    /// An error code names a full disk on one platform only.
    #[test]
    fn ran_out_of_disk_does_not_match_another_platforms_error_code() {
        // linux `EHOSTDOWN`
        #[cfg(unix)]
        assert_eq!(
            ran_out_of_disk("error: Host is down (os error 112)\n"),
            None
        );
        // windows `ERROR_OUT_OF_PAPER`
        #[cfg(windows)]
        assert_eq!(
            ran_out_of_disk("error: The printer is out of paper. (os error 28)\n"),
            None
        );
    }

    #[test]
    fn ran_out_of_disk_does_not_match_a_compile_error_or_nothing() {
        assert_eq!(
            ran_out_of_disk("error[E0308]: mismatched types\n  --> src/lib.rs:2:5\n"),
            None
        );
        assert_eq!(ran_out_of_disk(""), None);
    }

    /// A compile error quotes source lines. cargo-mutants' own source holds the message as a
    /// literal, and its CI runs cargo-mutants on itself.
    #[test]
    fn ran_out_of_disk_does_not_match_a_quoted_source_line() {
        assert_eq!(
            ran_out_of_disk(indoc! {r#"
            error[E0308]: mismatched types
              --> src/cargo.rs:30:5
               |
            30 |     "No space left on device",
               |     ^^^^^^^^^^^^^^^^^^^^^^^^^ expected `u8`, found `&str`
        "#}),
            None
        );
        assert_eq!(
            ran_out_of_disk(indoc! {r#"
            help: remove the extra argument
               |
            4  -     report("No space left on device");
            4  +     report();
        "#}),
            None
        );
    }

    /// cargo prints a build script's warning as `warning: <package>@<version>: <text>`, and
    /// replays it on every later build. cc-rs forwards a C compiler's diagnostics that way,
    /// quoted source included.
    #[test]
    fn ran_out_of_disk_reads_a_build_script_warning_without_its_prefix() {
        assert_eq!(
            ran_out_of_disk(
                "warning: p3@0.1.0:     3 |     const char *unused_fallback = \"No space left on device\";\n"
            ),
            None
        );
        #[cfg(unix)]
        assert_eq!(
            ran_out_of_disk(
                "warning: p3@0.1.0: error: could not write /t/x.o: No space left on device\n"
            )
            .as_deref(),
            Some("warning: p3@0.1.0: error: could not write /t/x.o: No space left on device")
        );
    }

    /// Only cargo's own prefix for a build script's warning comes off: a package name, `@`,
    /// and a version that starts with a digit.
    #[test]
    fn without_build_script_prefix_takes_off_only_a_package_and_version() {
        assert_eq!(without_build_script_prefix("warning: p3@0.1.0: x"), "x");
        assert_eq!(
            without_build_script_prefix("warning: cc_rs-2@1.2.3-rc.1+b.5: x"),
            "x"
        );
        for kept in [
            "warning: unused import: x",
            "warning: a b@0.1.0: x",
            "warning: p3@v1: x",
        ] {
            assert_eq!(without_build_script_prefix(kept), kept);
        }
    }

    /// rustc quotes source only on a gutter line: a number, a space, then one of `|-+~`
    /// followed by a space or the end of the line, or a bare `|`.
    #[test]
    fn quotes_source_only_on_rustc_gutter_lines() {
        assert!(quotes_source("   |     ^^^^ expected `u8`"));
        assert!(quotes_source("30 |     let x = 1;"));
        assert!(quotes_source("4  -"));
        // no space between the number and the bar
        assert!(!quotes_source("4| x"));
        // the symbol is not followed by a space
        assert!(!quotes_source("4 -> x"));
    }

    /// The macOS linker reports a full disk by its error number only.
    #[test]
    #[cfg(unix)]
    fn ran_out_of_disk_matches_the_linker_only_for_errno_28() {
        assert!(
            ran_out_of_disk(indoc! {"
            error: linking with `cc` failed: exit status: 1
              = note: ld: ftruncate() failed, errno=28 for '/t/deps/x-1234'
        "})
            .is_some()
        );
        assert_eq!(
            ran_out_of_disk(indoc! {"
            error: linking with `cc` failed: exit status: 1
              = note: ld: library 'z' not found
        "}),
            None
        );
    }

    /// With `--message-format=json`, rustc puts the linker's output in a child message.
    #[test]
    #[cfg(unix)]
    fn ran_out_of_disk_matches_the_linker_in_a_json_compiler_message() {
        let linker = json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "linking with `cc` failed: exit status: 1",
                "children": [
                    {"level": "note", "message": "\"cc\" \"-arch\" \"arm64\" \"/t/deps/x.o\"", "children": [], "spans": [], "rendered": null},
                    {"level": "note", "message": "ld: ftruncate() failed, errno=28 for '/t/deps/x-1234'\nclang: error: linker command failed with exit code 1", "children": [], "spans": [], "rendered": null},
                ],
                "spans": [],
                "rendered": "error: linking with `cc` failed: exit status: 1\n",
            },
        });
        assert_eq!(
            ran_out_of_disk(&linker.to_string()).as_deref(),
            Some("ld: ftruncate() failed, errno=28 for '/t/deps/x-1234'")
        );
    }

    /// With `--message-format=json`, which the schema's check and build use, a diagnostic is one
    /// line. Its own message counts. Its rendered text and spans quote source, so they don't.
    #[test]
    fn ran_out_of_disk_reads_only_the_messages_of_a_json_compiler_message() {
        #[cfg(unix)]
        let full = json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "could not write output to /t/deps/x.rcgu.o: No space left on device",
                "children": [],
                "spans": [],
                "rendered": "error: could not write output to /t/deps/x.rcgu.o: No space left on device\n",
            },
        });
        #[cfg(unix)]
        assert!(ran_out_of_disk(&full.to_string()).is_some());
        let quoted = json!({
            "reason": "compiler-message",
            "message": {
                "level": "warning",
                "message": "unused variable: `expected`",
                "children": [{
                    "level": "help",
                    "message": "if this is intentional, prefix it with an underscore: `_expected`",
                    "children": [],
                    "spans": [],
                    "rendered": null,
                }],
                "spans": [{"text": [{"text": "    let expected = \"No space left on device\";"}]}],
                "rendered": "warning: unused variable: `expected`\n --> src/lib.rs:4:9\n  |\n4 |     let expected = \"No space left on device\";\n",
            },
        });
        assert_eq!(ran_out_of_disk(&quoted.to_string()), None);
    }

    /// With `CARGO_TERM_COLOR=always`, which the fork's CI sets, cargo colors its output,
    /// and a quoted source line starts with an escape sequence rather than its number.
    #[test]
    fn ran_out_of_disk_does_not_match_a_colored_quoted_source_line() {
        assert_eq!(
            ran_out_of_disk(
                "\x1b[1m\x1b[94m12\x1b[0m \x1b[1m\x1b[94m|\x1b[0m         let expected = \"No space left on device\";\n"
            ),
            None
        );
        #[cfg(unix)]
        assert!(
            ran_out_of_disk(
                "\x1b[1m\x1b[91merror\x1b[0m\x1b[1m: No space left on device (os error 28)\x1b[0m\n"
            )
            .is_some()
        );
    }

    /// Only a failed check or build is read. A test's own output can say the disk is
    /// full, and a test that fails is a caught mutant. The error quotes the line that
    /// matched, so a false stop explains itself.
    #[test]
    fn stop_if_disk_full_reads_only_a_failed_check_or_build() {
        let log_path = Utf8Path::new("log/x.log");
        let line = if cfg!(windows) {
            "error: There is not enough space on the disk."
        } else {
            "error: No space left on device"
        };
        let full = || -> Result<String> { Ok(format!("   Compiling x v0.1.0\n{line}\n")) };
        let unread = || -> Result<&str> { bail!("the output was read") };
        for phase in [Phase::Check, Phase::Build] {
            let err = stop_if_disk_full(phase, Exit::Failure(101), full, log_path).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "the disk is full: cargo {phase} failed; see log/x.log, which says: {line}"
                )
            );
            stop_if_disk_full(phase, Exit::Success, unread, log_path).unwrap();
        }
        stop_if_disk_full(Phase::Test, Exit::Failure(101), unread, log_path).unwrap();
    }

    #[test]
    fn generate_cargo_args_for_baseline_with_default_options() {
        let options = Options::default();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            ["check", "--tests", "--verbose", "--workspace"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Build, &options)[1..],
            ["test", "--no-run", "--verbose", "--workspace"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            ["test", "--verbose", "--workspace"]
        );
    }

    #[test]
    fn generate_cargo_args_with_additional_cargo_test_args_and_package() {
        let mut options = Options::default();
        options
            .additional_cargo_test_args
            .extend(["--lib", "--no-fail-fast"].iter().map(ToString::to_string));
        assert_eq!(
            cargo_argv(
                &PackageSelection::one(
                    "cargo-mutants-testdata-something",
                    "0.1.0",
                    "",
                    "src/lib.rs"
                ),
                Phase::Check,
                &options
            )[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--package=cargo-mutants-testdata-something@0.1.0",
            ]
        );
    }

    #[test]
    fn generate_cargo_args_with_additional_cargo_args_and_test_args() {
        let mut options = Options::default();
        options
            .additional_cargo_test_args
            .extend(["--lib", "--no-fail-fast"].iter().map(|&s| s.to_string()));
        options
            .additional_cargo_args
            .extend(["--release".to_owned()]);
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            ["check", "--tests", "--verbose", "--workspace", "--release"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Build, &options)[1..],
            ["test", "--no-run", "--verbose", "--workspace", "--release"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            [
                "test",
                "--verbose",
                "--workspace",
                "--release",
                "--lib",
                "--no-fail-fast"
            ]
        );
    }

    #[test]
    fn no_default_features_args_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--no-default-features"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--workspace",
                "--no-default-features"
            ]
        );
    }

    #[test]
    fn all_features_args_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--all-features"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--workspace",
                "--all-features"
            ]
        );
    }

    #[test]
    fn cap_lints_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--cap-lints=true"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            ["check", "--tests", "--verbose", "--workspace",]
        );
    }

    #[test]
    fn feature_args_passed_to_cargo() {
        let args = Args::try_parse_from(
            ["mutants", "--features", "foo", "--features", "bar,baz"].as_slice(),
        )
        .unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--workspace",
                "--features=foo",
                "--features=bar,baz"
            ]
        );
    }

    #[test]
    fn profile_arg_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--profile", "mutants"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--profile=mutants",
                "--verbose",
                "--workspace",
            ]
        );
    }

    #[test]
    fn nextest_gets_special_cargo_profile_option() {
        let args = Args::try_parse_from(
            ["mutants", "--test-tool=nextest", "--profile", "mutants"].as_slice(),
        )
        .unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Build, &options)[1..],
            [
                "nextest",
                "run",
                "--no-run",
                "--cargo-profile=mutants",
                "--verbose",
                "--workspace",
            ]
        );
    }

    rusty_fork_test! {
        #[test]
        fn rustflags_without_cap_lints_and_no_environment_variables() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    ..Default::default()
                }),
                None
            );
        }
        #[test]
        fn rustflags_with_cap_lints_and_no_environment_variables() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    cap_lints: true,
                    ..Default::default()
                }),
                Some("--cap-lints=warn".into())
            );
        }

        // Don't generate an empty argument if the encoded rustflags is empty.
        #[test]
        fn rustflags_with_empty_encoded_rustflags() {
            single_threaded_set_env_var("CARGO_ENCODED_RUSTFLAGS", "");
            assert_eq!(
                encoded_rustflags(&Options {
                    cap_lints: true,
                    ..Default::default()
                }).unwrap(),
                "--cap-lints=warn"
            );
        }

        #[test]
        fn rustflags_added_to_existing_encoded_rustflags() {
            single_threaded_set_env_var("RUSTFLAGS", "--something\x1f--else");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            let options = Options {
                cap_lints: true,
                ..Default::default()
            };
            assert_eq!(encoded_rustflags(&options).unwrap(), "--something\x1f--else\x1f--cap-lints=warn");
        }

        #[test]
        fn rustflags_added_to_existing_rustflags() {
            single_threaded_set_env_var("RUSTFLAGS", "-Dwarnings");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(encoded_rustflags(&Options {
                cap_lints: true,
                ..Default::default()
            }).unwrap(), "-Dwarnings\x1f--cap-lints=warn");
        }
    }
}
