# fallback-build-cost

## Problem

Fallback mutants are most of a full run's wall-clock time. A fallback mutant is one the schema
cannot embed, so the fork tests it the classic way, with a build of its own. On
`jast-platform`'s `backend-core` invocation, fallback testing took 55% of the wall (Measured 1).

**The cause is an inherited environment variable, not the fallback design.** The shell that ran
the gate exported `CARGO_INCREMENTAL=0`, and cargo-mutants passes it to every cargo command in
its own scratch build directories (Measured 5). Each fallback build then compiled the mutated
package from nothing. With incremental compilation on, the same build takes half the time.
An unviable `const` mutant fails in 3.6 s. The run's median for one was 28.4 s, with two builds
sharing the machine (Measured 6).

**This section said the cost came from rebuilding every test target, and that `const` mutants
needed different handling. Both were wrong as causes.** Every target is rebuilt, but narrowing
the targets saves little once the build is incremental (Measured 6 and 7). Kane asked for those
two changes on 2026-10-05. Out of scope records them with the numbers that would bring them back.

`CARGO_INCREMENTAL=0` is a reasonable setting for a shell. `jast-platform`'s agents set it so that
a shared `target/` does not regrow its incremental cache between batches. It is the wrong setting
for a scratch build directory that cargo-mutants creates, rebuilds dozens of times, and deletes.

## User outcome

A maintainer running the full gate waits for the mutants that need testing, not for repeated
whole-package rebuilds. The outcome counts are unchanged. The speed does not depend on what the
maintainer's shell exports.

## Measured

Measured 1 to 4 are from the fork at `27.1.0+omnith.1`, rev `2f837e8`, run by `jast-platform`'s
`just merge-gate` on 2026-10-05 (start `16:49:48Z`) on branch `feat/bor-1-outage-requeue` at
`07e57895`. Read from `target/mutants-core/mutants.out/` of that worktree. **That run had
`CARGO_INCREMENTAL=0` in its environment** (Measured 5), so its build times are not incremental.

**1. Wall-clock split for `-p backend-core --features pg-tests,s3-tests`.**
`schemata.json`:

```
wall_seconds               2168.5
check_seconds                71.7
build_seconds                22.0
baseline_test_seconds        36.8
mutant_tests_wall_seconds   696.6
fallback_wall_seconds      1190.5
jobs / test_jobs              2 / 2
mutants 3017, embedded 2345, fallback 672
```

**2. Fallback time by reason.** `schemata.json` → `fallback_time_by_reason`. Seconds are serial,
summed over mutants:

```
compile_error      count 604  seconds   18.5
const_context      count  47  seconds 1999.0
unsupported_genre  count   5  seconds  237.3
impl_trait_return  count   4  seconds  112.1
let_chain          count  12  seconds    0.0
```

By file, the 47 `const_context` mutants are: `s3_object_store.rs` 12, `config.rs` 8,
`clamav_scanner.rs` 8, `derivation.rs` 8, others 11. One source line, `config.rs:675`,
`const SOURCE_MAX_BYTES_DEFAULT: u64 = 64 * 1024 * 1024 * 1024;`, gives six.

Of the 672 fallback mutants, 615 were proven unviable by the check pass and never built. The other
57 were built and tested the classic way. Phase durations are from `outcomes.json`
`phase_results`, and the catching test binary is from each mutant's log:

```
reason             summary       n   build sum  build median  test sum
const_context      CaughtMutant  26     1093 s       42.1 s     172 s
const_context      Unviable      21      733 s       28.4 s       0 s
unsupported_genre  CaughtMutant   5      215 s       41.2 s      23 s
impl_trait_return  CaughtMutant   1       59 s       58.8 s       8 s
impl_trait_return  Unviable       3       46 s       14.1 s       0 s
compile_error      Unviable       1       19 s       18.5 s       0 s
```

The lib unit-test binary `backend_core` caught all 32 caught mutants. None was missed. All 21
unviable `const_context` mutants failed on a `const` assertion in test code, for example
`s3_object_store.rs:3138`:

```
error[E0080]: evaluation panicked: assertion failed: MULTIPART_THRESHOLD_BYTES >= 5 * 1024 * 1024
error: could not compile `backend-core` (lib test) due to 1 previous error
```

**3. What a fallback build compiles.** `log/apps__backend-core__src__config.rs_line_675_col_67_001.log`
ran 28 rustc commands: the lib, the lib test harness, each binary, each binary's test harness,
and each integration test. None carried `-C incremental=`:

