// Copyright 2026 Martin Pool

//! Replay the test commands that `cargo test -vv` ran, without running cargo.
//!
//! Concurrent `cargo test` invocations sharing one target directory wait for each
//! other on cargo's build-directory lock while cargo checks whether anything needs
//! rebuilding (though not while the tests themselves run). To avoid that, the
//! schema baseline runs `cargo test -vv --message-format=json`, which prints each
//! test command with the exact environment cargo gives it, in the order cargo runs
//! them: library unit tests, binaries, integration tests, then doctests via
//! `rustdoc --test`. Each mutant is then tested by running those same commands
//! directly, with the mutant id added to the environment, stopping at the first
//! failure as cargo does.
//!
//! Cargo prints the commands for people rather than programs, so the parse is
//! checked against the JSON artifact messages from the same run: every test
//! executable that cargo built must be run by exactly one parsed command, and
//! nothing else is. If that doesn't hold the commands are not used, and each mutant
//! is tested with `cargo test` instead.

#![warn(clippy::pedantic)]

use std::collections::BTreeSet;

use anyhow::{bail, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use serde_json::Value;
use tracing::debug;

use crate::Result;

/// One command that cargo ran to test the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayCommand {
    /// Environment variables cargo set for this command.
    pub env: Vec<(String, String)>,
    /// The program followed by its arguments.
    pub argv: Vec<String>,
    /// Directory to run in.
    pub cwd: Utf8PathBuf,
    /// The command ran no tests in the baseline (see [`runs_no_tests`]), so it isn't
    /// replayed to run the tests.
    pub idle: bool,
}

/// What cargo prints before each command it runs, with `--verbose`.
const RUNNING: &str = "Running `";

/// How cargo quoted the words of the commands it printed.
///
/// Cargo quotes with the `shell-escape` crate: POSIX shell quoting on Unix, and on
/// Windows `CommandLineToArgvW` quoting unless `MSYSTEM` is set. On Windows each
/// environment variable is also printed as `set NAME=value&& `.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quoting {
    Posix,
    Windows,
}

impl Quoting {
    /// The quoting cargo uses when run from this process, which it inherits the
    /// environment of.
    pub(crate) fn host() -> Quoting {
        if cfg!(windows) && std::env::var_os("MSYSTEM").is_none() {
            Quoting::Windows
        } else {
            Quoting::Posix
        }
    }
}

/// Extract the test commands from the output of `cargo test -vv --message-format=json`.
///
/// Only commands after cargo's first `Finished` line are included, so that build
/// steps are not. Test binaries run in their package directory, taken from
/// `CARGO_MANIFEST_DIR`; `rustdoc` runs in `workspace_root`, as cargo runs it.
/// Commands whose output shows they ran no tests are marked [`ReplayCommand::idle`].
///
/// # Errors
///
/// If the commands don't account for exactly the test executables that cargo
/// reported building, or a doctest command can't be parsed.
pub(crate) fn test_commands(
    cargo_output: &str,
    workspace_root: &Utf8Path,
    quoting: Quoting,
) -> Result<Vec<ReplayCommand>> {
    Ok(
        test_commands_with_output(cargo_output, workspace_root, quoting)?
            .into_iter()
            .map(|(command, output)| ReplayCommand {
                idle: runs_no_tests(output),
                ..command
            })
            .collect(),
    )
}

/// True if `output`, from one test command, shows libtest ran no tests at all.
///
/// Every libtest summary line must report zero passed, failed, and measured tests
/// (ignored and filtered-out tests don't run). The set of tests in a binary, and
/// which of them are ignored or filtered, is fixed when the schema is built and by
/// the arguments, so it is the same for every mutant: a command that ran no tests
/// in the baseline can't observe any mutant, and needn't be replayed.
///
/// Output without a libtest summary, as from a `harness = false` target, is never
/// considered empty.
pub(crate) fn runs_no_tests(output: &str) -> bool {
    let summaries = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("test result: "))
        .collect_vec();
    !summaries.is_empty() && summaries.iter().all(|summary| summary_ran_nothing(summary))
}

/// True for a libtest summary like `ok. 0 passed; 0 failed; 2 ignored; 0 measured; ...`
/// that ran no tests.
fn summary_ran_nothing(summary: &str) -> bool {
    let Some((_status, counts)) = summary.split_once(". ") else {
        return false;
    };
    let count = |label: &str| {
        counts.split(';').find_map(|part| {
            let (n, name) = part.trim().split_once(' ')?;
            (name == label).then(|| n.parse::<u64>().ok()).flatten()
        })
    };
    ["passed", "failed", "measured"]
        .iter()
        .all(|label| count(label) == Some(0))
}

