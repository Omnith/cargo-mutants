// Copyright 2026 Martin Pool

//! Stop a test run as soon as the test harness reports that a test failed.
//!
//! libtest on stable Rust has no way to stop a test binary at its first failure,
//! so a caught mutant waits for every other test in the binary, however slow.
//! But libtest reports each result as soon as it's known, as a line like
//! `test path::to::name ... FAILED`, and once it has reported a failure the binary
//! will exit unsuccessfully, and so will `cargo test`. The outcome of the mutant is
//! then already known, and the tests can be killed.
//!
//! Tests can write arbitrary text to the same stdout and stderr, for example from
//! child processes whose output isn't captured, so a line that merely looks like a
//! failure report isn't trusted: the test name must be one that libtest reported as
//! `ok` in the baseline run of the unmutated tree.

#![warn(clippy::pedantic)]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory};
use itertools::Itertools;
use tracing::debug;

use crate::mutant::Mutant;
use crate::options::{Options, TestTool};
use crate::{Args, Result};

/// The tests whose failure stops a mutant's tests, learned from the baseline's log.
///
/// Returns `None` if tests shouldn't be stopped: if the options turn it off, if
/// nextest is used (it has its own fail-fast), if `--no-fail-fast` asks cargo to run
/// all the tests, or if no test results are found in the log, as when libtest is
/// asked for its terse format.
fn known_tests(options: &Options, baseline_log: &str) -> Option<KnownTests> {
    if !options.stop_tests_on_failure
        || options.test_tool() == TestTool::Nextest
        || options
            .additional_cargo_test_args
            .iter()
            .any(|arg| arg == "--no-fail-fast")
    {
        return None;
    }
    let known_tests = KnownTests::from_log(baseline_log);
    debug!(
        n_tests = known_tests.names.len(),
        "tests whose failure stops a mutant's tests"
    );
    if known_tests.names.is_empty() {
        None
    } else {
        Some(known_tests)
    }
}

/// The tests whose failure stops a mutant's tests, for each package selection whose
/// baseline ran, keyed by the arguments of that selection's `cargo test` command.
///
/// Names are only trusted for the selection whose baseline reported them.
#[derive(Debug, Default)]
pub struct KnownTestsBySelection {
    by_argv: HashMap<Vec<String>, KnownTests>,
}

impl KnownTestsBySelection {
    /// Learn the tests of the selection tested by `cargo_test_argv` from its baseline
    /// log, unless tests shouldn't be stopped.
    pub fn add_baseline(
        &mut self,
        options: &Options,
        cargo_test_argv: Vec<String>,
        baseline_log: &str,
    ) {
        match known_tests(options, baseline_log) {
            Some(known_tests) => self.by_argv.insert(cargo_test_argv, known_tests),
            None => self.by_argv.remove(&cargo_test_argv),
        };
    }

    /// The known tests of the selection tested by `cargo_test_argv`, if any.
    pub fn get(&self, cargo_test_argv: &[String]) -> Option<&KnownTests> {
        self.by_argv.get(cargo_test_argv)
    }
}

/// Names of tests that passed in the baseline, whose failure can stop a later run.
#[derive(Debug)]
pub struct KnownTests {
    names: HashSet<String>,
}

impl KnownTests {
    /// Collect the names of tests that libtest reported as passing in `log`.
    pub fn from_log(log: &str) -> KnownTests {
        KnownTests {
            names: log
                .lines()
                .filter_map(|line| result_line(line, "ok"))
                .map(str::to_owned)
                .collect(),
        }
    }

    /// If `line` is libtest's report that a known test failed, return the test's name.
    pub fn failed_test<'l>(&self, line: &'l str) -> Option<&'l str> {
        result_line(line, "FAILED").filter(|name| self.names.contains(*name))
    }
}

/// The number of tests that libtest reported as passing in `log`.
pub fn passed_tests(log: &str) -> usize {
    log.lines()
        .filter(|line| result_line(line, "ok").is_some())
        .count()
}

/// If `line` is libtest's report of a test with the given result, return the test name.
///
/// libtest's default "pretty" format reports each test as `test NAME ... RESULT` on
/// its own line.
fn result_line<'l>(line: &'l str, result: &str) -> Option<&'l str> {
    line.strip_suffix('\r')
        .unwrap_or(line)
        .strip_prefix("test ")?
        .strip_suffix(result)?
        .strip_suffix(" ... ")
}

