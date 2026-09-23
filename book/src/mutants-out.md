# The `mutants.out` directory

A `mutants.out` directory is created in the original source directory. You can put the output directory elsewhere with the `--output` option
or using `CARGO_MUTANTS_OUTPUT` environment variable or via `output` directive in the config file.

On each run, any existing `mutants.out` is renamed to `mutants.out.old`, and any
existing `mutants.out.old` is deleted.

The output directory contains:

* A `lock.json`, on which an [fs2 lock](https://docs.rs/fs2) is held while
  cargo-mutants is running, to avoid two tasks trying to write to the same
  directory at the same time. `lock.json` contains the start time, cargo-mutants
  version, username, and hostname. `lock.json` is left in `mutants.out` when the
  run completes, but the lock on it is released.

* A `mutants.json` file describing all the generated mutants.
  This file is completely written before testing begins.

* An `outcomes.json` file describing the results of all tests,
  summary counts of each outcome, and the cargo-mutants version.
  `end_time` is set once the run finishes; it is `null` in a run that is still in progress
  or was interrupted.

* A `diff/` directory, containing a diff file for each mutation, relative to the unmutated baseline.
  `mutants.json` includes for each mutant the name of the diff file.

* A `log/` directory, with one log file for each mutation plus the baseline
  unmutated case. The log contains the diff of the mutation plus the output from
  cargo. `outcomes.json` includes for each mutant the name of the log file.
  When a name is used more than once, later logs get a suffix such as `_001`.

  With [schemata](schemata.md), `log/` also has logs of the steps that build and
  check the schema, named `schemata-*.log`:

  * `schemata-check-N.log` and `schemata-build-N.log`: the Nth pass of checking or
    building the schema, after which mutants that don't compile are dropped from it.
  * `schemata-baseline.log`: the baseline tests of the schema with no mutant active,
    one for each set of packages tested.
  * `schemata-baseline-original.log`: if those tests fail, the same tests of the
    unmutated tree, to tell whether the schema changed the tests' behavior.
  * `schemata-baseline-direct.log`: the baseline's test commands run again directly,
    without cargo, as mutants are then tested.
  * `schemata-baseline-concurrent.log`: the baseline's tests run by several jobs at
    once, to find tests that interfere with each other in a shared build directory.
  * `schemata-coverage-list.log` and `schemata-coverage-tests.log`: with
    coverage-based test selection, building the instrumented tests and listing them,
    and running the tests to collect coverage.

* With schemata, a `schemata.json` file recording how the mutants were built and
  tested: how many were built into the schema and how many were tested separately,
  and why, the passes that checked and built the schema, the timings of each step,
  and, with coverage-based test selection, the tests chosen for each mutant. Like
  `outcomes.json`, it's replaced atomically.

* With coverage-based test selection, `uncovered.txt`, listing the mutants reported
  missed without running any tests, because no test executes their code. These are
  also listed in `missed.txt`.

* `caught.txt`, `missed.txt`, `timeout.txt`, `unviable.txt`, each listing mutants with the corresponding outcome.
  Mutants are added to these lists as they finish, and when the run ends, including when it's
  interrupted, each list is rewritten in the order the mutants were discovered (the order of
  `--list`), so that the lists are the same from run to run even though concurrent jobs
  finish mutants in varying order. The outcomes in `outcomes.json` stay in the order the
  mutants finished.

* A `debug.log` file with detailed trace messages, including, at the end of the run, the
  [timing breakdown](output.md#timing-breakdown) as structured events.

* `previously_caught.txt` accumulates a list of mutants caught in previous runs with [`--iterate`](iterate.md).

The contents of the directory and the format of these files is subject to change in future versions.

These files are incrementally updated while cargo-mutants runs, so other programs can read them to follow progress.

`outcomes.json` is replaced atomically, so readers always see either the previous complete file or the new one, never a partly written file. While mutants are being tested, it's rewritten at most about once per second, so it may briefly lag behind the `.txt` lists. It's brought fully up to date when the run finishes, including when the run is interrupted with Ctrl-C or stopped by an error.

Because each update writes a new file (named `.outcomes.json.*.tmp` while it's being written) and renames it over the old one, programs that watch `outcomes.json` for changes should watch the `mutants.out` directory rather than the file itself. If cargo-mutants is killed while writing, a leftover `.tmp` file may remain; it can be deleted.

There is generally no reason to include this directory in version control, so it is recommended that you add `/mutants.out*` to your `.gitignore` file or equivalent. This will exclude both `mutants.out` and `mutants.out.old`.