/// Like [`test_commands`], but also return the text printed after each command,
/// up to the next test command: what it printed, and cargo's header for the next one.
///
/// # Errors
///
/// As for [`test_commands`].
pub(crate) fn test_commands_with_output<'out>(
    cargo_output: &'out str,
    workspace_root: &Utf8Path,
    quoting: Quoting,
) -> Result<Vec<(ReplayCommand, &'out str)>> {
    let executables = test_executables(cargo_output);
    let Some(rest) = after_finished_line(cargo_output) else {
        bail!("no Finished line in cargo output");
    };
    let mut commands = Vec::new();
    // Where the output of each command in `commands` starts, and where the last ended.
    let mut output_starts = Vec::new();
    let mut output_ends = Vec::new();
    let mut after_doctests_header = false;
    let mut pos = 0;
    while pos < rest.len() {
        let line_end = rest[pos..].find('\n').map_or(rest.len(), |i| pos + i);
        let untrimmed = &rest[pos..line_end];
        let line = untrimmed.trim();
        let is_doctests_header = line.starts_with("Doc-tests ");
        if line.starts_with(RUNNING) {
            let start = pos + (untrimmed.len() - untrimmed.trim_start().len()) + RUNNING.len();
            let parsed = split_words(&rest[start..], quoting).and_then(|(words, len)| {
                classify(words, workspace_root, &executables, after_doctests_header)
                    .map(|command| (command, len))
            });
            match parsed {
                Some((command, len)) => {
                    if !commands.is_empty() {
                        output_ends.push(pos);
                    }
                    commands.push(command);
                    // Continue after the closing backtick's line.
                    let end = start + len;
                    pos = rest[end..].find('\n').map_or(rest.len(), |i| end + i + 1);
                    output_starts.push(pos);
                    after_doctests_header = false;
                    continue;
                }
                None if after_doctests_header => {
                    bail!("could not parse the doctest command after a Doc-tests line")
                }
                // Probably output from a test that looks like cargo's.
                None => debug!(line, "ignoring unrecognized Running line"),
            }
        } else if after_doctests_header && !line.is_empty() {
            bail!("no doctest command after a Doc-tests line");
        }
        if !line.is_empty() {
            after_doctests_header = is_doctests_header;
        }
        pos = line_end + 1;
    }
    ensure!(
        !after_doctests_header,
        "no doctest command after a Doc-tests line"
    );
    let replayed = commands
        .iter()
        .filter(|c| !is_rustdoc(&c.argv[0]))
        .map(|c| c.argv[0].as_str())
        .collect_vec();
    if let Some(duplicate) = replayed.iter().duplicates().next() {
        bail!("test executable {duplicate} was run more than once");
    }
    let missing = executables
        .iter()
        .filter(|exe| !replayed.contains(&exe.as_str()))
        .collect_vec();
    ensure!(
        missing.is_empty(),
        "no command found for test executables {missing:?}"
    );
    if !commands.is_empty() {
        output_ends.push(rest.len());
    }
    Ok(commands
        .into_iter()
        .zip(output_starts.into_iter().zip(output_ends))
        .map(|(command, (start, end))| (command, &rest[start.min(end)..end]))
        .collect())
}

/// The text after cargo's first `Finished` line, if there is one.
///
/// Test output comes after that line, so the first is cargo's. Cargo before 1.77
/// printed `Finished test [unoptimized + debuginfo]`, and later versions
/// ``Finished `test` profile``.
fn after_finished_line(cargo_output: &str) -> Option<&str> {
    let mut pos = 0;
    for line in cargo_output.split_inclusive('\n') {
        pos += line.len();
        if line.trim_start().starts_with("Finished ") {
            return Some(&cargo_output[pos..]);
        }
    }
    None
}

/// Test executables that cargo built or found fresh, from its JSON artifact messages.
fn test_executables(cargo_output: &str) -> BTreeSet<String> {
    cargo_output
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["reason"] == "compiler-artifact" && value["profile"]["test"] == true)
        .filter_map(|value| value["executable"].as_str().map(str::to_owned))
        .collect()
}

/// True if `program` is `rustdoc`, which runs doctests.
pub(crate) fn is_rustdoc(program: &str) -> bool {
    Utf8Path::new(program)
        .file_name()
        .is_some_and(|name| name.starts_with("rustdoc"))
}