/// Watches a log file, as a process appends to it, for a known test's failure.
pub struct FailureWatch<'k> {
    known_tests: &'k KnownTests,
    log: File,
    /// Bytes read after the last complete line.
    partial_line: Vec<u8>,
}

impl<'k> FailureWatch<'k> {
    /// Start watching text appended to `log_path` from now on.
    pub fn new(known_tests: &'k KnownTests, log_path: &Utf8Path) -> Result<FailureWatch<'k>> {
        let mut log = File::open(log_path).with_context(|| format!("open {log_path} to watch"))?;
        log.seek(SeekFrom::End(0))
            .with_context(|| format!("seek to end of {log_path}"))?;
        Ok(FailureWatch {
            known_tests,
            log,
            partial_line: Vec::new(),
        })
    }

    /// Read what's been appended since the last call, and return the name of the first
    /// known test reported as failing, if any.
    pub fn poll(&mut self) -> Result<Option<String>> {
        self.log
            .read_to_end(&mut self.partial_line)
            .context("read log to watch for test failures")?;
        let Some(end) = self.partial_line.iter().rposition(|&b| b == b'\n') else {
            return Ok(None);
        };
        let complete: Vec<u8> = self.partial_line.drain(..=end).collect();
        Ok(String::from_utf8_lossy(&complete)
            .lines()
            .find_map(|line| self.known_tests.failed_test(line))
            .map(str::to_owned))
    }
}

/// Arguments of the run that its rerun command leaves out, by their clap ids: those
/// that select mutants, shape the run, or name its output, which the rerun sets itself.
const NOT_REPEATED: &[&str] = &[
    // Selection of mutants: the rerun selects its mutant with `--re`.
    "examine_re",
    "exclude_re",
    "file",
    "exclude",
    "in_diff",
    "iterate",
    "shard",
    "sharding",
    // Shape of the run.
    "jobs",
    "shuffle",
    "no_shuffle",
    "list",
    "list_files",
    "json",
    "stop_tests_on_failure",
    // Output: the rerun has its own, so as not to rotate the run's.
    "output",
    // Not a test run at all.
    "completions",
    "emit_schema",
    "mutate_file",
    "version",
];

/// Name of the directory, within the run's `mutants.out`, in which reruns write their
/// own `mutants.out`.
const RERUN_OUTPUT_DIR: &str = "rerun";

/// How to rerun one mutant with the complete output of its tests, suggested in its
/// log when its tests are stopped at the first failure.
///
/// The rerun repeats the run's command-line options, except those that select mutants
/// or shape the run, so that it builds and tests the mutant the same way. It writes its
/// output inside the run's `mutants.out`, so it doesn't rotate the run's output away.
#[derive(Debug, Clone, Default)]
pub struct Rerun {
    /// Options of the run that the rerun repeats, as `--name=value` or `--name`.
    options: Vec<String>,
    /// Arguments for `cargo test`, given after `--`.
    cargo_test_args: Vec<String>,
    /// Where the rerun writes its `mutants.out`.
    output: Option<Utf8PathBuf>,
}

impl Rerun {
    /// Learn how to rerun a mutant from the `matches` of the run's command line, and
    /// the path of its `mutants.out` directory.
    pub fn new(matches: &ArgMatches, run_output: &Utf8Path) -> Rerun {
        let mut rerun = Rerun {
            output: Some(run_output.join(RERUN_OUTPUT_DIR)),
            ..Rerun::default()
        };
        for arg in Args::command().get_arguments() {
            let id = arg.get_id().as_str();
            if NOT_REPEATED.contains(&id)
                || matches.value_source(id) != Some(ValueSource::CommandLine)
            {
                continue;
            }
            let values = matches
                .get_raw_occurrences(id)
                .into_iter()
                .flatten()
                .flatten()
                .map(|value| value.to_string_lossy().into_owned());
            if arg.is_last_set() {
                rerun.cargo_test_args.extend(values);
                continue;
            }
            let Some(long) = arg.get_long() else {
                debug!(id, "Not repeating an option without a long name in reruns");
                continue;
            };
            if arg.get_action().takes_values() {
                rerun
                    .options
                    .extend(values.map(|value| format!("--{long}={value}")));
            } else {
                rerun.options.push(format!("--{long}"));
            }
        }
        rerun
    }