```
*** .../cargo test --no-run --profile=mutants --verbose --package=backend-core@0.1.0 --features=pg-tests,s3-tests
    Finished `mutants` profile [unoptimized] target(s) in 41.57s
```

**4. The other invocations of the same run.** Fallback is a smaller share where the package is
smaller. `schemata.json` per invocation:

```
yunyun   wall 344 s   fallback_wall  46.8 s   unsupported_genre 4 → 80.8 s
format   wall 174 s   fallback_wall  74.9 s   const_context 16 → 78.4 s, source_read_by_tests 12 → 58.1 s
codegen  wall  61 s
dedup    wall  26 s
```

**5. Where `CARGO_INCREMENTAL=0` came from.** The fork never sets it:
`grep -rn INCREMENTAL src tests` matches nothing. Without it in the environment, cargo enables
incremental for this profile. In a scratch worktree of `jast-platform` `main` at `aadd3fe8`:

```
$ env -u CARGO_INCREMENTAL cargo test --no-run --profile=mutants -v -p backend-core \
    --features pg-tests,s3-tests 2>&1 | grep -c "incremental="
27
```

`jast-platform`'s execution plans tell every agent to run cargo and `just` with
`CARGO_INCREMENTAL=0`, for example `docs/work/increment-3/build-outage-requeue/plan.md`, line 95.
The gate inherits it from there.

**6. A fallback build, by target set and incremental mode.** One build at a time in that scratch
worktree, `CARGO_TARGET_DIR` its own, warmed first, each row a fresh mutation of
`config.rs:675` or `s3_object_store.rs:43`. `cargo test --no-run --profile=mutants -p
backend-core --features pg-tests,s3-tests`, plus `--lib` where named:

| Build | `CARGO_INCREMENTAL=0` | `CARGO_INCREMENTAL=1` |
|---|---|---|
| all targets, viable mutant | 21.0 s, 22.4 s | 10.5 s |
| `--lib` only, viable mutant | 15.4 s, 14.9 s | 5.4 s, 6.8 s |
| all targets, unviable `const` mutant | 28.4 s median in Measured 2 | 3.6 s |
| `--lib` only, unviable `const` mutant | | 2.5 s |

The run in Measured 1 had two fallback workers building at once, which is why its viable builds
took about 42 s against 21 s here.

**7. Widening after a narrow build.** After a `--lib` build of a mutant (6.8 s), the all-targets
build of the same mutant took 10.2 s more and did not recompile the lib test harness. Its only
`--test` rustc line for `backend_core` was the `main.rs` binary's harness. Narrow-then-widen
costs 17 s where one all-targets build costs 10.5 s.

**8. Disk.** The incremental cache of that worktree's `target/mutants/incremental` was 1.4 GB,
in a `target/` of 3.8 GB. Each scratch build directory holds one while a run is in progress.

**9. A seeded build directory does not reuse a copied incremental cache.** The probe copied the
warm worktree, including `target/`, to a new path with `cp -c -R`. It gave every workspace `.rs`
file a newer modification time, as seeding does (`book/src/build-dirs.md`), then mutated
`config.rs:675` and built with `CARGO_INCREMENTAL` unset:

| Copied `target/mutants/incremental` | First build | Second build, another mutation |
|---|---|---|
| kept, 1.5 GB after | 37.7 s, 28 rustc commands | 12.9 s |
| removed before the build | 31.8 s, 28 rustc commands | 12.1 s |

The copied cache made the first build slower, not faster. Later builds are warm either way.

**10. Cargo's precedence for incremental mode.** Found by the architecture review on cargo 1.97.1,
on a crate with a `mutants` profile that inherits `test`, counting `incremental=` in `cargo build
-v`. `CARGO_INCREMENTAL` wins over everything. Next is `build.incremental`, from a config file or
from `CARGO_BUILD_INCREMENTAL`. The profile's `incremental` comes last:

| Setting | `incremental=` lines |
|---|---|
| none | 1 |
| `CARGO_INCREMENTAL=0` | 0 |
| `CARGO_BUILD_INCREMENTAL=false` | 0 |
| `[build] incremental = false` in config | 0 |
| profile `incremental = false` | 0 |
| `CARGO_INCREMENTAL=1` with `CARGO_BUILD_INCREMENTAL=false` | 1 |
| profile `false` with `CARGO_BUILD_INCREMENTAL=true` | 1 |