/// Make a command from the words cargo printed, if it runs a known test executable,
/// or is `rustdoc` following a `Doc-tests` line.
fn classify(
    words: Vec<String>,
    workspace_root: &Utf8Path,
    executables: &BTreeSet<String>,
    doctests: bool,
) -> Option<ReplayCommand> {
    let (env, rest) = split_env(words);
    // Cargo prints the program without quoting, so a path containing spaces is split
    // into several words: join them until they name a known executable.
    let (program, arguments) = (1..=rest.len())
        .find_map(|n| {
            let program = rest[..n].join(" ");
            executables
                .contains(&program)
                .then(|| (program, &rest[n..]))
        })
        .or_else(|| {
            let (program, arguments) = rest.split_first()?;
            (doctests && is_rustdoc(program)).then(|| (program.clone(), arguments))
        })?;
    let cwd = if is_rustdoc(&program) {
        workspace_root.to_owned()
    } else {
        env.iter()
            .find(|(key, _)| key == "CARGO_MANIFEST_DIR")
            .map(|(_, dir)| Utf8PathBuf::from(dir))?
    };
    let argv = std::iter::once(program)
        .chain(arguments.iter().cloned())
        .collect();
    Some(ReplayCommand {
        env,
        argv,
        cwd,
        idle: false,
    })
}

/// Split leading environment assignments from the program and arguments.
///
/// Assignments are `NAME=value`, or on Windows `set` followed by `NAME=value&&`.
fn split_env(mut words: Vec<String>) -> (Vec<(String, String)>, Vec<String>) {
    let mut env = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if words[i] == "set"
            && let Some(assignment) = words.get(i + 1).and_then(|w| w.strip_suffix("&&"))
            && is_assignment(assignment)
        {
            env.push(split_assignment(assignment));
            i += 2;
        } else if is_assignment(&words[i]) {
            env.push(split_assignment(&words[i]));
            i += 1;
        } else {
            break;
        }
    }
    (env, words.split_off(i))
}

fn split_assignment(word: &str) -> (String, String) {
    let (key, value) = word.split_once('=').expect("assignment has =");
    (key.to_owned(), value.to_owned())
}

/// True for a word like `NAME=value`.
///
/// Unlike shell variables, names may contain `-`, as in `CARGO_BIN_EXE_<name>` for
/// binaries with hyphenated names. A program path contains `/` or `\`, so isn't
/// confused with an assignment.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// Split a command quoted as cargo prints it, up to the unquoted backtick that ends it.
///
/// Returns the words and the byte offset of the closing backtick, or `None` if there
/// is none, or if a line ends outside quotes before it: cargo quotes every value
/// that contains a newline, so the text isn't a command cargo printed.
fn split_words(text: &str, quoting: Quoting) -> Option<(Vec<String>, usize)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '`' => {
                if in_word {
                    words.push(word);
                }
                return Some((words, i));
            }
            '\n' => return None,
            ' ' | '\t' | '\r' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            _ => {
                in_word = true;
                match quoting {
                    Quoting::Posix => posix_char(c, &mut chars, &mut word)?,
                    Quoting::Windows => windows_char(c, &mut chars, &mut word)?,
                }
            }
        }
    }
    None
}

type Chars<'a> = std::iter::Peekable<std::str::CharIndices<'a>>;

/// Add a character outside quotes, and any quoted string it starts, to `word`.
fn posix_char(c: char, chars: &mut Chars, word: &mut String) -> Option<()> {
    match c {
        '\'' => loop {
            match chars.next()?.1 {
                '\'' => break,
                c => word.push(c),
            }
        },
        '"' => loop {
            match chars.next()?.1 {
                '"' => break,
                '\\' => word.push(chars.next()?.1),
                c => word.push(c),
            }
        },
        '\\' => word.push(chars.next()?.1),
        c => word.push(c),
    }
    Some(())
}

/// Add a character outside quotes, and any quoted string it starts, to `word`,
/// with the backslash rules of `CommandLineToArgvW`.
fn windows_char(c: char, chars: &mut Chars, word: &mut String) -> Option<()> {
    let mut quoted = false;
    let mut c = c;
    loop {
        match c {
            '\\' => {
                let mut backslashes = 1;
                while chars.next_if(|&(_, c)| c == '\\').is_some() {
                    backslashes += 1;
                }
                if chars.next_if(|&(_, c)| c == '"').is_some() {
                    word.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        word.push('"');
                    } else {
                        quoted = !quoted;
                    }
                } else {
                    word.extend(std::iter::repeat_n('\\', backslashes));
                }
            }
            '"' => quoted = !quoted,
            c => word.push(c),
        }
        if !quoted {
            return Some(());
        }
        c = chars.next()?.1;
    }
}

