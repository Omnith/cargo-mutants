# fallback-build-cost

## Problem

Fallback mutants are most of a full run's wall-clock time. A fallback mutant is one the schema
cannot embed, so the fork tests it the classic way, with a build of its own. On
`jast-platform`'s `backend-core` invocation, fallback testing took 55% of the wall (Measured 1).

**The cause is an inherited environment variable, not the fallback design.** The shell that ran
the gate exported `CARGO_INCREMENTAL=0`, and cargo-mutants passes it to every cargo command in
its own scratch build directories (Measured 5). Each fallback build then compiled the mutated
package from nothing. With incremental compilation on, one build alone takes half the time, and
an unviable `const` mutant fails in 3.6 s against the run's 28.4 s median (Measured 6). In the
real run shape, on a loaded machine, the fallback phase took 0.4 to 0.7 of its time with
incremental off. Every outcome was the same (Measured 12).

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
maintainer's shell exports. A full disk stops the run with an error. It does not hide a missed mutant as
an unviable one.

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

The run in Measured 1 had two fallback workers building at once. Its viable builds took about
42 s against 21 s here. Contention is the likely cause, but this probe did not measure it.
Measured 12 measures the real run shape.

**7. Widening after a narrow build.** After a `--lib` build of a mutant (6.8 s), the all-targets
build of the same mutant took 10.2 s more and did not recompile the lib test harness. Its only
`--test` rustc line for `backend_core` was the `main.rs` binary's harness. Narrow-then-widen
costs 17 s where one all-targets build costs 10.5 s.

**8. Disk.** The incremental cache of that worktree's `target/mutants/incremental` was 1.4 GB,
in a `target/` of 3.8 GB. Each scratch build directory holds one while a run is in progress.
The adversarial review measured more in the same worktree:
- A fresh cache is 0.83 GB. It reaches about 1.5 GB after one more build, because rustc keeps two
  sessions per crate. It stayed near 1.5 GB over 8 more builds.
- `cargo check --tests` keeps a cache of its own. One check pass grew the cache from 1.4 GB to
  1.8 GB, and from 28 crate directories to 46. build_dir_0 runs the schema's check pass, so it
  holds about 1.9 GB.
- A `backend-core` run with `-j2` therefore peaks at about 3.4 GB more than it does without
  incremental mode. The coverage copy is deleted before the fallback build dirs exist
  (`src/schemata/mod.rs:1324-1340`), so it does not add to the peak. A classic run with
  `--no-schemata -jN` adds about N × 1.5 GB.

**9. A seeded build directory does not reuse a copied incremental cache.** The probe copied the
warm worktree, including `target/`, to a new path with `cp -c -R`. It gave every workspace `.rs`
file a newer modification time, as seeding does (`book/src/build-dirs.md`), then mutated
`config.rs:675` and built with `CARGO_INCREMENTAL` unset:

| Copied `target/mutants/incremental` | First build | Second build, another mutation |
|---|---|---|
| kept, 1.5 GB after | 37.7 s, 28 rustc commands | 12.9 s |
| removed before the build | 31.8 s, 28 rustc commands | 12.1 s |

The copied cache was not reused: both first builds ran all 28 rustc commands, and the second
builds took the same time. Each row is one sample, so the 6 s difference between the first builds
is within noise. The finding is "no reuse", not "slower". Later builds are warm either way.

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

The adversarial review added, on cargo 1.96: `CARGO_PROFILE_MUTANTS_INCREMENTAL=false`,
`CARGO_PROFILE_TEST_INCREMENTAL=false` and `CARGO_PROFILE_DEV_INCREMENTAL=false` each gave 0 lines
for a profile `mutants` that inherits `test`. These set the profile from the environment.

**11. Every cargo command in a scratch build dir takes its environment from
`build_dir_cargo_env`.** Verified by the architecture review: the classic lab (`lab.rs:451` →
`run_cargo`), check and build iterations (`run.rs:326`), the baseline (`run.rs:436`, `:449`),
`concurrent_baseline` and `probe_jobs` (through `run_tests`), replays (`run.rs:678`), the
`cargo test` path (`run.rs:1001`), and both coverage sites (`collect.rs:391` and `:501`, which
start from `Runner::cargo_env`). In the run of Measured 1, `debug.log` holds 6,228
`Overriding` events, one for each `start process`.