**11. Every cargo command in a scratch build dir takes its environment from
`build_dir_cargo_env`.** Verified by the architecture review: the classic lab (`lab.rs:451` →
`run_cargo`), check and build iterations (`run.rs:326`), the baseline (`run.rs:436`, `:449`),
`concurrent_baseline` and `probe_jobs` (through `run_tests`), replays (`run.rs:678`), the
`cargo test` path (`run.rs:1001`), and both coverage sites (`collect.rs:391` and `:501`, which
start from `Runner::cargo_env`). In the run of Measured 1, `debug.log` holds 6,228
`Overriding` events, one for each `start process`.

Re-derive Measured 1 and 2: read the named key from `mutants.out/schemata.json`, or sum
`phase_results[].duration` by phase in `outcomes.json` for the mutants named in
`fallback_mutants`. Re-derive Measured 6, 7 and 9: the probe scripts mutate one line, time the
cargo command above with each setting, then restore the line.

## Approach

**A command that cargo-mutants runs in a build directory it owns does not inherit
`CARGO_INCREMENTAL` or `CARGO_BUILD_INCREMENTAL`.** Then cargo's config files and the profile
decide (Measured 10). A profile that inherits from `test` or `dev` has incremental on.

A setting in a config file or in the profile is honoured, because it is a decision about the
build. An environment variable usually is not: it is set for the whole shell, for a reason
unrelated to mutation testing. `CARGO_BUILD_INCREMENTAL` is removed with `CARGO_INCREMENTAL` for
the reason that `build_dir_cargo_env` treats `CARGO_BUILD_TARGET_DIR` with `CARGO_TARGET_DIR`
(`src/cargo.rs:112`). Either spelling in the environment disables incremental mode.

With `--in-place` the build directory is the user's own tree, so their environment stands. This
follows the existing rule for `CARGO_TARGET_DIR`, which `build_dir_cargo_env` overrides in
scratch directories and keeps in place.

**Removing, not setting.** Setting `CARGO_INCREMENTAL=1` would override a profile's
`incremental = false` without saying so. Removing the inherited variables leaves the config and
the profile in charge. A user who wants incremental off in scratch builds, for example on a
disk-bound CI runner, sets `incremental = false` in the mutation profile or `build.incremental =
false` in cargo config.

**The removal covers every command `build_dir_cargo_env` serves** (Measured 11). That includes the
coverage collector's instrumented build and its test runs. It is one build per run, so it costs
one incremental cache's disk and gains nothing measured. Leaving it out would need a second
environment path for one build, and the two would drift.

**Seeding a build directory skips the incremental cache.** `copy_target_dir` copies the baseline
build's `target/` into each extra build directory and into the coverage copy. With incremental
on, that `target/` holds a cache of about 1.4 GB (Measured 8), and a seeded directory does not
reuse it (Measured 9). The copy leaves out every directory named `incremental` that sits directly
in a profile's output directory, `target/<profile>/` or `target/<triple>/<profile>/`.

**The first fallback build in each build directory is a full build of the workspace packages.**
Measured 9 shows it for a seeded directory. build_dir_0's cache was built from the schema's
source, which differs from the restored original in most functions. Later builds in a directory
are incremental. With `--jobs 2` that is two cold builds per run.

Tests that cargo runs also stop seeing an inherited `CARGO_INCREMENTAL` or
`CARGO_BUILD_INCREMENTAL`.

## Interfaces / contracts

**`process.rs` owns the environment type.** `Process::run` and `Process::start` take it in place
of `&[(String, String)]`. `cargo.rs` builds it. `process.rs` sits below `cargo.rs` and must not
depend on it.

```rust
/// Environment changes for a child process, relative to cargo-mutants' own environment.
pub struct Env {
    /// Variables inherited from cargo-mutants' environment that the child must not see.
    pub remove: Vec<String>,
    /// Variables to set.
    pub set: Vec<(String, String)>,
}
```

**`Process::start` applies `remove` first, then `set`.** An explicit `set` therefore always wins.
`remove` exists for inherited values only.

**`build_dir_cargo_env` returns an `Env`.** `remove` holds `CARGO_INCREMENTAL` and
`CARGO_BUILD_INCREMENTAL` when the build dir is not in place, and is empty in place. It holds the
names whether or not the variables are set, so the contract does not depend on the caller's
environment. The schemata runner's `cargo_env` and the coverage collector add their variables to
`set` and pass `remove` through unchanged.

**`copy_target_dir` skips a profile's `incremental` directory.** The skip applies to seeding and
to the coverage copy, which both call it. Nothing else about the copy changes.

**The removal is reported once per run, never per command.** `build_dir_cargo_env` runs for every
spawned process, including every replayed test command (Measured 11), so an event there would add
one line per process.
- One debug event at the start of the run names each removed variable that was set, with its
  inherited value. It is emitted only when the run uses scratch build directories.
