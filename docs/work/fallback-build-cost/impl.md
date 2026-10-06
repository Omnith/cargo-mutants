# fallback-build-cost: what shipped

Fallback mutants now build incrementally whatever the shell exports. On `jast-platform`'s
`backend-core` the fallback phase took 0.455 of its old time (B1 + B2 against A1 + A2, below).
Every outcome was the same. A full disk now stops the run.

## What shipped

Branch `feat/fbc-1-incremental-scratch-dirs`, version `27.1.0+omnith.2`.

| Change | Commit |
|---|---|
| `Env { remove, set }` in `src/process.rs`. `Process::run` and `Process::start` take it. `remove` applies first, so `set` wins | `85e9f7e` |
| Scratch build dirs remove `CARGO_INCREMENTAL` and `CARGO_BUILD_INCREMENTAL` when the value turns incremental off. `--in-place` keeps them. The coverage build sets `CARGO_INCREMENTAL=0` | `c7f8fd4` |
| Seeding a build dir, and `copy_target`, skip an `incremental` directory beside a `.fingerprint` directory | `2136719` |
| The overrides are reported once per run: one console line, one `build_dirs.env_overrides` debug event, and `removed_env` in `schemata.json` | `412abe4` |
| A check or build whose output says the disk is full returns an error. The worker that sees it empties the queue. New trees `testdata/disk_full_build` and `testdata/disk_full_literal` | `2558cc8` |
| `NEWS.md`, `book/src/build-dirs.md` and `book/src/schemata.md` describe both changes. Version bump | `0398944` |
| Plan and design corrections from execution, listed under Execution findings | `ae00f8b`, `91426b9`, `e808677` |

## Acceptance runs

Design Acceptance criterion 5, run as plan Task 7 on 2026-10-05, 19:42 to 20:26 local time.
`schemata.json` records the same runs as `start_time` 01:42Z to 02:18Z on 2026-10-06.

Setup:
- A `jast-platform` worktree detached at `aadd3fe8`, the rev of design Measured 12.
- Its own Postgres and MinIO on ports 55436 and 59006, compose project `rbp-fbc`.
- The old binary is rev `2f837e8`, `27.1.0+omnith.1`, built in release mode from a worktree.
- The new binary is `0398944`, `27.1.0+omnith.2`, built in release mode.
- Each binary was run by path. No `cargo install`.
- Another session's builds ran on the machine throughout. The load average stayed between 9 and 18.

Each run, in the `jast` worktree:

```
JAST_DB_PORT=55436 JAST_MINIO_PORT=59006 <env> <binary> mutants \
  -p backend-core --features pg-tests,s3-tests -j2 \
  --file apps/backend-core/src/config.rs --file apps/backend-core/src/adapters/s3_object_store.rs \
  --output <scratch>/out-<run>
```

| Run | Binary | `<env>` | `wall_seconds` | `fallback_wall_seconds` | classic fallback mutants | classic build median | outcomes | load after (1, 5, 15 min) |
|---|---|---|---|---|---|---|---|---|
| A1 | old | `CARGO_INCREMENTAL=0` | 596.7 | 298.0 | 20 | 25.5 s | 280 caught, 57 unviable | 14.29, 12.26, 9.04 |
| B1 | new | `CARGO_INCREMENTAL=0` | 448.3 | 142.8 | 20 | 4.9 s | 280 caught, 57 unviable | 12.16, 11.20, 9.50 |
| A2 | old | `CARGO_INCREMENTAL=0` | 631.4 | 338.5 | 20 | 30.5 s | 280 caught, 57 unviable | 18.21, 15.12, 11.93 |
| B2 | new | `CARGO_INCREMENTAL=0` | 455.0 | 146.9 | 20 | 9.9 s | 280 caught, 57 unviable | 10.67, 11.58, 11.35 |
| C | new | `env -u CARGO_INCREMENTAL` | 450.2 | 138.0 | 20 | 6.1 s | 280 caught, 57 unviable | 14.90, 11.48, 11.24 |

Every run had 337 mutants: 272 embedded and 65 fallback. Of the 65, the check pass proved 45
unviable without a build. The other 20 were built and tested the classic way.

Re-derive each column from a run's `mutants.out/`:

```
jq -c '{wall_seconds, fallback_wall_seconds, removed_env}' schemata.json
jq '[.fallback_mutants[] | select(.proven_unviable | not)] | length' schemata.json
jq -r '.outcomes[] | select(.scenario | type == "object" and has("Mutant")) | "\(.scenario.Mutant.name)\t\(.summary)"' outcomes.json | sort
```

The classic build median is the median `duration` of the `Build` phase in `phase_results`, over
the 20 classic mutants.

## Acceptance criteria

