// Copyright 2026 Martin Pool

//! Parse the tests listed by a test binary run with `--list --format terse`.

#![warn(clippy::pedantic)]

/// Arguments appended to each test command to list its tests rather than run them.
pub(crate) const LIST_ARGS: [&str; 3] = ["--list", "--format", "terse"];

/// The tests a command listed, from what it printed, as returned by
/// [`crate::schemata::replay::test_commands_with_output`]. Benchmarks are not
/// included.
pub(crate) fn listed_tests(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.trim_end().strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn listed_tests_keeps_tests_and_not_benchmarks_or_headers() {
        let output = indoc! {"
            tests::a: test
            tests::b: test\r
            benches::fast: bench
               Doc-tests foo
        "};
        assert_eq!(listed_tests(output), ["tests::a", "tests::b"]);
        assert!(listed_tests("").is_empty());
    }
}