- `schemata.json` gets `removed_env`, a map from each removed variable that was set to its
  inherited value, for example `{"CARGO_INCREMENTAL": "0"}`. It is empty when nothing was set or
  the run is in place. Measured 5 needed a grep through mutant logs to find the cause. This key
  replaces that.

The plan fixes the type's exact name and visibility, and the call site of the once-per-run event.
The contract is the two halves, the removal order, the two variable names and the two reports.

## Acceptance criteria

1. **Unit tests in `cargo.rs`**, beside
   `build_dir_cargo_env_sets_cargo_target_dir_to_own_target_except_in_place`: `remove` names both
   variables for a scratch build dir and is empty with `--in-place`. **In `process.rs`**: a
   variable in both `remove` and `set` reaches the child with the `set` value.
2. **A unit test in `copy_tree.rs`**, beside `copy_target_dir_when_requested`: a
   `target/debug/incremental/` and a `target/<triple>/debug/incremental/` are not copied. A file
   beside each is copied.
3. **Integration tests in `tests/main.rs`, one per path**, following the two tests of the
   `CARGO_TARGET_DIR` change at `tests/main.rs:5681` (`--no-schemata -j2`) and `:5712`
   (`--schemata`). Each runs on a small testdata tree with `CARGO_INCREMENTAL=0` and
   `CARGO_BUILD_INCREMENTAL=false` in the environment. The classic test asserts that a mutant's
   log has a rustc line for the mutated crate carrying `-C incremental=`. The schemata test
   asserts the same of a `schemata-build` log, and that `schemata.json` `removed_env` names both
   variables with their values. Watch each fail on the current code before the change.
4. **The same mutants get the same outcomes, faster, and the speed does not depend on the
   shell.** In `jast-platform` on `main`, with `just dev-db` and `just dev-minio` up (the
   `pg-tests,s3-tests` features need Postgres and MinIO), run
   `cargo mutants -p backend-core --features pg-tests,s3-tests -j2 --file apps/backend-core/src/config.rs --file apps/backend-core/src/adapters/s3_object_store.rs`
   three times:

   | Run | Rev | Environment |
   |---|---|---|
   | A | `2f837e8` | `CARGO_INCREMENTAL=0` exported |
   | B | this item's | `CARGO_INCREMENTAL=0` exported |
   | C | this item's | `CARGO_INCREMENTAL` unset |

   - Every mutant's outcome in `outcomes.json` is identical across A, B and C.
   - The two files hold 20 of the 47 `const_context` mutants in Measured 2. Each run's
     `schemata.json` names at least 20 classically tested fallback mutants. Fewer means the
     comparison does not measure this change.
   - B's `fallback_wall_seconds` is at most half of A's. C's is within 20% of B's.
   - In B, every classically built fallback mutant's log carries `-C incremental=` on the
     `backend_core` rustc line. In A, none does.
   - B's `schemata.json` `removed_env` is `{"CARGO_INCREMENTAL": "0"}`. C's is empty.
5. `cargo test --all-features` and `cargo clippy --all-targets --all-features -- -D warnings`
   pass in the fork. `cargo fmt` is clean.
6. `NEWS.md` and `book/src/build-dirs.md` say that a scratch build dir ignores an inherited
   `CARGO_INCREMENTAL` and `CARGO_BUILD_INCREMENTAL`, and why. They say that `--in-place`, a
   config file and the profile are honoured, how to turn incremental mode off, and that seeding
   skips the incremental cache.

## Out of scope

| Item | Why not now | What brings it back |
|---|---|---|
| Row 1: build only the targets of the tests that reach a fallback mutant, widen when they pass | Measured 6 and 7 with incremental on: it saves about 4 to 5 s per caught mutant and costs about 7 s per missed one. It can also report a mutant caught that the classic way reports unviable, when the mutant breaks only a target outside the narrow set | a re-measured run after this item where caught fallback builds still take a large share of `fallback_wall_seconds` |
| Row 2: handle `const`-context mutants differently, for example by checking them before building | Measured 6: with incremental on, an unviable `const` mutant fails in about 3.6 s. A viable one costs the same as any other fallback build | the same re-measure, with `const_context` still the largest reason in `fallback_time_by_reason` |
| Bumping the fork's rev in `jast-platform` | That repository pins the rev in its `Justfile` (`just _mutants-tool`) | this item merging. It is a one-line change there |

## Dependencies

None in this repository. `jast-platform`'s `mutants-test-waits` item cuts the embedded-test half of
the same run. The two are independent.