**12. The real run shape, end to end.** Installed fork `27.1.0+omnith.1` (rev `2f837e8`), a
scratch worktree of `jast-platform` `main` at `aadd3fe8`, its own Postgres and MinIO
(`COMPOSE_PROJECT_NAME=rbp-fbc`), 2026-10-05:

```
cargo mutants -p backend-core --features pg-tests,s3-tests -j2 \
  --file apps/backend-core/src/config.rs --file apps/backend-core/src/adapters/s3_object_store.rs
```

Four runs back to back, alternating the environment. Every run had 337 mutants, 272 embedded and
65 fallback, of which 20 were built and tested the classic way. Every mutant's outcome was the
same in all four runs: 280 caught, 57 unviable.

| Run | `CARGO_INCREMENTAL` | `fallback_wall_seconds` | classic build median | classic build sum | `wall_seconds` |
|---|---|---|---|---|---|
| A | `0` | 404.9 | 36.3 s | 761 s | 803.1 |
| A′ | unset | 285.4 | 19.2 s | 494 s | 642.7 |
| A2 | `0` | 369.9 | 29.9 s | 691 s | 846.0 |
| A′2 | unset | 154.9 | 4.9 s | 259 s | 469.5 |

- A′ over A is 0.70. A′2 over A2 is 0.42. Summed over both pairs it is 0.57.
- Another session's full mutation gate ran on the same machine during all four runs. The load
  average was 32 at the end of A′ and 14 at the end of A′2. The spread between the pairs is that
  load, not the setting.
- The slowest build with incremental on was the first in a build dir: 71.6 s in A′, 53.8 s in
  A′2. With incremental off the slowest was 50.9 s and 53.1 s.
- In A′ the unviable `s3_object_store.rs` builds took 8.8 to 34.3 s, against 3.6 s alone in
  Measured 6.
- Only A′'s logs carry `incremental=`. Four of its logs for `s3_object_store.rs:43` have it, and
  none of A's do.

The single-build probes in Measured 6 overstate the gain under load. The real gain is between
0.4 and 0.7 of the fallback phase.

**13. What cargo and rustc print when the disk fills.** A 60 MB HFS+ disk image, filled to
572 KiB free, a one-function crate, `CARGO_TARGET_DIR` on the image, 2026-10-05. Paths shortened
to `<mnt>`:

```
$ cargo test --no-run -v          # exit 101
error: could not write output to <mnt>/t-build/debug/deps/enospc_probe-....rcgu.o: No space left on device
error: failed to build archive at `<mnt>/t-build/debug/deps/libenospc_probe-....rlib`: couldn't create a directory for the temp file: No space left on device (os error 28) at path "<mnt>/
$ cargo check --tests -v          # exit 101, once the image was full
error: No space left on device (os error 28) at path "<mnt>/t-check2Bgxhcw"
```

rustc's object write, its archive step and cargo itself each print `No space left on device`.
That is the `strerror` text for `ENOSPC` on macOS and Linux. Windows prints `There is not enough
space on the disk. (os error 112)` for `ERROR_DISK_FULL`. Both reach a classic mutant's log, and
`outcome.rs:278` reports the mutant `Unviable` because its build failed.

**14. Where the first disk-full check went wrong.** Found by the plan's adversarial review,
2026-10-05, on a scratch build of this plan's detector, and on a filled 60 MB disk image:
- **The schemata path quotes source in JSON.** `drop_until_clean` adds `--message-format=json`
  (`src/schemata/run.rs:322`), so each diagnostic is one line beginning `{"reason":...`, and its
  `rendered` and `spans[].text` fields quote the source line. A crate holding
  `let expected = "No space left on device";` gave an ordinary warning, and the schema build
  stopped with `Error: the disk is full: cargo build failed`. `--no-schemata` on the same crate
  gave `4 mutants tested: 2 caught, 2 unviable`.
- **rustc's suggestion gutter quotes source too:** `4 -     report("No space left on device");`.
- **The macOS linker words it differently.** Linking a 4 MB binary onto the full image gave
  `error: linking with \`cc\` failed: exit status: 1` and
  `= note: ld: ftruncate() failed, errno=28 for '<mnt>/big'`. Neither marker of Measured 13
  appears.
- **With `-jN` the other workers drain the queue.** `join_threads` (`src/lab.rs:240`) waits for
  every worker, and `run_queue` takes the next mutant regardless. With `-j2` and one mutant that
  hit the marker, the error printed at once. The run then tested the 24 other mutants for 92 s
  on one worker and exited 1.
