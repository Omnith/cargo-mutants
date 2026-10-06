# Mutant schemata

By default, cargo-mutants builds the tree once for all the mutants it can, rather
than once per mutant. This is called *mutant schemata*. Each mutant then costs
only a run of the tests, and with [coverage-based test
selection](#coverage-based-test-selection), usually only of the few tests that
execute the mutated code.

This page calls building and testing each mutant separately *the classic way*.
Mutants that schemata can't handle are still tested the classic way, in the same
run, so every mutant gets an outcome, meant to be the one it would get without
schemata; the [limitations](#limitations) list the known exceptions.

On one crate with 2,074 mutants, a full run took 9 minutes, where the classic way
was estimated at about 5.5 hours. On a sample of 104 of its mutants tested both
ways, every outcome matched.

## How it works

cargo-mutants rewrites the mutated source files into a *schema* that contains all
the mutants it can embed, each guarded by a check of a mutant id read from the
`CARGO_MUTANTS_SCHEMATA_ID` environment variable when the tests run. The schema
is built once, and each mutant is tested by running the same tests with a
different id. Because the id is only read at runtime, changing it doesn't make
Cargo rebuild anything. With no mutant selected, the schema behaves like the
original code.

The steps are:

1. Decide which mutants can be embedded. The others *fall back* to being tested
   the classic way (see [below](#when-it-falls-back)).
2. Build the schema with `cargo test --no-run`. If it doesn't compile, drop the
   mutants that the errors point to, and repeat.
3. Run the tests with no mutant selected. This is the [baseline](baseline.md):
   they must pass, as without schemata. The schema's build is the baseline's
   build phase, and `mutants.out/log/baseline.log` shows the build commands and
   everything the tests printed.
4. Choose how many mutants to test at once.
5. Decide whether to use [coverage-based test
   selection](#coverage-based-test-selection), and if so, build a copy of the
   unmutated tree with coverage instrumentation and record which tests execute which
   functions.
6. Test each embedded mutant.
7. Restore the original source and test the fallback mutants the classic way.

### How the tests are run

Concurrent `cargo test` commands in the same directory wait for each other on
Cargo's locks while Cargo checks that everything is up to date, although not
while the tests themselves run. To avoid this, the baseline `cargo test` is run
with `-vv`, which prints each test command Cargo runs, with the environment it
sets. For each mutant, cargo-mutants runs those same commands directly, in the
same order (unit tests, binaries, integration tests, then doctests with
`rustdoc --test`), adding the mutant id to the environment, and stopping at the
first failure as Cargo does. As without schemata, a test binary is also
[stopped at its first failed test](fail-fast.md).

Cargo prints these commands for people to read, so cargo-mutants checks what it
reads against the test executables that Cargo reports building (with
`--message-format=json`): each must be run by exactly one command, and lines
printed by the tests themselves are ignored. It understands Cargo's quoting on
Unix and on Windows, values that span several lines (such as a multi-line
package description), and program paths that contain spaces.

Test commands that ran no tests in the baseline, such as the unit tests of a
binary that has none, doctests of a crate that has none, or a test target whose
tests are all `#[ignore]`d, are not run again: the tests a command runs are fixed
when the schema is built, so it can't notice any mutant. A command is only
skipped if its output is libtest's and every summary reports no tests passed,
failed, or measured, so custom test harnesses (`harness = false`) always run.
`schemata.json` reports how many commands were skipped as
`idle_commands_skipped`. (With coverage-based test selection, selected tests
always come from commands that ran tests.)

If the commands can't be captured, or replaying them with no mutant selected
fails, cargo-mutants runs `cargo test` for each mutant instead.

### How many mutants are tested at once

With `--jobs`, that many embedded mutants are tested at once, all sharing the
schema's build directory.

Without `--jobs`, cargo-mutants chooses by measuring: it runs the tests with no
mutant selected alone, then as 2, 4, 8, ... copies at once, and stops doubling
when throughput (test runs finished per second) rises by less than 15%, or at
half the number of CPUs. It uses the last number that gained. If the tests fail
when several copies run at once, it tests one mutant at a time, as with
`--jobs`. The timings are in `schemata.json` as `jobs_probe`, and the number used
as `test_jobs`. Because mutants are tested concurrently, the order in which they
are reported can vary from run to run; use `-j1` for a fixed order. With
`--shuffle`, mutants are tested in a random order, as without schemata.

This measures rather than using the number of CPUs because a test suite often
doesn't use them all, and the limit is sometimes elsewhere. For example, one
suite used about 1.3 CPUs, and throughput stopped rising at 2 copies with 6 CPUs
idle: every test run started freshly written scripts, and macOS scanned each of
them in a single system service (`XprotectService`) before running it. Setting
`RUST_TEST_THREADS` to share the CPUs between copies made that suite 3-6%
slower, so cargo-mutants doesn't change it.

The limit of half the CPUs is a judgment call, not a measured optimum: the
probe runs the unmutated tests, but some mutants make a test spin until it
times out, using a whole CPU each, and the headroom keeps several of those from
starving the other tests into spurious timeouts. It made no difference on the
suite measured, where 2 copies were best.

Fallback mutants are tested by `--jobs` workers, one by default.

### Coverage-based test selection

`--test-selection=coverage` runs only the tests that execute each mutant's code,
rather than every test. By default, `--test-selection=auto` does this when
collecting coverage is expected to take less time than it saves (see [choosing
automatically](#choosing-automatically)). It needs `llvm-profdata` and `llvm-cov`
matching the LLVM version of the rustc that builds the tree:

```sh
rustup component add llvm-tools
```

Run this in the tree, so that rustup installs the component for the toolchain the
tree uses, including one chosen by a `rust-toolchain.toml` file. cargo-mutants
looks for the tools in that toolchain's sysroot (`rustc --print sysroot`, run in
the tree). Setting `LLVM_PROFDATA` and `LLVM_COV` to their paths overrides this.

Once the schema is built and its baseline has passed, cargo-mutants copies the
unmutated tree, seeding the copy's `target` directory from the schema's build
directory (unless `--seed-target=false`), and builds it with only the workspace's
crates instrumented for coverage. Dependencies are not instrumented, so they are
reused, and the schema's own build is left as it is. Then it runs each test by itself, in parallel, and
records which functions it executes, including in programs the test runs, such as
`CARGO_BIN_EXE_*` binaries. Then each embedded mutant is tested in one of three ways:

- If some tests execute the function containing the mutant, only those tests run,
  fastest first, in batches of 1, 2, 4, ... tests, stopping at the first failure. If
  they all pass, all the tests run to confirm that the mutant is missed, so a missed
  mutant is one that no test catches, as without test selection.
- If no test executes the mutant's code, cargo-mutants checks whether that code
  ran at all in the baseline, as the schema records (see
  [below](#mutants-reported-missed-without-running-tests)). If it did, coverage
  missed it, for example in a program that a test killed, which writes no coverage,
  so all the tests run, as for a mutant whose selected tests pass. If it never ran,
  it doesn't run with the mutant either, so the mutant is reported missed without
  running any tests. Missed mutants that no test executes are also listed in
  `mutants.out/uncovered.txt`.
- Otherwise, all the tests run. This happens when coverage can't show that the code
  is not executed: when the mutated code isn't in an instrumented function, when no
  code in the file ran in any test, when the only tests that execute it fail when run
  by themselves, or, for code no test executes, when the package has doctests (which
  aren't instrumented), when some test's profile is missing or incomplete because it
  timed out or failed when run by itself, or when tests executed functions that
  aren't in the coverage mapping.

A test binary that lists no tests, which might have a custom harness, is run as a
whole, as if it were one test. Tests that fail when run by themselves are never
selected. A mutant reported missed this way, including one that no test executes,
is still tested the classic way when a missed outcome with the schema can't be
trusted, as described below. Mutants that fall back to being built individually run
all the tests.

#### Mutants reported missed without running tests

Coverage can miss code that ran: a process that is killed, as a test might kill a
server it started, writes no coverage profile. So the schema also records, in every
process that runs with no mutant selected, the first time each place with mutants
runs, writing it to a file straight away, before that code runs, so that it survives
the process being killed. A mutant is reported missed without running tests only if
no test executes its code according to coverage and its code never ran in the
baseline, or in the runs that choose how many mutants to test at once.

That is the outcome all the tests would give, provided that:

- The tests run the same code every time, rather than depending on timing,
  randomness, or the order of a `HashMap`. Otherwise code that didn't run in the
  baseline might run in another run.
- Every process that runs the tree's code during the tests sees the
  `CARGO_MUTANTS_SCHEMATA_ID` environment variable and can write to the schema's
  marker directory, a temporary directory that cargo-mutants creates, as normal
  test processes and the programs they start do. cargo-mutants checks that some test
  process recorded seeing the variable; if none did, every mutant is tested the
  classic way (see [below](#the-whole-run)). But if a test runs the tree's code
  somewhere else, such as in a container, set `CARGO_MUTANTS_COVERAGE_CONFIRM=all`
  to run all the tests for every mutant that no test executes.

Mutants whose code a test might read as text are still tested again the classic way
if they're missed, as described below.

#### Choosing automatically

Collecting coverage is a fixed cost, which only pays off if there are enough
mutants: for a handful of mutants, for example from a small
[`--in-diff`](in-diff.md), running all the tests for each of them can be faster.
So by default, with `--test-selection=auto`, cargo-mutants decides once the
schema's baseline has passed and the number of mutants to test at once is chosen,
from what it measured in this run:

- Running all the tests for the embedded mutants would take the number of embedded
  mutants times one run of the tests, as long as the run that chose how many
  mutants to test at once took with that many at once, divided by that many. (With
  tests of several packages, this is shared evenly among them.)
- Coverage is expected to save 55% of that. Missed mutants still run all the tests,
  and caught mutants stop at the first failed test even without coverage. (On one
  crate, it saved 54% for 9 mutants and 58% for 91.)
- Collecting coverage costs a rebuild of the workspace's crates with
  instrumentation, and running each test alone. The rebuild is estimated from the
  schema's build: its last build if it was built again after dropping mutants,
  which rebuilt only the workspace's crates, or else the whole build, which also
  built the dependencies, and so overestimates it. Building with instrumentation and
  listing the tests is taken to be 1.4 times as long. Running the tests alone is
  estimated at 0.1 second for each test that passed in the baseline, plus one run
  of the tests, shared among the CPUs.

Coverage is collected only if the expected saving is larger than the cost. For
example, on a crate whose 818 tests take about 4 seconds, 10 mutants would take
about 30 seconds with all the tests, which coverage would cut by about 16 seconds,
but collecting it took 36 seconds, so it isn't collected; for 104 mutants it is,
and cuts the run from about 5 minutes to 3.

The decision is not printed. `mutants.out/schemata.json` records it as
`coverage_decision`, with `collect` (whether coverage was collected), the measured
inputs, and the estimates, such as `all_tests_seconds`, `saved_seconds`, and
`collect_seconds`; `mutants.out/debug.log` has the same as a
`schemata.coverage.decision` event.

The estimate doesn't include copying the tree and its `target` directory, which
is fast on filesystems that support reflinks, such as APFS and Btrfs, but a full
copy elsewhere.

To always collect coverage, pass `--test-selection=coverage`, or set
`test_selection = "coverage"` in `.cargo/mutants.toml`; to never collect it, use
`all`.

`mutants.out/schemata.json` has a `test_selection` section with the time to collect
coverage, the number of mutants tested each way and why all tests ran, the number of
tests selected, the number of confirmations and how many of them changed the
outcome, and `uncovered_ran_in_baseline`, the number of mutants that no test
executes according to coverage but whose code ran in the baseline. Each entry in
`mutant_tests` has a `selection` with the same details for that mutant, including
`ran_in_baseline` for those no test executes.

### Mutants that can't compile

A mutant is *proven unviable* if it replaces the `&&` of a let chain, or if every
compile error blamed on it is in its own replacement and is one whose cause can't
be the schema: an unresolved name, a missing method, or an unsatisfied trait bound
or operator, such as `Default::default()` for a type without `Default`. Such a
mutant fails to build the classic way too, so it's recorded as unviable without
building it again, once the baseline passes. On one crate, all 458 mutants
recorded this way were also unviable when built the classic way.

Other mutants that the schema's compile errors point to are tested the classic
way.

## When it falls back

### The whole run

Schemata are not used, and every mutant is built and tested the classic way,
when an option they don't support is in effect: `--test-tool=nextest` (or
`test_tool = "nextest"` in the config), `--in-place`, `--check`, or
`--baseline=skip`. cargo-mutants then says which, for example:

```text
 INFO Not using schemata, since they don't support --test-tool=nextest
```

If `--schemata` is given on the command line together with one of these options,
that's an error instead.

Every mutant is also tested the classic way if none can be embedded, or if the
tests fail with the schema and no mutant selected but pass on the unmutated tree,
for example because a test reads its own source from a path built at runtime. If
the tests fail on the unmutated tree too, cargo-mutants stops, as it does without
schemata, and shows the output of the failing tests.

If the tests fail with the schema, but some files are named by string literals, so
that tests might read them (see [below](#mutants-tested-again-the-classic-way)),
perhaps that's why: a test might compare a generated file with the output of the
code generator, for example. Then cargo-mutants first leaves those files out of the
schema, so that tests read their original text, rebuilds the schema without them,
and runs the tests with no mutant selected once more. If they pass, the other
mutants are tested with the schema, and the mutants in those files are tested the
classic way afterwards, with the reason `source_read_by_tests`. A crate root can't
be left out on its own, since it carries code that every embedded mutant of the
crate needs, so if tests read a crate root, every mutant of its package is tested
the classic way, with the reason `crate_root_read_by_tests`. cargo-mutants says
which files it left out:

```text
 INFO Tests fail with the schema, perhaps because they read src/generated/metadata.rs: testing the 14 mutants in them the classic way
```

If the tests still fail, or no mutant is left embedded, every mutant is tested the
classic way, as above. The rebuild and the second run are recorded as
`baseline_retry` in `schemata.json` (see [Troubleshooting](#troubleshooting)).

Every mutant is also tested the classic way if no test process records that it ran
the tree's code with the mutant id in its environment. Each process that runs the
schema's code with no mutant selected records this in a temporary directory, so this
happens when the tests don't see the variable or can't write there, for example
when a target runner runs them in a sandbox or on another machine; then every
mutant would look missed. It also happens if the tests never run any code that has
mutants. cargo-mutants warns:

```text
 WARN No test process recorded running the tree's code with the mutant id, perhaps because the tests run in a sandbox; testing all mutants the classic way
```

These mutants are listed as fallback mutants with the reason
`markers_not_recorded`.

### Test selection

If `--test-selection` is `auto`, the default, or `coverage` from the config file,
but `llvm-profdata` and `llvm-cov` aren't found for the tree's toolchain, every
embedded mutant runs all the tests, and cargo-mutants says so once:

```text
 INFO Running all tests for each mutant: coverage-based test selection needs llvm-tools for this tree's toolchain (rustup component add llvm-tools)
```

If `--test-selection=coverage` is given on the command line, missing tools are an
error instead. If coverage can't be collected for some other reason, cargo-mutants
warns and runs all the tests for every mutant.

### Individual mutants

Mutants that replace function bodies, binary and unary operators, match arms,
and match guards are embedded. Others *fall back* to being tested the classic
way, one build per mutant, after the embedded mutants are tested. Mutants fall
back if:

- they delete struct fields;
- they are in a const context (a `const` or `static` item, a `const fn`, an array
  length, and so on), where the id can't be read;
- they replace the value of a function that returns `impl Trait`: each
  replacement has its own type, but the function can return only one;
- they are in a proc-macro crate, which runs at compile time;
- they are in a workspace package that a build script or a proc-macro crate in
  the workspace depends on, directly or through other workspace packages, since
  its code can also run at compile time;
- the schema doesn't compile with them and they aren't proven unviable (see
  above). Attribute macros that give the tokens of a function body a different
  span make errors point elsewhere, so all the mutants in that file fall back;
- they were missed, but their outcome might be different the classic way,
  because tests might read their file or some tests ran the tree's code without
  the mutant id (see below);
- the tests failed with the schema, and tests might read their file, or a crate
  root of their package (see [above](#the-whole-run)).

(Mutants that replace the `&&` of a let chain never compile, and are recorded as
unviable.)

The baseline builds and runs the tests of every mutant, including those that
fall back, so the timeouts allow for all of them.

Mutants are tested with the same meaning as the classic textual replacement, even
where the new operator makes Rust group the operands differently: `a - b * c`
with `*` replaced by `+` means `(a - b) + c`, so that mutant's alternative is
the whole expression `a - b + c`, rather than `b + c` in place of `b * c`.

Fallback mutants are tested the classic way, starting in the schema's build
directory after the original source is restored. With `--jobs`, the other build
directories are [seeded](build-dirs.md#seeding-build-directories-from-the-baseline)
from its `target/` directory, so they don't build the dependencies again.

### Mutants tested again the classic way

All the mutants share one build, and their tests run in one directory, so
cargo-mutants checks for some ways that the results could differ from building
each mutant separately:

- The schema records when each mutant's code actually runs. If the tests fail
  for a mutant whose code never ran, the failure can't have been caused by it,
  for example a flaky test or tests of other mutants writing the same files, so
  its tests are run again with nothing else running, and that result is used.
- The tests are first run several times at once with no mutant selected: with
  `--jobs`, that many times; otherwise, each number of copies probed to choose
  how many mutants to test at once. If any fail, the tests interfere with each
  other in the shared directory, so mutants are tested one at a time.
- The schema records when its code runs in a process without the mutant id in
  its environment: a test that runs a binary of the tree after clearing its
  environment (with `env_clear()`, or by removing variables starting with
  `CARGO_MUTANTS_`), or code that runs at build time. That code is not mutated,
  so if this happens by the end of the baseline, mutants that are missed are
  tested again the classic way.
- Tests might read source files as text, for example an architecture test that
  checks the `use` lines of each module. cargo-mutants looks for string literals
  in the package that name a mutated file or a crate root, like
  `include_str!("shapes.rs")` or `read_to_string("src/lib.rs")`, relative to the
  file containing the literal, to the package, or to the workspace. Those files
  are embedded like any other: whichever mutant is selected, tests read the same
  schema text, and the baseline checks that they pass with it, so a mutant
  caught with the schema was caught by what its code does. But the classic way,
  tests read the mutated text, which might make them fail, so mutants in those
  files that are missed with the schema are tested again the classic way.

Mutants tested again the classic way are listed as fallback mutants with the
reason `environment_cleared` or `source_read_by_tests_missed_retest`.

## Limitations

- Only the default `cargo test` tool is supported, not nextest; and `--in-place`,
  `--check`, and `--baseline=skip` can't be used. With any of them, every mutant
  is tested the classic way.
- All jobs share one build directory, so when several mutants are tested at
  once, tests that write to fixed paths in the source tree or
  `CARGO_TARGET_TMPDIR` may interfere with each other; use `--jobs=1` to avoid
  it. The checks above catch interference that shows up when the tests run
  without mutants, or for mutants whose code doesn't run, but not interference
  that only happens with some mutants.
- Tests that read source files see the schema. If they check text that the
  schema changes, like the exact text of a function body, the baseline fails.
  If the file is named by a string literal, it's left out of the schema and its
  mutants are tested the classic way (see [above](#the-whole-run)); if not, for
  example when the path is built from `file!()` at runtime, every mutant is
  tested the classic way. Nor are missed mutants in a file read through such a
  path tested again the classic way.
- A mutant caught with the schema might be missed the classic way if tests pass
  source text that the schema changes to the mutated code itself, as a parser
  or formatter might when it is tested on its own source: the mutated code then
  processes the schema text, not the text it would process the classic way.
- In crates that deny warnings, a classic mutant can be unviable only because it
  causes a warning, for example an unused parameter when a function body is
  replaced. The same mutant in the schema doesn't cause that warning, so it's
  tested instead. Use `--cap-lints=true` for comparable results.
- Code that runs at compile time can't see the mutant id, so workspace packages
  used by build scripts and proc macros fall back (see above). Tests that run a
  program built from the tree see the mutant only if they pass on the
  environment. When they don't, missed mutants are tested the classic way (see
  above), but a mutant might still be reported caught when the mutated test
  process disagrees with an unmutated child process, where the classic way would
  have mutated both.
- The mutant id is read once per process, the first time mutated code runs, and
  kept for the rest of the process. A test that clears its own environment, or
  removes the variable, before then runs the unmutated code for the rest of that
  process; that's recorded like a cleared environment (see above). A test that
  does so later still runs the mutant. Tests that change
  `CARGO_MUTANTS_SCHEMATA_ID` themselves are not supported.
- Replayed test commands inherit cargo-mutants' environment rather than
  Cargo's, so variables that Cargo sets only for itself are not set.
- Embedded mutants are tested concurrently by default, so the order of the
  results varies between runs.

With coverage-based test selection:

- A mutant that makes a test hang might be reported caught rather than timed out, if a
  selected test fails before the one that hangs runs.
- If selected tests time out, the other selected tests that could fail first when all
  the tests run also run before the mutant is reported timed out, within one more
  timeout: those in earlier test binaries, since Cargo runs the binaries in order and
  stops at the first failure (unless `--no-fail-fast` is given), and those in the
  same binary, since they run alongside the test that hangs, if a failure
  [stops the tests](fail-fast.md). If one fails, the mutant is caught, as it would be
  when all the tests run.
- Selected tests run in a different grouping than usual, so tests that interfere with
  each other can give different results.
- A program run by a test with a cleared environment doesn't write its coverage, so
  its code looks unexecuted. If none of a file's code is seen to run, all tests run for
  its mutants, but if some is, its other code might be reported as not executed.
- Doctests aren't instrumented, so mutants whose code only doctests execute run all
  the tests.
- cargo-mutants runs as rustc's `RUSTC_WORKSPACE_WRAPPER` for the instrumented build,
  and runs any wrapper that was already set in turn.
- A caught mutant's log shows only the selected tests that ran.

## Turning it off

To build and test every mutant the classic way, pass `--no-schemata`, or set
this in `.cargo/mutants.toml`:

```toml
schemata = false
```

To keep schemata but run all the tests for every mutant, pass
`--test-selection=all`, or set:

```toml
test_selection = "all"
```

Options on the command line override the config file: `--schemata` turns schemata
on even if the config turns them off.

To see the complete output of one mutant's tests, see [rerunning one
mutant](fail-fast.md#rerunning-one-mutant-with-complete-output).

## Troubleshooting

`mutants.out/schemata.json` records what happened, and is replaced atomically, so
a program reading it while cargo-mutants runs always sees a complete file. It
lists the fallback mutants and why (`fallback_mutants`, `fallback_by_reason`),
with counts and timings for each stage. For each mutant dropped because of a
compile error, its `blame` lists the errors: the rustc error code, message, and
location in the schema (the lines are those of the original source), and whether
the error was in the mutant's own replacement (`arm`), elsewhere in the `match`
that holds it (`site`), or elsewhere in the file (`file`). `proven_unviable`
marks those recorded unviable without a build.

It also records the checks on the results: `reached` and `retested` for each
mutant tested with the schema, `retested_unreached`,
`concurrent_baseline_passed`, `test_jobs`, `env_cleared_executables`,
`source_read_files`, `source_read_mutants` (the number of mutants embedded
in those files), `baseline_processes` (the number of test processes that recorded
running the tree's code with no mutant selected), and `ran_in_baseline_mutants`
(the number of embedded mutants whose code ran then).

`removed_env` records each variable that the build directories removed from
cargo-mutants' environment, with the value it had, such as `CARGO_INCREMENTAL`.
See [Incremental compilation](build-dirs.md#incremental-compilation).

If the tests failed with the schema and files that tests might read were left out
of it, `baseline_retry` records the `files` and `packages` left out, the number of
`mutants` that fell back, the cost (`failed_baseline_seconds`, `build_passes`,
`build_seconds`, and `baseline_seconds`), and the `outcome`: `passed`,
`schema_changes_behavior` or `failed` if the tests failed again with the schema or
on the unmutated tree, `nothing_embedded` if no mutant was left, or
`nothing_left_out` if no embedded mutant was in those files, so the baseline
wasn't run again. `debug.log` has the same as a `schemata.baseline.retry` event.
Only the second baseline's records of which code ran with no mutant selected are
kept.

The schema's check and build are a fixed cost, recorded in `schemata.json`
(`check_seconds` and `build_seconds`) and in `mutants.out/debug.log` as a
`timing.schema_build` event next to the other [timing
events](output.md#timing-breakdown). It's also the baseline's build phase in
`outcomes.json`, so the timing breakdown shows it as the baseline build. Embedded
mutants aren't built individually, so they have only a test phase. The time spent
on fallback mutants is recorded for each reason they fell back, as
`fallback_time_by_reason` in `schemata.json` (`count` and `seconds`), and as
`timing.schema_fallback` events in `debug.log` (`reason`, `count`, and
`total_secs`). Each stage is logged in `debug.log` as an event whose name starts
with `schemata.`, and the logs of the schema's builds and baseline runs are in
`mutants.out/log/schemata-*.log`.

These environment variables change how schemata work. They're meant for
diagnosing problems and for experiments, and might change in future versions:

| Variable | Values | Effect |
|---|---|---|
| `CARGO_MUTANTS_SCHEMATA_STOP_AFTER` | `build` | Stop after the schema is built, without testing any mutants, to see which mutants are embedded and why the others aren't. |
| `CARGO_MUTANTS_SCHEMATA_DROPPED` | `unviable` (default), `classic` | `classic` builds proven-unviable mutants the classic way rather than recording them as unviable. |
| `CARGO_MUTANTS_SCHEMATA_EXEC` | `direct` (default), `cargo` | `cargo` runs `cargo test` for each mutant rather than replaying the test commands. |
| `CARGO_MUTANTS_SCHEMATA_CHECK_PHASE` | `build` (default), `check` | `check` uses `cargo check` for the loop that drops mutants that don't compile, followed by a separate build. In a fresh build directory that compiles the dependencies twice, but it can be faster when many passes are needed, since each pass is then only a check. |
| `CARGO_MUTANTS_COVERAGE_CONFIRM` | `reached` (default), `all`, `passed`, `none` | Which mutants are confirmed by running all the tests: `reached` confirms those whose selected tests pass, and those that no test executes whose code ran in the baseline (see [above](#mutants-reported-missed-without-running-tests)); `all` confirms every mutant that no test executes too, which is slower but doesn't rely on the schema recording where code ran. `passed` and `none` are unsafe, for experiments only: they can report mutants missed that all the tests catch. `passed` confirms only those whose selected tests pass, so a mutant that no test's coverage shows running is reported missed even if its code ran in a process that was killed; `none` confirms none, so a mutant is reported missed if its selected tests pass. |
| `CARGO_MUTANTS_COVERAGE_UNOBSERVED_FILES` | `full_suite` (default), `uncovered` | `uncovered` reports mutants in files where no code ran in any test as missed without running tests, rather than running all the tests for them. This is faster when whole files are untested, for example code whose tests need features that aren't enabled, but trusts that no test ran the file's code in a program whose coverage was lost. |

An unrecognized value is reported as a warning, and the default is used.