    /// A shell command that tests only `mutant`, without stopping its tests early.
    pub fn command(&self, mutant: &Mutant) -> String {
        let name_re = format!("^{}$", regex::escape(&mutant.name(true)));
        let mut words = vec![
            "cargo",
            "mutants",
            "--re",
            &name_re,
            "--stop-tests-on-failure=false",
        ];
        if let Some(output) = &self.output {
            words.extend(["-o", output.as_str()]);
        }
        words.extend(self.options.iter().map(String::as_str));
        if !self.cargo_test_args.is_empty() {
            words.push("--");
            words.extend(self.cargo_test_args.iter().map(String::as_str));
        }
        words.into_iter().map(shell_word).join(" ")
    }
}

/// Quote `word` for a shell, if it needs it: for a POSIX shell, or for PowerShell on
/// Windows.
fn shell_word(word: &str) -> String {
    if cfg!(windows) {
        powershell_word(word)
    } else {
        posix_word(word)
    }
}

/// True if `word` means the same to a POSIX shell and PowerShell without quotes.
fn is_plain_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,%".contains(c))
}

/// Quote `word` for a POSIX shell, if it needs it.
fn posix_word(word: &str) -> String {
    if is_plain_word(word) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Quote `word` for PowerShell, if it needs it.
///
/// In a single-quoted PowerShell string, a single quote is written twice. PowerShell
/// takes the typographic quotes U+2018 to U+201B as single quotes too.
fn powershell_word(word: &str) -> String {
    if is_plain_word(word) {
        return word.to_owned();
    }
    let mut quoted = String::with_capacity(word.len() + 2);
    quoted.push('\'');
    for c in word.chars() {
        if matches!(c, '\'' | '\u{2018}'..='\u{201B}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod test {
    use std::fs::OpenOptions;
    use std::io::Write;

    use clap::Parser;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::config::Config;
    use crate::visit::mutate_source_str;

    const BASELINE_LOG: &str = indoc! {r"
        *** cargo test
           Compiling foo v0.1.0 (/ws/foo)
            Running unittests src/lib.rs (target/debug/deps/foo-0123)

        running 4 tests
        test a::passes ... ok
        test a::is_ignored ... ignored
        test a::is_ignored_with_reason ... ignored, too slow
        test a::panics - should panic ... ok

        test result: ok. 2 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out

           Doc-tests foo

        running 1 test
        test src/lib.rs - f (line 3) ... ok
    "};

    fn known() -> KnownTests {
        KnownTests::from_log(BASELINE_LOG)
    }

    fn options(args: &[&str]) -> Options {
        let args = Args::parse_from(["mutants"].iter().chain(args));
        Options::new(&args, &Config::default()).unwrap()
    }

    /// Split a command made by [`Rerun::command`] into words, undoing [`shell_word`].
    fn split_command(command: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word: Option<String> = None;
        let mut chars = command.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ' ' => words.extend(word.take()),
                '\\' if !cfg!(windows) => {
                    let escaped = chars.next().expect("escaped character");
                    word.get_or_insert_default().push(escaped);
                }
                '\'' => {
                    let word = word.get_or_insert_default();
                    loop {
                        let c = chars.next().expect("closing quote");
                        let is_quote = if cfg!(windows) {
                            matches!(c, '\'' | '\u{2018}'..='\u{201B}')
                        } else {
                            c == '\''
                        };
                        if !is_quote {
                            word.push(c);
                        } else if cfg!(windows) && chars.peek() == Some(&c) {
                            word.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                c => word.get_or_insert_default().push(c),
            }
        }
        words.extend(word);
        words
    }

    /// The value of `flag` in the command line `words`.
    fn value_after<'w>(words: &'w [String], flag: &str) -> &'w str {
        let i = words
            .iter()
            .position(|w| w == flag)
            .unwrap_or_else(|| panic!("{flag} in {words:?}"));
        &words[i + 1]
    }

    fn matches(args: &[&str]) -> ArgMatches {
        Args::command()
            .try_get_matches_from(["mutants"].iter().chain(args))
            .unwrap()
    }

    const RERUN_SOURCE: &str = indoc! {r#"
        fn greeting(name: &str) -> &'static str {
            if name.len() > 1 && name != "x" { "hello" } else { "hi" }
        }
        fn scale(a: u32, b: u32) -> u32 {
            a * b + (a - 1)
        }
    "#};

    #[test]
    fn rerun_command_re_matches_exactly_its_mutant() {
        let mutants = mutate_source_str(RERUN_SOURCE, &Options::default()).unwrap();
        assert!(
            mutants
                .iter()
                .any(|m| m.name(true).contains("&'static str")),
            "some mutant names need quoting"
        );
        let rerun = Rerun::new(&matches(&[]), Utf8Path::new("mutants.out"));
        for mutant in &mutants {
            let command = rerun.command(mutant);
            let words = split_command(&command);
            assert_eq!(words[..2], ["cargo", "mutants"], "{command}");
            let options = options(&["--re", value_after(&words, "--re")]);
            let selected = mutants
                .iter()
                .filter(|m| options.allows_mutant(m))
                .map(|m| m.name(true))
                .collect_vec();
            assert_eq!(selected, [mutant.name(true)], "{command}");
        }
    }

    /// Options that a rerun repeats, and so parses the same way, each with its value.
    const REPEATED_OPTIONS: &[&[&str]] = &[
        &["-d", "my tree"],
        &["--features=a b"],
        &["--no-default-features"],
        &["--profile=fast"],
        &["--config", "my config.toml"],
        &["--test-package=other"],
        &["-C", "--release"],
        &["--cargo-test-arg=--doc"],
        &["--timeout=30"],
        &["--build-timeout-multiplier=3"],
        &["--test-tool=nextest"],
        &["--baseline=skip"],
        &["--cap-lints=true"],
        &["--error=it's"],
        &["--no-schemata"],
        &["-p", "pkg"],
        &["-v"],
    ];

    /// Options that select mutants or shape the run, which a rerun omits.
    const OMITTED_OPTIONS: &[&[&str]] = &[
        &["--re", "other"],
        &["-E", "excluded"],
        &["-f", "src/*.rs"],
        &["-e", "src/main.rs"],
        &["--shard=1/4"],
        &["--sharding=round-robin"],
        &["--in-diff=my.diff"],
        &["--iterate"],
        &["-j3"],
        &["--shuffle"],
        &["-o", "elsewhere"],
        &["--stop-tests-on-failure=true"],
        &["--list"],
        &["--json"],
    ];

    #[test]
    fn rerun_command_repeats_options_of_the_run_except_selection_and_output() {
        let ids = Args::command()
            .get_arguments()
            .map(|arg| arg.get_id().to_string())
            .collect_vec();
        for id in NOT_REPEATED {
            assert!(ids.iter().any(|i| i == id), "{id} is not an argument");
        }
        let mutant = &mutate_source_str(RERUN_SOURCE, &Options::default()).unwrap()[0];
        let trailing = ["--", "--test-threads=1"];
        let run_args = OMITTED_OPTIONS
            .iter()
            .interleave(REPEATED_OPTIONS)
            .flat_map(|option| option.iter())
            .chain(&trailing)
            .copied()
            .collect_vec();
        let command =
            Rerun::new(&matches(&run_args), Utf8Path::new("out dir/mutants.out")).command(mutant);
        let words = split_command(&command);
        assert_eq!(words[..2], ["cargo", "mutants"], "{command}");
        let rerun_args = Args::try_parse_from(
            ["mutants"]
                .into_iter()
                .chain(words[2..].iter().map(String::as_str)),
        )
        .unwrap_or_else(|err| panic!("{command}: {err}"));
        let name_re = format!("^{}$", regex::escape(&mutant.name(true)));
        let rerun_options = [
            "--re",
            &name_re,
            "--stop-tests-on-failure=false",
            "-o",
            "out dir/mutants.out/rerun",
        ];
        let expected = Args::try_parse_from(
            ["mutants"]
                .iter()
                .chain(REPEATED_OPTIONS.concat().iter())
                .chain(&rerun_options)
                .chain(&trailing),
        )
        .unwrap();
        assert_eq!(rerun_args, expected, "{command}");
    }

    #[test]
    fn powershell_word_doubles_every_character_powershell_takes_as_a_single_quote() {
        assert_eq!(powershell_word("plain-word"), "plain-word");
        assert_eq!(powershell_word("it's"), "'it''s'");
        for quote in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            assert_eq!(
                powershell_word(&format!("a{quote}b $x")),
                format!("'a{quote}{quote}b $x'"),
                "{quote:?}"
            );
        }
    }

    #[test]
    fn known_tests_by_selection_get_returns_tests_of_the_same_selection_only() {
        let mut by_selection = KnownTestsBySelection::default();
        let a_argv = vec!["cargo".to_owned(), "test".to_owned(), "-p=a".to_owned()];
        let b_argv = vec!["cargo".to_owned(), "test".to_owned(), "-p=b".to_owned()];
        by_selection.add_baseline(&options(&[]), a_argv.clone(), "test a::t ... ok\n");
        by_selection.add_baseline(&options(&[]), b_argv.clone(), "test b::t ... ok\n");
        let a = by_selection.get(&a_argv).expect("tests of a are known");
        assert_eq!(a.failed_test("test a::t ... FAILED"), Some("a::t"));
        assert_eq!(a.failed_test("test b::t ... FAILED"), None);
        assert!(
            by_selection
                .get(&["cargo".to_owned(), "test".to_owned()])
                .is_none()
        );
        by_selection.add_baseline(
            &options(&["--stop-tests-on-failure=false"]),
            a_argv.clone(),
            "test a::t ... ok\n",
        );
        assert!(
            by_selection.get(&a_argv).is_none(),
            "no tests are known when tests aren't stopped"
        );
    }

    #[test]
    fn known_tests_is_none_when_tests_should_not_be_stopped() {
        assert!(known_tests(&options(&[]), BASELINE_LOG).is_some());
        for args in [
            ["--stop-tests-on-failure=false"].as_slice(),
            &["--test-tool=nextest"],
            &["--", "--no-fail-fast"],
        ] {
            assert!(
                known_tests(&options(args), BASELINE_LOG).is_none(),
                "{args:?}"
            );
        }
        let terse_log = "running 2 tests\n..\ntest result: ok. 2 passed; 0 failed\n";
        assert!(known_tests(&options(&[]), terse_log).is_none());
    }

    #[test]
    fn known_tests_from_log_has_tests_reported_ok() {
        let known = known();
        let names: HashSet<&str> = known.names.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            HashSet::from([
                "a::passes",
                "a::panics - should panic",
                "src/lib.rs - f (line 3)"
            ])
        );
    }

    #[test]
    fn passed_tests_counts_every_test_reported_ok() {
        assert_eq!(passed_tests(BASELINE_LOG), 3);
        assert_eq!(passed_tests(&BASELINE_LOG.repeat(2)), 6);
    }

    #[test]
    fn failed_test_names_known_test_reported_failed() {
        let known = known();
        assert_eq!(
            known.failed_test("test a::passes ... FAILED"),
            Some("a::passes")
        );
        assert_eq!(
            known.failed_test("test a::panics - should panic ... FAILED"),
            Some("a::panics - should panic")
        );
        assert_eq!(
            known.failed_test("test src/lib.rs - f (line 3) ... FAILED\r"),
            Some("src/lib.rs - f (line 3)")
        );
    }

    #[test]
    fn failed_test_ignores_lines_not_exactly_libtest_failures_of_known_tests() {
        let known = known();
        for line in [
            "test a::passes ... ok",
            "test a::is_ignored ... FAILED",
            "test a::unknown ... FAILED",
            " test a::passes ... FAILED",
            "note: test a::passes ... FAILED",
            "test a::passes ... FAILED and more",
            "test a::passes ... FAILED (time limit exceeded)",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored",
            "FAILED",
            "",
        ] {
            assert_eq!(known.failed_test(line), None, "{line:?}");
        }
    }

    #[test]
    fn failure_watch_reports_known_failure_only_once_its_line_is_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = Utf8PathBuf::try_from(tmp.path().join("log")).unwrap();
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        // Written before the watch starts, so not seen.
        log.write_all(b"test a::passes ... FAILED\n").unwrap();
        let known = known();
        let mut watch = FailureWatch::new(&known, &log_path).unwrap();
        assert_eq!(watch.poll().unwrap(), None);
        log.write_all(b"test a::unknown ... FAILED\ntest a::pass")
            .unwrap();
        assert_eq!(watch.poll().unwrap(), None);
        log.write_all(b"es ... FAILED").unwrap();
        assert_eq!(watch.poll().unwrap(), None, "line is not complete yet");
        log.write_all(b"\n").unwrap();
        assert_eq!(watch.poll().unwrap().as_deref(), Some("a::passes"));
    }
}