- **Removing `CARGO_INCREMENTAL=1` turns incremental off** when the profile says
  `incremental = false`: `cargo build -v` gave one `incremental=` line with it and none without.
- **A colored quoted line starts with an escape sequence, not its line number.** Found in
  Batch B's execution, 2026-10-05. Under `CARGO_TERM_COLOR=always`, which the fork's own CI sets
  (`.github/workflows/tests.yml:38`), `od -c` on a classic `disk_full_literal` log showed the
  quoted line as `033[1m033[94m12033[0m 033[1m033[94m|033[0m`. The quoted source rule did not
  see a line number, so the line counted as a full disk. The integration tests cannot show it,
  because their `run()` strips `CARGO_TERM_COLOR`.

Re-derive Measured 1 and 2: read the named key from `mutants.out/schemata.json`, or sum
`phase_results[].duration` by phase in `outcomes.json` for the mutants named in
`fallback_mutants`. Re-derive Measured 6, 7 and 9: the probe scripts mutate one line, time the
cargo command above with each setting, then restore the line. Re-derive Measured 12: run the
command above with each environment, then read `schemata.json` and `outcomes.json`.

## Approach

**A command that cargo-mutants runs in a build directory it owns does not inherit the two global
incremental switches, `CARGO_INCREMENTAL` and `CARGO_BUILD_INCREMENTAL`.** Then cargo's config
files and the profile decide (Measured 10). A profile that inherits from `test` or `dev` has
incremental on.

A config file or a profile is a decision about the build, so it is honoured. The two global
switches usually are not: they are set for a whole shell or a whole CI job, for a reason
unrelated to mutation testing. `CARGO_BUILD_INCREMENTAL` is removed with `CARGO_INCREMENTAL` for
the reason that `build_dir_cargo_env` treats `CARGO_BUILD_TARGET_DIR` with `CARGO_TARGET_DIR`
(`src/cargo.rs:112`). Either spelling disables incremental mode.

The profile's own environment form, `CARGO_PROFILE_<NAME>_INCREMENTAL`, is not removed. It names
one profile, so whoever sets it has made a decision about that profile.

With `--in-place` the build directory is the user's own tree, so their environment stands. This
follows the existing rule for `CARGO_TARGET_DIR`, which `build_dir_cargo_env` overrides in
scratch directories and keeps in place.

**Removing, not setting, and only a value that turns incremental off.** Setting
`CARGO_INCREMENTAL=1` would override a profile's `incremental = false` without saying so.
Removing the global switches leaves the config and the profile in charge. A switch that turns
incremental *on* stays: removing it would turn incremental off for a profile that says
`incremental = false` (Measured 14). Cargo reads `CARGO_INCREMENTAL` as on only when it is `1`,
and `CARGO_BUILD_INCREMENTAL` as on only when it is `true`. Any other value of either is removed.

**The cost is disk, and it lands on runs that turned incremental off to save disk.** A
`backend-core` run with `-j2` peaks at about 3.4 GB more (Measured 8). A classic `-jN` run adds
about N × 1.5 GB. CI images commonly export `CARGO_INCREMENTAL=0`: the fork's own CI does
(`.github/workflows/tests.yml:39`), and so does `Swatinem/rust-cache`. Three controls answer this:
- The run prints one line on the console when a removal takes effect. It names the variable and
  says how to keep incremental off.
- The opt-out is one of: `incremental = false` in the mutation profile,
  `CARGO_PROFILE_<NAME>_INCREMENTAL=false` in the environment, or `build.incremental = false` in
  cargo config. The book and `NEWS.md` name all three.
- Seeding stops copying the cache, below.

**The coverage collector's build stays non-incremental.** It runs once per run, so incremental
mode gains it nothing and costs about 0.8 GB and about 17% on a cold build (adversarial review,
two cold builds of the 28 rustc commands each way). The collector already adds its own variables
to the environment (`collect.rs:391-404`). It adds `CARGO_INCREMENTAL=0` there too, and `set`
wins over `remove`.

