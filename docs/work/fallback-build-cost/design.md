# fallback-build-cost

## Problem

Fallback mutants are most of a full run's wall-clock time. A fallback mutant is one the schema
cannot embed, so the fork tests it the classic way, with a build of its own. On
`jast-platform`'s `backend-core` invocation, fallback testing took 55% of the wall (Measured 1).

**The cause is an inherited environment variable, not the fallback design.** The shell that ran
the gate exported `CARGO_INCREMENTAL=0`, and cargo-mutants passes it to every cargo command in
its own scratch build directories (Measured 5). Each fallback build then compiled the mutated
package from nothing. With incremental compilation on, the same build takes half the time, and an
unviable `const` mutant fails in an eighth of the time (Measured 6).

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

Re-derive Measured 1 and 2: read the named key from `mutants.out/schemata.json`, or sum
`phase_results[].duration` by phase in `outcomes.json` for the mutants named in
`fallback_mutants`. Re-derive Measured 6 and 7: the probe scripts mutate one line, time the
cargo command above with each setting, then restore the line.

## Approach

**A cargo command in a build directory that cargo-mutants owns does not inherit
`CARGO_INCREMENTAL`.** The profile then decides. A profile that inherits from `test` or `dev`
has incremental on. A profile that sets `incremental = false` is honoured, because that is a
decision about the mutation build and the environment variable usually is not.

With `--in-place` the build directory is the user's own tree, so their environment stands. This
follows the existing rule for `CARGO_TARGET_DIR` in `build_dir_cargo_env`, which overrides the
user's setting in scratch directories and keeps it in place.

**Removing, not setting.** Setting `CARGO_INCREMENTAL=1` would override a profile's
`incremental = false` without saying so. Removing the inherited variable leaves the profile in
charge.

**The first fallback build in each build directory is still a full build of the workspace
packages.** A directory's incremental cache is built from the schema's source, and a seeded
directory has a different path, so neither matches the restored original source. Later builds
in that directory are incremental. With `--jobs 2` that is two cold builds per run.

The change applies to every cargo command that `build_dir_cargo_env` serves: the classic lab's
check, build and test phases, and the schemata runner's check, build, baseline and test
commands. Tests that cargo runs also stop seeing an inherited `CARGO_INCREMENTAL`.

## Interfaces / contracts

**`build_dir_cargo_env` returns what to set and what to remove.** A small struct replaces the
bare `Vec<(String, String)>` at this boundary:

```rust
/// Environment changes for a cargo command run in a build dir.
pub(crate) struct CargoEnv {
    /// Variables to set.
    pub set: Vec<(String, String)>,
    /// Variables inherited from cargo-mutants' environment that the command must not see.
    pub remove: Vec<String>,
}
```

`remove` holds `CARGO_INCREMENTAL` when the build dir is not in place, and is empty in place. It
holds the name whether or not the variable is set, so the contract does not depend on the
caller's environment.

**`Process::run` and `Process::start` apply removals** with `Command::env_remove` after
`Command::envs`. Their five callers (`cargo.rs` `run_cargo`, `schemata/run.rs` at three sites,
`schemata/coverage/collect.rs`) pass the removals through. The coverage collector passes none: it
builds its own environment for an instrumented copy, and this item does not change it.

The schemata runner's `cargo_env` adds the mutant id and other variables to the `set` half. It
passes `remove` through unchanged.

**One debug event when an inherited value is dropped.** It names the variable and its value, in
the same shape as the `CARGO_TARGET_DIR` override's event. A slow run is then explained by
`debug.log`, and the rustc lines in each mutant's log show `-C incremental=`.

The plan defines the type's name, module and visibility. The contract is the two halves, and that
`remove` is applied after `set`.

## Acceptance criteria

1. **A unit test** beside `build_dir_cargo_env_sets_cargo_target_dir_to_own_target_except_in_place`:
   `remove` names `CARGO_INCREMENTAL` for a scratch build dir and is empty with `--in-place`.
2. **An integration test** in `tests/main.rs` runs cargo-mutants on a small testdata tree with
   `CARGO_INCREMENTAL=0` in its environment. A mutant's log shows a rustc line for the mutated
   crate that carries `-C incremental=`. Watch it fail on the current code before the change.
3. **The same mutants get the same outcomes, faster.** In `jast-platform` on `main`, with
   `CARGO_INCREMENTAL=0` exported, run
   `cargo mutants -p backend-core --features pg-tests,s3-tests -j2 --file apps/backend-core/src/config.rs --file apps/backend-core/src/adapters/s3_object_store.rs`
   once with rev `2f837e8` and once with this item's rev. The two files hold 20 of the 47
   `const_context` mutants in Measured 2.
   - Every mutant's outcome in `outcomes.json` is identical between the two runs.
   - `fallback_wall_seconds` in `schemata.json` is at most half of the old rev's.
   - Every fallback mutant's build log on the new rev shows `-C incremental=`.
4. `cargo test --all-features` and `cargo clippy --all-targets --all-features -- -D warnings`
   pass in the fork. `cargo fmt` is clean.
5. `NEWS.md` and `book/src/build-dirs.md` say that a scratch build dir ignores an inherited
   `CARGO_INCREMENTAL`, why, and that `--in-place` keeps it.

## Out of scope

| Item | Why not now | What brings it back |
|---|---|---|
| Row 1: build only the targets of the tests that reach a fallback mutant, widen when they pass | Measured 6 and 7 with incremental on: it saves about 4 to 5 s per caught mutant and costs about 7 s per missed one. It can also report a mutant caught that the classic way reports unviable, when the mutant breaks only a target outside the narrow set | a re-measured run after this item where caught fallback builds still take a large share of `fallback_wall_seconds` |
| Row 2: handle `const`-context mutants differently, for example by checking them before building | Measured 6: with incremental on, an unviable `const` mutant fails in about 3.6 s. A viable one costs the same as any other fallback build | the same re-measure, with `const_context` still the largest reason in `fallback_time_by_reason` |
| The coverage collector's environment | One build per run, not one per mutant | a measured coverage build that is slow for the same reason |
| Bumping the fork's rev in `jast-platform` | That repository pins the rev in its `Justfile` (`just _mutants-tool`) | this item merging. It is a one-line change there |

## Dependencies

None in this repository. `jast-platform`'s `mutants-test-waits` item cuts the embedded-test half of
the same run. The two are independent.