| # | Criterion | Evidence, 2026-10-05 | Result |
|---|---|---|---|
| 1 | `remove` names both switches for a scratch build dir and is empty in place. A child does not see a removed variable, and sees the `set` value of one both removed and set | unit tests `incremental_switches_that_turn_incremental_off_are_removed_except_in_place`, `incremental_switches_that_turn_incremental_on_or_are_unset_are_kept`, `child_does_not_see_a_variable_that_remove_names`, `child_sees_the_set_value_of_a_variable_that_is_removed_and_set`, `child_inherits_a_variable_that_remove_does_not_name` | pass |
| 2 | Seeding and `copy_target` skip an `incremental` directory beside `.fingerprint`, and copy one without it | `copy_target_dir_skips_incremental_caches_beside_fingerprints`, `copy_tree_with_copy_target_skips_incremental_caches_beside_fingerprints` | pass |
| 3 | Classic and schemata builds carry `-C incremental=` with both switches exported. `removed_env` names both. A profile switch keeps incremental off | `incremental_switches_from_environment_are_not_inherited_by_build_dirs`, `incremental_switches_from_environment_are_not_inherited_by_schemata_build`, `incremental_off_in_the_profile_is_honoured_by_build_dirs`, `coverage_build_is_not_incremental_in_test_selection_coverage_tree` | pass |
| 4 | A full disk stops the run on both paths and empties the queue. Quoted source, including a colored line and JSON `rendered` text, does not stop it | six `ran_out_of_disk_*` unit tests, `a_build_that_runs_out_of_disk_stops_the_run_in_disk_full_build_tree_with_schemata`, `..._without_schemata`, `source_holding_the_disk_full_message_does_not_stop_the_run_in_disk_full_literal_tree` | pass |
| 5a | Every mutant's outcome is identical across all five runs | the five sorted `name<TAB>summary` lists have one SHA-1, and `diff` of A1 against each other run prints nothing | pass |
| 5b | Each run tests at least 20 fallback mutants the classic way | 20 in each run, from the `jq` count above | pass |
| 5c | In B1, B2 and C every classic log has `-C incremental=` on the `backend_core` rustc line. In A1 and A2 none has | B1 20 of 20 logs (68 of 68 `--crate-name backend_core` rustc lines), B2 20 of 20 (69 of 69), C 20 of 20 (68 of 68). A1 0 of 20 (0 of 86 lines), A2 0 of 20 (0 of 92). No A log anywhere in `log/` holds `-C incremental=` | pass |
| 5d | `removed_env` is `{"CARGO_INCREMENTAL": "0"}` in B1 and B2, and `{}` in C | the `jq` line above. B1's console printed the removal line once. C's printed none. A1 and A2 have no such key | pass |
| 5e | B1 + B2 `fallback_wall_seconds` is at most 0.75 of A1 + A2 | 289.8 s against 636.5 s, a ratio of 0.455. Per pair: B1/A1 0.48, B2/A2 0.43. No rerun was needed | pass |
| 6 | `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings` and the suite pass | at `0398944`: `cargo fmt --check` exit 0, clippy clean, `cargo nextest run --all-features` gave `664 tests run: 664 passed (2 slow), 3 skipped` | pass |
| 7 | `NEWS.md` and the book state the removal, what is honoured, the cost, the opt-outs, the CI note, the seeding rule and the disk-full stop | `NEWS.md` `## Unreleased` Changed and Fixed bullets. `book/src/build-dirs.md` `## Incremental compilation` and the seeding section. `book/src/schemata.md` names `removed_env` | pass |

The 5c count of `--crate-name backend_core` lines differs between runs of the same mutants. A
caught mutant's log has 4 in every run. An unviable mutant's build stops when a target fails, and
its log has 3 to 5. Re-derive 5c with
`grep -l -E -- '--crate-name backend_core .*-C incremental=' log/*.log | wc -l`. It also counts
three `schemata-build` logs, so it gives 23 for B1, B2 and C and 0 for A1 and A2.

## Execution findings

Batches A to C folded each finding into the plan or the design before the next batch.

| Finding | Folded into | Commit |
|---|---|---|
| Task 2 Step 3 expected 6 tests to run and Task 4 Step 4 expected 3. Both were wrong: a unit test that does not compile stops the whole build, so nextest runs none. `--test main` shows the integration tests' RED | plan Tasks 2 and 4 | `ae00f8b` |
| `testdata/disk_full_build/build.rs` matched `x + 2` in raw source. The classic way writes `x + /* ~ changed by cargo-mutants ~ */ 2`, so the trigger never fired without schemata: `22 mutants tested: 2 missed, 20 caught`. The script now strips the marker | plan Task 5, `testdata/disk_full_build/build.rs` | `91426b9`, `2558cc8` |
| `disk_full_literal` has two unviable mutants, not one: `+` mutates to `-` and to `*`, and neither compiles on a `String`. Both paths gave `4 mutants tested: 2 caught, 2 unviable` | plan Task 5, the tree's description and test | `91426b9`, `2558cc8` |
| A colored quoted source line starts with an escape sequence, not its line number, so it counted as a full disk under `CARGO_TERM_COLOR=always`. A plain line now loses its ANSI sequences first. Ninth detector test added | design Measured 14 and Approach, plan Task 5 Step 6 | `e808677`, `2558cc8` |
| The plan's Changed text said a scratch build dir drops both switches. The shipped `NEWS.md` and book say it drops a switch only when its value turns incremental off, and keeps `CARGO_INCREMENTAL=1` and `CARGO_BUILD_INCREMENTAL=true`. That is the rule `incremental_switches_to_remove` in `src/cargo.rs` applies | `NEWS.md`, `book/src/build-dirs.md` | `0398944` |