**Seeding a build directory skips the incremental cache.** `copy_target_dir` copies the baseline
build's `target/` into each extra build directory and into the coverage copy. A seeded directory
does not reuse a copied cache (Measured 9), so copying it spends disk for nothing. The copy leaves
out a directory named `incremental` only when a `.fingerprint` directory sits beside it. That is
cargo's layout for a profile's output directory, at any depth, and it excludes an `incremental`
directory that a test creates under `target/tmp/`. Fingerprints do not refer to the incremental
directory: the adversarial review deleted it and cargo still reported the crate `Fresh`.
`copy_tree`'s `copy_target` option, which copies the user's own `target/` into build_dir_0, skips
it by the same rule.

**A check or build that fails because the disk is full ends the run with an error.** Today it
makes the mutant `Unviable` (`src/outcome.rs:278`), so a full disk can hide a missed mutant while
a gate that counts misses still passes. This item adds disk use (Measured 8), so it makes that
more likely, and the fix is small. When a check or build phase fails and the output that phase
reports the disk full, the phase returns an error instead of a result (Measured 13 and 14).
The run then stops the way any internal error stops it: the mutant is reverted, `main` returns
the error, and the process exits non-zero. The message names the full disk and the phase's log.
A gate that checks its population against the outcomes then fails on the partial run as well.

**What counts as the disk reporting full.** The text of a source line can hold the same words,
so the check reads only what the toolchain says, never what it quotes:
- A cargo JSON compiler message counts only through its `message` and its children's `message`
  fields, each read line by line. Its `rendered` text and its spans quote source. A child can
  hold the linker's whole output.
- Another JSON line, such as an artifact notice, never counts.
- A plain line loses its ANSI control sequences first, because cargo colors its output under
  `CARGO_TERM_COLOR=always` (Measured 14).
- A plain line counts unless rustc is quoting source on it: a line that, trimmed of leading
  space, starts with `|`, or with digits, one or more spaces, and one of `|`, `-`, `+` or `~`
  followed by a space or the end of the line.
- A line counts when it holds a marker of the platform cargo-mutants runs on. On macOS and
  Linux the markers are `No space left on device` and `(os error 28)` (`ENOSPC`), and `errno=28`
  on a line that also holds `ld:` (the macOS linker). On Windows they are
  `There is not enough space on the disk` and `(os error 112)` (`ERROR_DISK_FULL`, whose text
  is localized but whose code is not). The same rule applies to a plain line and to each line
  of a JSON `message` field.
- **This rule read every platform's markers on every platform, and the linker rule only on plain
  lines. Both were wrong.** Linux's `(os error 112)` is `EHOSTDOWN`, measured in a debian
  container, so a host that is down read as a full disk. With `--message-format=json`, which
  the schema's build uses, rustc puts the linker's note in a child `message`, so a full disk at
  link time made every embedded mutant fall back as an unattributed compile error.

**One worker's disk-full error stops the others.** A worker that gets the error empties the
shared queue before it returns, so every other worker finishes the mutant it holds and takes no
other. Without this, the run tests every remaining mutant first and fails anyway (Measured 14).

The check covers the classic lab's check and build phases (`run_cargo`) and the schemata runner's
check and build steps (`Runner::run_step` with `Phase::Check` or `Phase::Build`). This section said
it also covered the coverage build, which was wrong: that build runs through `run_step` as
`Phase::Test`. A failed coverage build turns test selection off. It does not change a mutant's
outcome. The check does not cover the test phase. A test's own output can carry that text, for example a test of
disk-full handling, and a test that fails is a caught mutant by definition.

**The first fallback build in each build directory is a full build of the workspace packages.**
Measured 9 shows it for a seeded directory. build_dir_0's cache was built from the schema's
source, which differs from the restored original in most functions. Later builds in a directory
are incremental. With `--jobs 2` that is two cold builds per run.

Tests that cargo runs also stop seeing the two global switches.

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

**`build_dir_cargo_env` returns an `Env`.** `remove` holds each of `CARGO_INCREMENTAL` and
`CARGO_BUILD_INCREMENTAL` whose inherited value turns incremental off, when the build dir is not
in place. It is empty in place. The decision reads the environment through a function argument,
as `EnvSettings::parse` does in `src/schemata/mod.rs`, so tests do not change the process
environment. The schemata runner's `cargo_env` adds its variables to `set` and passes `remove`
through unchanged. The coverage collector does the same, and its `set` includes
`CARGO_INCREMENTAL=0`.