#[cfg(test)]
mod test {
    use indoc::{formatdoc, indoc};
    use pretty_assertions::assert_eq;

    use super::*;

    /// A JSON artifact message for a test executable, as cargo prints it.
    fn artifact(executable: &str) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "profile": {"test": true},
            "executable": executable,
            "fresh": true,
        })
        .to_string()
    }

    fn words(text: &str, quoting: Quoting) -> Vec<String> {
        let text = format!("{text}`");
        let (words, end) = split_words(&text, quoting).expect("command ends with a backtick");
        assert_eq!(end, text.len() - 1);
        words
    }

    #[test]
    fn split_words_handles_posix_quoting() {
        assert_eq!(
            words(
                r"A=1 B='two words' C='' D='it'\''s' E='wow'\!'' /bin/prog --flag 'x y'",
                Quoting::Posix
            ),
            [
                "A=1",
                "B=two words",
                "C=",
                "D=it's",
                "E=wow!",
                "/bin/prog",
                "--flag",
                "x y"
            ]
        );
        assert_eq!(words(r#"a\ b "c d""#, Quoting::Posix), ["a b", "c d"]);
        assert_eq!(split_words("a 'b`", Quoting::Posix), None);
    }

    #[test]
    fn split_words_handles_windows_quoting() {
        assert_eq!(
            words(
                r#"set A="two words"&& set P=C:\a\b&& C:\t\deps\x.exe "--features=\"default\"" "\path\my documents\\" """#,
                Quoting::Windows
            ),
            [
                "set",
                "A=two words&&",
                "set",
                r"P=C:\a\b&&",
                r"C:\t\deps\x.exe",
                r#"--features="default""#,
                r"\path\my documents\",
                ""
            ]
        );
    }

    #[test]
    fn split_words_stops_at_line_end_outside_quotes() {
        assert_eq!(split_words("a b\nc`", Quoting::Posix), None);
        assert_eq!(
            split_words("a 'b\nc'`", Quoting::Posix),
            Some((vec!["a".to_owned(), "b\nc".to_owned()], 7))
        );
    }

    #[test]
    fn test_commands_parses_commands_after_finished_line() {
        let output = formatdoc! {r"
            *** header
               Compiling foo v0.1.0 (/ws/foo)
                 Running `rustc --crate-name foo src/lib.rs`
            {artifact}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO=/bin/cargo CARGO_MANIFEST_DIR=/ws/foo CARGO_PKG_AUTHORS='' CARGO_PKG_DESCRIPTION='a b' /ws/target/debug/deps/foo-123 --test-threads=1`
            running 1 test
            test t ... ok
               Doc-tests foo
                 Running `CARGO=/bin/cargo CARGO_MANIFEST_DIR=/ws/foo rustdoc --test foo/src/lib.rs --test-run-directory /ws/foo`
            running 0 tests
        ", artifact = artifact("/ws/target/debug/deps/foo-123")};
        let commands = test_commands(&output, "/ws".into(), Quoting::Posix).unwrap();
        assert_eq!(
            commands,
            [
                ReplayCommand {
                    env: vec![
                        ("CARGO".to_owned(), "/bin/cargo".to_owned()),
                        ("CARGO_MANIFEST_DIR".to_owned(), "/ws/foo".to_owned()),
                        ("CARGO_PKG_AUTHORS".to_owned(), String::new()),
                        ("CARGO_PKG_DESCRIPTION".to_owned(), "a b".to_owned()),
                    ],
                    argv: vec![
                        "/ws/target/debug/deps/foo-123".to_owned(),
                        "--test-threads=1".to_owned()
                    ],
                    cwd: "/ws/foo".into(),
                    idle: false,
                },
                ReplayCommand {
                    env: vec![
                        ("CARGO".to_owned(), "/bin/cargo".to_owned()),
                        ("CARGO_MANIFEST_DIR".to_owned(), "/ws/foo".to_owned()),
                    ],
                    argv: vec![
                        "rustdoc".to_owned(),
                        "--test".to_owned(),
                        "foo/src/lib.rs".to_owned(),
                        "--test-run-directory".to_owned(),
                        "/ws/foo".to_owned(),
                    ],
                    cwd: "/ws".into(),
                    idle: false,
                },
            ]
        );
    }

    #[test]
    fn test_commands_parses_env_values_spanning_lines() {
        // Cargo prints a newline in a value, like a multi-line package description,
        // literally. This used to lose the command, so no tests were replayed.
        let output = formatdoc! {"
            {artifact}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_MANIFEST_DIR=/ws/foo CARGO_PKG_DESCRIPTION='first
            second `line`' /ws/target/debug/deps/foo-123`
            running 1 test
               Doc-tests foo
                 Running `CARGO_MANIFEST_DIR=/ws/foo CARGO_PKG_DESCRIPTION='first
            second `line`' rustdoc --test src/lib.rs`
        ", artifact = artifact("/ws/target/debug/deps/foo-123")};
        let commands = test_commands(&output, "/ws".into(), Quoting::Posix).unwrap();
        assert_eq!(
            commands.iter().map(|c| c.argv[0].as_str()).collect_vec(),
            ["/ws/target/debug/deps/foo-123", "rustdoc"]
        );
        assert_eq!(
            commands[0].env[1].1, "first\nsecond `line`",
            "value keeps its newline"
        );
    }

    #[test]
    fn test_commands_ignores_running_lines_printed_by_tests() {
        // A test's child process prints lines like cargo's, which are not test commands.
        let output = formatdoc! {"
            {artifact}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/foo-123`
            running 1 test
                 Running `rustc --crate-name child src/main.rs`
                 Running `unterminated
            test t ... ok
        ", artifact = artifact("/ws/target/debug/deps/foo-123")};
        let commands = test_commands(&output, "/ws".into(), Quoting::Posix).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn runs_no_tests_is_true_when_every_libtest_summary_ran_nothing() {
        let empty = indoc! {r"
            running 0 tests

            test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
        "};
        assert!(runs_no_tests(empty));
        let only_ignored = indoc! {r"
            running 2 tests
            test live_a ... ignored
            test live_b ... ignored

            test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
        "};
        assert!(runs_no_tests(only_ignored));
        // rustdoc prints one summary for merged doctests and one for the rest.
        assert!(runs_no_tests(&format!("{empty}\n{empty}")));
    }

    #[test]
    fn runs_no_tests_is_false_when_any_test_ran_or_output_is_not_libtest() {
        let ran = indoc! {r"
            running 816 tests
            test result: ok. 814 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 3.15s
        "};
        assert!(!runs_no_tests(ran));
        let measured =
            "test result: ok. 0 passed; 0 failed; 0 ignored; 1 measured; 0 filtered out\n";
        assert!(!runs_no_tests(measured));
        let empty = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert!(
            !runs_no_tests(&format!("{empty}{ran}")),
            "one summary ran tests"
        );
        // A `harness = false` target prints whatever it likes, so it is never idle.
        assert!(!runs_no_tests("custom harness: all good\n"));
        assert!(!runs_no_tests(""));
        assert!(!runs_no_tests("test result: ok. garbled\n"));
    }

    #[test]
    fn test_commands_marks_commands_that_ran_no_tests_idle() {
        let output = formatdoc! {"
            {a}
            {b}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/foo-123`
            running 1 test
            test t ... ok

            test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/tool-456`
            running 1 test
            test needs_service ... ignored

            test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

               Doc-tests foo
                 Running `CARGO_MANIFEST_DIR=/ws/foo rustdoc --test src/lib.rs`
            running 0 tests

            test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
        ", a = artifact("/ws/target/debug/deps/foo-123"), b = artifact("/ws/target/debug/deps/tool-456")};
        let idle = test_commands(&output, "/ws".into(), Quoting::Posix)
            .unwrap()
            .into_iter()
            .map(|command| (command.argv[0].clone(), command.idle))
            .collect_vec();
        assert_eq!(
            idle,
            [
                ("/ws/target/debug/deps/foo-123".to_owned(), false),
                ("/ws/target/debug/deps/tool-456".to_owned(), true),
                ("rustdoc".to_owned(), true),
            ]
        );
    }

    #[test]
    fn test_commands_with_output_gives_what_each_command_printed() {
        // A lookalike Running line printed by a test is part of that test's output.
        let output = formatdoc! {"
            {a}
            {b}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/foo-123 --list`
            tests::a: test
                 Running `rustc --crate-name child src/main.rs`
            tests::b: test
                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/cli-456 --list`
               Doc-tests foo
                 Running `CARGO_MANIFEST_DIR=/ws/foo rustdoc --test src/lib.rs`
            src/lib.rs - f (line 1): test
        ", a = artifact("/ws/target/debug/deps/foo-123"), b = artifact("/ws/target/debug/deps/cli-456")};
        let outputs = test_commands_with_output(&output, "/ws".into(), Quoting::Posix)
            .unwrap()
            .into_iter()
            .map(|(command, output)| (command.argv[0].clone(), output.to_owned()))
            .collect_vec();
        assert_eq!(
            outputs,
            [
                (
                    "/ws/target/debug/deps/foo-123".to_owned(),
                    "tests::a: test\n     Running `rustc --crate-name child src/main.rs`\ntests::b: test\n"
                        .to_owned()
                ),
                (
                    "/ws/target/debug/deps/cli-456".to_owned(),
                    "   Doc-tests foo\n".to_owned()
                ),
                (
                    "rustdoc".to_owned(),
                    "src/lib.rs - f (line 1): test\n".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn test_commands_fails_if_a_test_executable_is_not_run() {
        let output = formatdoc! {"
            {a}
            {b}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/foo-123`
        ", a = artifact("/ws/target/debug/deps/foo-123"), b = artifact("/ws/target/debug/deps/cli-456")};
        let err = test_commands(&output, "/ws".into(), Quoting::Posix).unwrap_err();
        assert!(err.to_string().contains("cli-456"), "{err}");
    }

    #[test]
    fn test_commands_fails_if_a_doctest_command_is_not_parsed() {
        let output = indoc! {"
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
               Doc-tests foo
                 Running `CARGO_MANIFEST_DIR=/ws/foo rustdoc --test 'src/lib.rs`
        "};
        assert!(test_commands(output, "/ws".into(), Quoting::Posix).is_err());
    }

    #[test]
    fn test_commands_parses_windows_set_syntax_and_program_paths_with_spaces() {
        let exe = r"C:\Users\A B\target\debug\deps\foo-123.exe";
        let output = formatdoc! {r#"
            {artifact}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `set CARGO_MANIFEST_DIR="C:\Users\A B\foo"&& set PATH="C:\Users\A B\target\debug\deps;C:\Windows"&& {exe} --quiet`
        "#, artifact = artifact(exe)};
        let commands = test_commands(&output, r"C:\Users\A B".into(), Quoting::Windows).unwrap();
        assert_eq!(
            commands,
            [ReplayCommand {
                env: vec![
                    (
                        "CARGO_MANIFEST_DIR".to_owned(),
                        r"C:\Users\A B\foo".to_owned()
                    ),
                    (
                        "PATH".to_owned(),
                        r"C:\Users\A B\target\debug\deps;C:\Windows".to_owned()
                    ),
                ],
                argv: vec![exe.to_owned(), "--quiet".to_owned()],
                cwd: r"C:\Users\A B\foo".into(),
                idle: false,
            }]
        );
    }

    #[test]
    fn test_commands_accepts_hyphens_in_bin_exe_variable_names() {
        // Binary names can contain hyphens, and so can CARGO_BIN_EXE_<name>.
        let output = formatdoc! {"
            {artifact}
                Finished `test` profile [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_BIN_EXE_gen-fixtures=/ws/target/debug/gen-fixtures CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/cli-123`
        ", artifact = artifact("/ws/target/debug/deps/cli-123")};
        let commands = test_commands(&output, "/ws".into(), Quoting::Posix).unwrap();
        assert_eq!(commands[0].argv, ["/ws/target/debug/deps/cli-123"]);
        assert_eq!(
            commands[0].env[0],
            (
                "CARGO_BIN_EXE_gen-fixtures".to_owned(),
                "/ws/target/debug/gen-fixtures".to_owned()
            )
        );
    }

    #[test]
    fn test_commands_accepts_finished_line_of_cargo_before_1_77() {
        let output = formatdoc! {"
            {artifact}
                Finished test [unoptimized + debuginfo] target(s) in 0.84s
                 Running `CARGO_MANIFEST_DIR=/ws/foo /ws/target/debug/deps/foo-123`
        ", artifact = artifact("/ws/target/debug/deps/foo-123")};
        assert_eq!(
            test_commands(&output, "/ws".into(), Quoting::Posix)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_commands_fails_without_finished_line() {
        assert!(test_commands("error: could not compile\n", "/ws".into(), Quoting::Posix).is_err());
    }
}
