# Fail-fast tests

When cargo-mutants runs the test suite, it only needs to find out if any tests fail, and so it's desirable that the test suite stop on the first failure.

In `cargo test` there are separate fail-fast configurations at two levels: the whole target and the individual test.

By default, `cargo test` will stop running test targets after the first one fails. Do not pass `--no-fail-fast` to `cargo test` under cargo-mutants.

## Stopping tests at the first failure

Within a single test target, the standard Rust test harness (libtest) on stable Rust keeps running every remaining test after one fails. For a mutant that's caught by a fast test, most of the test time can be spent waiting for slow tests whose results no longer matter.

So, by default, cargo-mutants watches the test output as it's written, and as soon as the test harness reports that a test failed, with a line like

    test tests::reads_config ... FAILED

it kills the tests and records the mutant as caught. The test process is recorded as having failed with exit code 101, as `cargo test` would have. The mutant's log ends with a line saying which test failed, and how to rerun just that mutant with the complete output of its tests:

    *** stopped tests: tests::reads_config failed; rerun this mutant with complete output: cargo mutants --re '^src/config\.rs:12:5: replace parse \-> bool with true$' --stop-tests-on-failure=false -o /home/me/src/myproj/mutants.out/rerun

Once libtest reports a failed test, the test binary will exit unsuccessfully and so will `cargo test`, so a mutant whose tests are stopped would have been caught anyway, with one exception: if the mutant makes one test fail and another hang, stopping at the failure records the mutant as caught, whereas without stopping the hanging test would make it time out. Since timeouts give a different [exit code](exit-codes.md) than caught mutants, this can change the exit code of the run.

Tests can print arbitrary text, including from child processes whose output isn't captured by the test harness, so a line is only trusted if it names a test that passed in the [baseline](baseline.md) test run of the unmutated tree, with the same packages tested, and matches libtest's format exactly. A test whose output merely looks like a failure report doesn't stop the tests.

The trade-off is that the log of a stopped mutant doesn't contain the details that libtest prints only after all the tests in the binary have finished: the output of the failing test, such as its panic message, and the list of failures at the end. To see it for one mutant, run the command from its log (see below). To get complete logs for every mutant, turn this off with `--stop-tests-on-failure=false` on the command line or `stop_tests_on_failure = false` in `.cargo/mutants.toml`.

## Rerunning one mutant with complete output

The command in the log of a stopped mutant tests only that mutant, with its tests not stopped early, so its log then has the failing test's panic message and everything else the tests printed. It selects the mutant with [`--re`](filter_mutants.md), using the mutant's full name, with its line and column, escaped as a regular expression and anchored, so it matches no other mutant.

The command repeats the options you gave on the command line, such as `-d`, `-p`, `--features`, `--profile`, `--cargo-arg`, `--test-package`, timeouts, and the arguments after `--`, so that the mutant is found, built, and tested the same way. It leaves out the options that select mutants or shape the run: `--re`, `--exclude-re`, `--file`, `--exclude`, `--in-diff`, `--iterate`, `--shard`, `--sharding`, `--jobs`, `--shuffle`, `--no-shuffle`, `--list`, `--list-files`, `--json`, `--stop-tests-on-failure`, and `--output`. Options given by environment variables, such as `CARGO_MUTANTS_JOBS`, aren't repeated, but apply again if they're still set. Run the command from the directory where you ran cargo-mutants. The quoting is for a Unix shell, or for PowerShell on Windows.

The rerun writes its output to `mutants.out/rerun/mutants.out`, within the run's own output directory, so it doesn't rotate the run's `mutants.out` to `mutants.out.old`. Running it again rotates only the previous rerun's output.

A `--re` on the command line is combined with any `examine_re` in `.cargo/mutants.toml`, so if the config sets `examine_re`, the rerun also tests the mutants that it matches.

The rerun still uses [schemata](schemata.md). For one mutant, the default `--test-selection=auto` usually [finds](schemata.md#choosing-automatically) that collecting coverage wouldn't pay off, so the rerun runs all the tests; but if coverage-based test selection is used, as when `--test-selection=coverage` was given, it runs the selected tests that execute the mutant's code, stopping at the first batch that fails. To see the output of the whole test suite exactly as building and testing that mutant by itself would show it, add `--no-schemata`; or, to keep schemata but run all the tests, add `--test-selection=all`.

Tests aren't stopped early:

- With `--baseline=skip`, because no test names are known.
- When a mutant's tests are of different packages than the baseline tested: for example with `--test-workspace=true`, or when mutants from several packages are tested, since the baseline tests all of those packages at once. (With [schemata](schemata.md), the default, the baseline runs separately for each set of packages, so this doesn't apply.)
- With `--test-tool=nextest`, which has its own fail-fast behavior.
- If `--no-fail-fast` is passed to `cargo test`.
- If libtest's `--format terse` (or `-q`) is passed to the test binaries, because it doesn't print test names.
- On Windows, when cargo-mutants runs `cargo test`, because killing cargo on Windows doesn't kill the test binary it's running.

Doctest names include line numbers, which can change when a mutation adds or removes lines; a doctest that fails at a different line number than it passed at in the baseline doesn't stop the tests early.

## libtest's own `--fail-fast`

Rust nightly releases after 1.92.0-nightly (2025-09-18) accept a `--fail-fast` option to the test harness. (This is distinct from the `--fail-fast` option to `cargo test`.) This causes the test target to stop after the first individual test fails, and to still print the failing test's output.

If you have a sufficiently recent toolchain you can enable this in the [`cargo_test_args`](cargo-args.md):

    cargo mutants -- -- -Zunstable-options --fail-fast

*Note*: There are two `--` separators: the first delimits the arguments from `cargo mutants` to be passed to `cargo test` and the second delimits the arguments from `cargo test` so they are passed to the test target.