**`copy_target_dir` and `copy_tree`'s `copy_target` skip an `incremental` directory that has a
`.fingerprint` sibling.** Nothing else about either copy changes.

**One function decides whether a failed phase ran out of disk.** It takes the text the phase
wrote to its log and applies the rule in Approach line by line. It reads JSON compiler messages
through the existing `serde_json` parsing style of `src/schemata/diagnostics.rs`. cargo-mutants'
own source holds the markers as literals, and its CI runs cargo-mutants on itself, so the quoted
source rule is not optional.

**A worker thread in `run_mutants` empties the queue on any error**, under the queue's lock, then
returns the error. That covers a failed copy of the workspace as well as a failed scenario.
**This said `Worker::run_queue` emptied it, which was wrong:** a copy that failed because the disk
was full returned before `run_queue` ran, and left the queue full. `run_cargo` and
`Runner::run_step` call it only for a failed check or build phase, and return an error that names
the disk and the log path. The text of a phase is what it appended to the scenario's log, not the
whole log, so an earlier phase's output cannot match.

**Environment overrides are reported once per run, never per command.** `build_dir_cargo_env`
runs for every spawned process, including every replayed test command. Its existing
`CARGO_TARGET_DIR` event already writes one line per process: 6,228 in the run of Measured 1
(Measured 11). That event moves into the once-per-run report with the new one, while there are
two sites.
- At the start of a run that uses scratch build directories, one debug event names each
  overridden or removed variable that was set, with its inherited value.
- When `CARGO_INCREMENTAL` or `CARGO_BUILD_INCREMENTAL` is removed, one console line at info
  level names it with its value, says that cargo config and the profile now decide, and names
  the opt-outs. It does not say the builds are incremental: a profile can still turn that off.
- `schemata.json` gets `removed_env`, a map from each removed variable that was set to its
  inherited value, for example `{"CARGO_INCREMENTAL": "0"}`. It is empty when nothing was set or
  the run is in place. Measured 5 needed a grep through mutant logs to find the cause. This key
  replaces that.

The plan fixes the type's exact name and visibility, and the call site of the once-per-run report.
The contract is the two halves, the removal order, the two variable names, the copy rule, the
three reports and the disk-full stop.

## Acceptance criteria

1. **Unit tests in `cargo.rs`**, beside
   `build_dir_cargo_env_sets_cargo_target_dir_to_own_target_except_in_place`: `remove` names both
   variables for a scratch build dir and is empty with `--in-place`. **In `process.rs`**: a
   variable that cargo-mutants' environment holds and that only `remove` names is absent in the
   child. A variable in both `remove` and `set` reaches the child with the `set` value.
2. **Unit tests in `copy_tree.rs`**, beside `copy_target_dir_when_requested`: an `incremental`
   directory beside a `.fingerprint` directory is not copied, at `target/debug/` and at
   `target/<triple>/debug/`. A file beside each is copied. An `incremental` directory with no
   `.fingerprint` sibling, such as `target/tmp/x/incremental/`, is copied. The same holds for
   `copy_tree` with `copy_target`.
3. **Integration tests in `tests/main.rs`, one per path**, following the two tests of the
   `CARGO_TARGET_DIR` change at `tests/main.rs:5681` (`--no-schemata -j2`) and `:5712`
   (`--schemata`). Each runs on a small testdata tree with `CARGO_INCREMENTAL=0` and
   `CARGO_BUILD_INCREMENTAL=false` in the environment. The classic test asserts that a mutant's
   log has a rustc line for the mutated crate carrying `-C incremental=`. The schemata test
   asserts the same of a `schemata-build` log, and that `schemata.json` `removed_env` names both
   variables with their values. A third test runs with
   `CARGO_PROFILE_<NAME>_INCREMENTAL=false` as well and asserts no `-C incremental=`. Watch each
   fail on the current code before the change.
4. **A full disk stops the run, and nothing else does.**
   - Unit tests of the detector. Each marker of Measured 13 and 14 matches, including the macOS
     linker's note and a JSON compiler message whose own `message` is rustc's ENOSPC text. A
     compile error that does not mention the disk does not match, nor does empty text. Neither
     does a quoted source line in each form: a `NN |` line, a `NN -` suggestion line, a colored
     `NN |` line, and a JSON compiler message that holds the marker only in its `rendered` text and spans.
   - Integration tests on a testdata tree with several functions. Its build script prints
     `No space left on device` and fails only when one mutation is present. Run with
     `--no-schemata -j2 --no-shuffle` and with `--schemata`. cargo-mutants exits non-zero, its
     output names the disk, and `unviable.txt` is empty. With `-j2`, `outcomes.json` holds far
     fewer mutants than the tree has, so the second worker stopped.
   - An integration test on a tree whose source holds `No space left on device` as a string
     literal, run with `--schemata`, finishes normally.
   - Watch each fail on the current code first.
5. **The same mutants get the same outcomes, and the builds are incremental whatever the shell
   exports.** In `jast-platform` on `main`, with `just dev-db` and `just dev-minio` up (the
   `pg-tests,s3-tests` features need Postgres and MinIO), run the invocation of Measured 12 in these
   environments:

   | Run | Rev | Environment |
   |---|---|---|
   | A | `2f837e8` | `CARGO_INCREMENTAL=0` exported |
   | B | this item's | `CARGO_INCREMENTAL=0` exported |
   | C | this item's | `CARGO_INCREMENTAL` unset |

   - Every mutant's outcome in `outcomes.json` is identical across A, B and C.
   - Each run tests at least 20 fallback mutants the classic way. Fewer means the comparison
     does not measure this change. Measured 12 found exactly 20.
   - In B and C, every classically built fallback mutant's log carries `-C incremental=` on the
     `backend_core` rustc line. In A, none does.
   - B's `schemata.json` `removed_env` is `{"CARGO_INCREMENTAL": "0"}`. C's is empty.
   - **Speed, measured as Measured 12 does.** Run A, B, A, B back to back, with no other build
     on the machine if possible, and record the load average after each run. The B runs'
     summed `fallback_wall_seconds` is at most 0.75 of the A runs' sum. Measured 12 gave 0.57
     under load. A ratio above 0.75 with every deterministic check passing means a removal path
     was missed, or load hid the gain. Rerun the pair before concluding which.
6. `cargo test --all-features` and `cargo clippy --all-targets --all-features -- -D warnings`
   pass in the fork. `cargo fmt` is clean.
7. `NEWS.md` and `book/src/build-dirs.md` say that a scratch build dir ignores the two global
   switches, and why. They say that `--in-place`, cargo config, the profile and
   `CARGO_PROFILE_<NAME>_INCREMENTAL` are honoured. They state the disk cost and the three
   opt-outs, with a note for CI users, and that seeding skips the incremental cache. `NEWS.md`
   also says that a check or build that runs out of disk now stops the run.

## Out of scope

| Item | Why not now | What brings it back |
|---|---|---|
| Row 1: build only the targets of the tests that reach a fallback mutant, widen when they pass | Measured 6 and 7 with incremental on: it saves about 4 to 5 s per caught mutant and costs about 7 s per missed one. It can also report a mutant caught that the classic way reports unviable, when the mutant breaks only a target outside the narrow set | a re-measured run after this item where caught fallback builds still take a large share of `fallback_wall_seconds` |
| Row 2: handle `const`-context mutants differently, for example by checking them before building | Measured 6: with incremental on, an unviable `const` mutant fails in about 3.6 s. A viable one costs the same as any other fallback build | the same re-measure, with `const_context` still the largest reason in `fallback_time_by_reason` |
| A test phase that fails because the disk is full is reported `CaughtMutant` | A test's own output can carry the same text, so matching it there would misreport real catches. The build phases cover most of the disk this item adds. This item raises the odds of the test-phase case too, because the disk is fuller during the run | a caught mutant whose log shows a disk-full error from the test harness itself |
| An incremental-only compiler error, such as "found unstable fingerprints", is reported `Unviable` | Upstream builds incremental by default, so the risk is not new to cargo-mutants. No case is known | an unviable mutant whose log has `internal compiler error` |
| `build.build-dir` (or `CARGO_BUILD_BUILD_DIR`) set to an absolute path moves `deps/` and `incremental/` out of each scratch `target/`, so build dirs share intermediate files | True today. The adversarial review demonstrated the layout, not a collision | a user report, or a probe that shows two build dirs colliding |
| Bumping the fork's rev in `jast-platform` | That repository pins the rev in its `Justfile` (`just _mutants-tool`) | this item merging. It is a one-line change there |

## Dependencies

None in this repository. `jast-platform`'s `mutants-test-waits` item cuts the embedded-test half of
the same run. The two are independent.
