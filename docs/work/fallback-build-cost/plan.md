# fallback-build-cost implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development`
> (recommended) or `superpowers:executing-plans` to implement this plan batch by batch. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** fallback mutants build incrementally whatever the shell exports. A full disk stops the
run instead of hiding a missed mutant.

**Architecture:** child processes take an `Env { remove, set }` from `process.rs`.
`build_dir_cargo_env` removes the two global incremental switches in scratch build dirs. Seeding
skips incremental caches. The overrides are reported once per run. `run_cargo` and
`Runner::run_step` turn a failed check or build whose output says the disk is full into an error.

**Tech stack:** Rust 2024, cargo-mutants' own test harness (`rusty-fork`, `assert_cmd`, testdata
trees), `cargo-nextest`.

The design is `design.md` in this directory. It says what and why. This plan says in what order.
Test bodies and doc comments below are the contract and stay verbatim. Implementation is given as
intent and pointers. The compiler and `maintainable-rust` produce the code.

---

## Execution rules (every batch)

**Skills to invoke at the start of each batch:** `superpowers:using-superpowers`,
`superpowers:subagent-driven-development`, `superpowers:test-driven-development`,
`superpowers:verification-before-completion`, `skills-dev:maintainable-rust`,
`skills-dev:api-and-interface-design`, `skills-dev:observability-and-instrumentation`.

**Where.** The orchestrator cuts the worktree before Batch A and runs `graft build` in it:

```
git -C ~/repos/cargo-mutants worktree add ~/repos/cargo-mutants-wt-fbc -b feat/fbc-1-incremental-scratch-dirs main
cd ~/repos/cargo-mutants-wt-fbc && graft build
```

- Prefix every Bash call with `cd ~/repos/cargo-mutants-wt-fbc &&`. The shell's working directory
  resets between calls.
- Every cargo command runs with `CARGO_TARGET_DIR=~/repos/cargo-mutants-wt-fbc/target`. Never
  share a target dir with another worktree: cargo can then run the other tree's test binary.
- Discover code with `graft grep "<literal>"`, `graft skeleton <file>`, `graft callers <symbol>`
  and `graft ask "<question>" --source`, each behind the same `cd` prefix.

**Disk.** The disk is shared with other sessions and has filled before.
- Run `df -h ~/repos` before the first build of each batch and before any `cargo test` of the
  whole suite. Stop and report if under 6 GiB is free.
- Delete what you create when you are done with it: scratch copies, extra target dirs, the old-rev
  worktree in Batch D. Report what you deleted.
- Do not delete anything you did not create.

**Git.**
- No tree-wide git: no `stash`, `clean`, `checkout --`, `reset`, `restore`.
- Commit with `git commit -m '<msg>' -- <paths>`. List every path. A new file needs
  `git add <path>` first: `git commit -- <path>` refuses a path git does not know.
- Commit messages follow this repository's own style: a plain sentence in the imperative, for
  example `Remove inherited incremental switches in scratch build dirs`. No Co-Authored-By.

**Gates.**
- `cargo fmt` before each commit.
- `cargo clippy --all-targets --all-features -- -D warnings` before each commit.
- Filtered `cargo nextest run --all-features <filter>` within a task. The whole suite at the end
  of each batch.
- **Quote a filter's expected count and compare it.** A filter that matches nothing passes.
- **Watch every new test fail before you make it pass.** Record the failure line in the batch
  report.

**Tool usage (verbatim from jast-platform's CLAUDE.md):**
- Do not use `cd` commands inline or the `git` command's `-C <dir>` flag when already in the
  command's target directory.
- Never pass git pager flags: no `git -c core.pager=...`, no `--no-pager`, no `GIT_PAGER=`
  overrides. `git commit` invokes no pager and short `git log`/`git status`/`git show --stat`
  calls don't need one; the harness handles output. Inconsistent pager flags also defeat the
  permission allowlist.
- When surfacing a command's exit status, use exactly one canonical form: **`echo "exit:$?"`**.
  Prefer relying on the harness's own exit reporting.

**Batches.** One implementer per batch, dispatched by the orchestrator, which reviews between
batches.

**Before Task 1, record a baseline of the whole suite** at the worktree's starting commit, once
with `CARGO_INCREMENTAL` unset and once with `CARGO_INCREMENTAL=0` exported. Record the pass,
fail and skip counts and the name of every failure. The plan's review saw
`schemata_fallback_in_schemata_tree_uses_seed_target_and_logs_timing` fail 3 of 4 times under
`CARGO_INCREMENTAL=0` before any change (`left: 2, right: 1` seeded dirs, at load 7 to 17). A
later failure that the baseline already shows is not this item's. Say so in the report rather
than fixing it.

| Batch | Tasks | Ends with |
|---|---|---|
| A | 1, 2, 3 | env plumbing, removal, copy rule, their tests green |
| B | 4, 5 | once-per-run report, disk-full stop |
| C | 6 | docs, version, the whole suite, clippy, fmt |
| D | 7 | the jast-platform acceptance runs, `impl.md` |
| E | 8 | pull request, reviews, merge |

---

## File map

| File | Change |
|---|---|
| `src/process.rs` | new `Env`. `Process::run` and `Process::start` take `&Env` |
| `src/cargo.rs` | `build_dir_cargo_env` returns `Env`. New `EnvOverrides` and `env_overrides`. New `ran_out_of_disk`. `run_cargo` stops on a full disk |
| `src/schemata/run.rs` | `Runner::cargo_env`, `test_env`, `run_step`, `run_test_command` carry `Env`. `run_step` stops on a full disk |
| `src/schemata/coverage/collect.rs` | both sites carry `Env`. The instrumented build sets `CARGO_INCREMENTAL=0` |
| `src/schemata/mod.rs` | `Report` gets `removed_env` |
| `src/main.rs` | reports the overrides once, after the debug log opens |
| `src/copy_tree.rs` | `copy_target_dir` and `copy_tree` skip an incremental cache |
| `tests/main.rs` | five integration tests and one helper |
| `testdata/disk_full_build/` | new tree |
| `NEWS.md`, `book/src/build-dirs.md`, `Cargo.toml`, `Cargo.lock` | docs and version |
| `docs/work/fallback-build-cost/impl.md` | new, Batch D |

---

## Task 1: `Env` for child processes (Batch A)

**Files:** modify `src/process.rs`, and every caller of `Process::run`: `src/cargo.rs`
`run_cargo`, `src/schemata/run.rs` (`run_step`, `run_test_command`, the `cargo test` path near
`:999`), `src/schemata/coverage/collect.rs` (`:405` through `run_step`, `:512` directly).

- [x] **Step 1: write the failing tests** at the end of `src/process.rs`, beside `mod test`.

```rust
#[cfg(all(test, unix))]
mod env_test {
    use std::fs::read_to_string;

    use camino::Utf8Path;
    use rusty_fork::rusty_fork_test;

    use super::{Env, Process};
    use crate::console::Console;
    use crate::output::OutputDir;
    use crate::test_util::single_threaded_set_env_var;

    const NAME: &str = "CARGO_MUTANTS_TEST_INHERITED";

    /// The value of [`NAME`] that a child process sees, or `unset`.
    fn child_sees(env: &Env) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = Utf8Path::from_path(tmp.path()).unwrap();
        let mut output_dir = OutputDir::new(tmp_path).unwrap();
        let mut log = output_dir.start_log("env").unwrap();
        let argv = ["sh", "-c", "echo \"seen=${CARGO_MUTANTS_TEST_INHERITED-unset}\""]
            .map(str::to_owned);
        let exit = Process::run(
            &argv,
            env,
            tmp_path,
            None,
            None,
            &mut log,
            &Console::new(),
            None,
        )
        .unwrap();
        assert!(exit.is_success(), "{exit:?}");
        let text = read_to_string(output_dir.path().join(log.log_path())).unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix("seen="))
            .expect("the child printed what it saw")
            .to_owned()
    }

    rusty_fork_test! {
        #[test]
        fn child_inherits_a_variable_that_remove_does_not_name() {
            single_threaded_set_env_var(NAME, "inherited");
            assert_eq!(child_sees(&Env::default()), "inherited");
        }

        #[test]
        fn child_does_not_see_a_variable_that_remove_names() {
            single_threaded_set_env_var(NAME, "inherited");
            let env = Env {
                remove: vec![NAME.to_owned()],
                set: Vec::new(),
            };
            assert_eq!(child_sees(&env), "unset");
        }

        #[test]
        fn child_sees_the_set_value_of_a_variable_that_is_removed_and_set() {
            single_threaded_set_env_var(NAME, "inherited");
            let env = Env {
                remove: vec![NAME.to_owned()],
                set: vec![(NAME.to_owned(), "set".to_owned())],
            };
            assert_eq!(child_sees(&env), "set");
        }
    }
}
```

The first test is the presence half. Without it, the second passes on a child that sees nothing
at all.

- [x] **Step 2: run them and watch them fail to compile on the missing `Env`.**

```
cargo nextest run --all-features -E 'test(/env_test::/)'
```

Expected: error `cannot find type Env`, or `unresolved import super::Env`.

- [x] **Step 3: add `Env` to `src/process.rs`**, above `pub struct Process`.

```rust
/// Environment changes for a child process, relative to cargo-mutants' own environment.
///
/// `remove` is applied before `set`, so a variable named in both reaches the child with
/// its `set` value. `remove` is for values inherited from cargo-mutants' environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// Variables inherited from cargo-mutants' environment that the child must not see.
    pub remove: Vec<String>,
    /// Variables to set.
    pub set: Vec<(String, String)>,
}
```

INTENT: `Process::run` and `Process::start` take `env: &Env` in place of
`env: &[(String, String)]`. In `start`, call `command.env_remove(name)` for each `remove` entry,
then `command.envs(...)` over `set`, before `stdin`. Keep the debug span unchanged.

- [x] **Step 4: change every caller to build an `Env`**, with `set` holding what it passed before
  and `remove` empty. Behaviour is unchanged in this task. Callers:
  - `run_cargo` passes `&Env { set: build_dir_cargo_env(...), remove: Vec::new() }` for now.
  - `Runner::cargo_env` and `Runner::test_env` keep returning `Vec<(String, String)>` for now.
    `run_step` and `run_test_command` wrap theirs. Task 2 changes the return types.
  - The `cargo test` path in `run.rs` near `:999` passes `&self.test_env(id)`. Wrap it the same
    way.
  - `collect.rs` `:512` wraps its `env`.

- [x] **Step 5: run the three tests and watch them pass.**

```
cargo nextest run --all-features -E 'test(/env_test::/)'
```

Expected: 3 passed.

- [x] **Step 6: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Let a child process have inherited variables removed' -- src/process.rs src/cargo.rs src/schemata/run.rs src/schemata/coverage/collect.rs
```

---

## Task 2: remove the incremental switches in scratch build dirs (Batch A)

**Files:** modify `src/cargo.rs` (`build_dir_cargo_env` `:105-123`, its test `:262-280`,
`run_cargo`), `src/schemata/run.rs` (`cargo_env` `:245`, `test_env` `:1015`, `run_step` `:260`,
`run_test_command` `:669`), `src/schemata/coverage/collect.rs` (`:391-404`, `:501-505`),
`tests/main.rs`.

- [x] **Step 1: write the failing unit tests** in `src/cargo.rs`'s `mod test`, beside
  `build_dir_cargo_env_sets_cargo_target_dir_to_own_target_except_in_place`.

```rust
#[test]
fn incremental_switches_that_turn_incremental_off_are_removed_except_in_place() {
    let off = |name: &str| match name {
        "CARGO_INCREMENTAL" => Some("0".to_owned()),
        "CARGO_BUILD_INCREMENTAL" => Some("false".to_owned()),
        _ => None,
    };
    assert_eq!(
        incremental_switches_to_remove(&Options::default(), off),
        ["CARGO_INCREMENTAL", "CARGO_BUILD_INCREMENTAL"]
    );
    let in_place = Options {
        in_place: true,
        ..Options::default()
    };
    assert_eq!(
        incremental_switches_to_remove(&in_place, off),
        Vec::<String>::new()
    );
}

/// A switch that turns incremental on stays: removing it would turn incremental off for a
/// profile that says `incremental = false`.
#[test]
fn incremental_switches_that_turn_incremental_on_or_are_unset_are_kept() {
    let on = |name: &str| match name {
        "CARGO_INCREMENTAL" => Some("1".to_owned()),
        "CARGO_BUILD_INCREMENTAL" => Some("true".to_owned()),
        _ => None,
    };
    assert_eq!(
        incremental_switches_to_remove(&Options::default(), on),
        Vec::<String>::new()
    );
    assert_eq!(
        incremental_switches_to_remove(&Options::default(), |_| None),
        Vec::<String>::new()
    );
}
```

The existing test reads the set half: change its `.into_iter()` to `.set.into_iter()`. It
asserts the same values as before.

- [x] **Step 2: write the failing integration tests** in `tests/main.rs`, after
  `cargo_target_dir_from_environment_is_not_used_by_schemata_build`.

```rust
/// The rustc command lines in a log that compile `crate_name`.
fn rustc_lines<'a>(log: &'a str, crate_name: &str) -> Vec<&'a str> {
    let flag = format!("--crate-name {crate_name} ");
    log.lines()
        .filter(|line| line.contains("Running `") && line.contains(&flag))
        .collect()
}

/// The text of every mutant's log in `mutants_out`: every log but the baseline's.
fn mutant_logs(mutants_out: &Path) -> Vec<String> {
    read_dir(mutants_out.join("log"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("baseline")
        })
        .map(|path| read_to_string(path).unwrap())
        .collect()
}

/// With incremental compilation turned off in the environment, as a CI job or a shell
/// might turn it off to save disk, each scratch build dir still builds incrementally:
/// it rebuilds the mutated package once per mutant. With `-j2` the second build dir is
/// seeded from the first, so the seeded copy is covered too.
#[test]
fn incremental_switches_from_environment_are_not_inherited_by_build_dirs() {
    let tmp = copy_of_testdata("small_well_tested");
    let out = tempdir().unwrap();
    run()
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_BUILD_INCREMENTAL", "false")
        .args(["mutants", "--no-times", "--no-schemata", "-j2", "-d"])
        .arg(tmp.path())
        .arg("-o")
        .arg(out.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .success();
    let logs = mutant_logs(&out.path().join("mutants.out"));
    assert!(logs.len() > 1, "{logs:?}");
    for log in &logs {
        let lines = rustc_lines(log, "cargo_mutants_testdata_small_well_tested");
        assert!(!lines.is_empty(), "the mutant was built: {log}");
        assert!(
            lines.iter().all(|line| line.contains("-C incremental=")),
            "{lines:?}"
        );
    }
}

#[test]
fn incremental_switches_from_environment_are_not_inherited_by_schemata_build() {
    let tmp = copy_of_testdata("small_well_tested");
    let out = tempdir().unwrap();
    run()
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_BUILD_INCREMENTAL", "false")
        .args(["mutants", "--no-times", "--schemata", "-d"])
        .arg(tmp.path())
        .arg("-o")
        .arg(out.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .success();
    let log = read_to_string(out.path().join("mutants.out/log/schemata-build-1.log")).unwrap();
    let lines = rustc_lines(&log, "cargo_mutants_testdata_small_well_tested");
    assert!(!lines.is_empty(), "the schema was built: {log}");
    assert!(
        lines.iter().all(|line| line.contains("-C incremental=")),
        "{lines:?}"
    );
}

/// Turning incremental compilation off for the profile, rather than for everything, is a
/// decision about the build, so it holds in scratch build dirs too.
#[test]
fn incremental_off_in_the_profile_is_honoured_by_build_dirs() {
    let tmp = copy_of_testdata("small_well_tested");
    let out = tempdir().unwrap();
    run()
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_PROFILE_DEV_INCREMENTAL", "false")
        .env("CARGO_PROFILE_TEST_INCREMENTAL", "false")
        .args(["mutants", "--no-times", "--no-schemata", "-d"])
        .arg(tmp.path())
        .arg("-o")
        .arg(out.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .success();
    let logs = mutant_logs(&out.path().join("mutants.out"));
    assert!(logs.len() > 1, "{logs:?}");
    for log in &logs {
        let lines = rustc_lines(log, "cargo_mutants_testdata_small_well_tested");
        assert!(!lines.is_empty(), "the mutant was built: {log}");
        assert!(
            lines.iter().all(|line| !line.contains("-C incremental=")),
            "{lines:?}"
        );
    }
}

/// The coverage build runs once per run, so incremental compilation gains it nothing and
/// only adds a cache: it stays off there, while the schema's own build is incremental.
#[test]
fn coverage_build_is_not_incremental_in_test_selection_coverage_tree() {
    if !llvm_tools_available("coverage_build_is_not_incremental_in_test_selection_coverage_tree") {
        return;
    }
    let tmp = copy_of_testdata("test_selection_coverage");
    let out = tempdir().unwrap();
    run()
        .env_remove("CARGO_INCREMENTAL")
        .env_remove("CARGO_BUILD_INCREMENTAL")
        .args([
            "mutants",
            "--no-times",
            "--schemata",
            "--test-selection=coverage",
            "-d",
        ])
        .arg(tmp.path())
        .arg("-o")
        .arg(out.path())
        .timeout(std::time::Duration::from_secs(600))
        .assert()
        .code(2); // Some mutants are missed.
    let log = |name: &str| read_to_string(out.path().join("mutants.out/log").join(name)).unwrap();
    let crate_name = "cargo_mutants_testdata_test_selection_coverage";
    let schema_build = log("schemata-build-1.log");
    let schema_lines = rustc_lines(&schema_build, crate_name);
    assert!(!schema_lines.is_empty(), "{schema_build}");
    assert!(
        schema_lines
            .iter()
            .all(|line| line.contains("-C incremental=")),
        "{schema_lines:?}"
    );
    let coverage_build = log("schemata-coverage-list.log");
    let coverage_lines = rustc_lines(&coverage_build, crate_name);
    assert!(!coverage_lines.is_empty(), "{coverage_build}");
    assert!(
        coverage_lines
            .iter()
            .all(|line| !line.contains("-C incremental=")),
        "{coverage_lines:?}"
    );
}
```

- [x] **Step 3: watch them fail.**

```
cargo nextest run --all-features -E 'test(/incremental_switches_that_turn/) | test(/incremental_switches_from_environment/) | test(/incremental_off_in_the_profile/) | test(/coverage_build_is_not_incremental/)'
```

Expected: none run. This said "6 run", which was wrong: a unit test that does not compile stops
the whole build, so nextest runs nothing. Run the same filter with `--test main` added to see the
four integration tests' RED while the unit tests do not compile.
- The unit tests fail to compile on the missing `incremental_switches_to_remove`. Record the
  error.
- `incremental_switches_..._build_dirs` and `..._schemata_build` fail on the `-C incremental=`
  assertion.
- `incremental_off_in_the_profile_...` passes today, because nothing is removed yet. It is the
  guard for the opt-out, so this pass is expected. Say so in the report.
- `coverage_build_is_not_incremental_...` fails on the coverage assertion. With both switches
  unset, the profile turns incremental on for the coverage build as well as the schema's. If
  llvm-tools is missing it prints `SKIPPED` and proves nothing. Install them with
  `rustup component add llvm-tools` before you run it.

- [x] **Step 4: implement.**

In `src/cargo.rs`, beside `build_dir_cargo_env`:

```rust
/// Variables that switch incremental compilation for every cargo command, each with the
/// one value that turns it on. Cargo reads `CARGO_INCREMENTAL` first, then
/// `build.incremental`, whose environment form is `CARGO_BUILD_INCREMENTAL`, then the
/// profile. Measured in `docs/work/fallback-build-cost/design.md`, Measured 10.
const INCREMENTAL_SWITCHES: [(&str, &str); 2] =
    [("CARGO_INCREMENTAL", "1"), ("CARGO_BUILD_INCREMENTAL", "true")];

/// The incremental switches in `var` that a scratch build dir removes: those set to
/// anything that turns incremental off. None in place.
///
/// A switch that turns incremental on stays, because removing it would turn incremental
/// off for a profile that says `incremental = false` (Measured 14).
///
/// `var` reads one variable, so that tests don't change the process environment.
```

INTENT: `pub(crate) fn incremental_switches_to_remove(options: &Options, var: impl Fn(&str) ->
Option<String>) -> Vec<String>`. Empty in place. Otherwise each switch name whose `var` value is
`Some` and not its on value, in `INCREMENTAL_SWITCHES` order.

Replace `build_dir_cargo_env`'s doc comment with:

```rust
/// Environment changes for cargo run in `build_dir`.
///
/// `set` holds those of [`cargo_env`], and `CARGO_TARGET_DIR` naming the build dir's own
/// `target/`, unless mutants are tested in place. A target dir that the user sets in the
/// environment or in cargo config would otherwise be shared by all the build dirs, so that
/// concurrent jobs would build into it at once and test each other's mutants.
/// `CARGO_TARGET_DIR` takes precedence over both.
///
/// `remove` holds the switches [`incremental_switches_to_remove`] names. A scratch build
/// dir rebuilds the mutated package once per mutant, so a switch that a shell or a CI job
/// sets to save disk would make every one of those builds start from nothing.
///
/// In place, there's only one build dir, which is the user's own tree, so their settings
/// are kept.
```

INTENT: return `Env`. `set` is today's vector. `remove` is
`incremental_switches_to_remove(options, |name| env::var(name).ok())`. Delete the per-call `Overriding ...` debug event
loop at `:112-119`. Task 4 reports it once per run.

`run_cargo` passes `&build_dir_cargo_env(...)` directly.

In `src/schemata/run.rs`: `Runner::cargo_env` and `Runner::test_env` return `Env`. `test_env`
pushes the id onto `set`. `run_step` takes `env: &Env`. `run_test_command` extends `env.set`
with the command's variables and the id. Callers that pass `&self.cargo_env()` keep working.

In `src/schemata/coverage/collect.rs` `:391`: extend `env.set` with the three variables as today,
and with `("CARGO_INCREMENTAL", "0")`. Add one comment line above it:

```rust
    // One build per run gains nothing from incremental compilation, and its cache is
    // disk that the run's other build dirs need.
```

`:501`: extend `env.set` as today.

- [x] **Step 5: watch them pass.** Same command as Step 3. Expected: 6 passed, or 5 and one
  `SKIPPED` only if llvm-tools cannot be installed. Report that case.

- [x] **Step 6: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Remove inherited incremental switches in scratch build dirs' -- src/cargo.rs src/schemata/run.rs src/schemata/coverage/collect.rs tests/main.rs
```

---

## Task 3: seeding skips incremental caches (Batch A)

**Files:** modify `src/copy_tree.rs` (`copy_target_dir` `:148`, `copy_tree`'s `filter_entry`
`:237-248`, `mod test`).

- [x] **Step 1: write the failing tests** in `src/copy_tree.rs`'s `mod test`, after
  `copy_target_dir_copies_nested_and_hidden_files_preserving_mtime`.

```rust
#[test]
fn copy_target_dir_skips_incremental_caches_beside_fingerprints() -> Result<()> {
    let tmp = TempDir::new().unwrap();
    let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
    let src = root.join("target");
    let profile_dirs = ["debug", "x86_64-unknown-linux-gnu/debug"];
    for profile_dir in profile_dirs {
        let profile_dir = src.join(profile_dir);
        create_dir_all(profile_dir.join(".fingerprint/foo-1234"))?;
        write(profile_dir.join(".fingerprint/foo-1234/lib-foo"), "fingerprint")?;
        create_dir_all(profile_dir.join("incremental/foo-1234"))?;
        write(profile_dir.join("incremental/foo-1234/cache"), "cache")?;
    }
    // A test's own scratch directory that happens to be called `incremental`.
    create_dir_all(src.join("tmp/x/incremental"))?;
    write(src.join("tmp/x/incremental/data"), "test data")?;
    let dest = root.join("new/target");
    create_dir(root.join("new"))?;

    copy_target_dir(&src, &dest, &Console::new())?;

    for profile_dir in profile_dirs {
        let profile_dir = dest.join(profile_dir);
        assert!(!profile_dir.join("incremental").exists(), "{profile_dir}");
        assert!(
            profile_dir.join(".fingerprint/foo-1234/lib-foo").is_file(),
            "{profile_dir}"
        );
    }
    assert!(dest.join("tmp/x/incremental/data").is_file());
    Ok(())
}

#[test]
fn copy_tree_with_copy_target_skips_incremental_caches_beside_fingerprints() -> Result<()> {
    let tmp_dir = TempDir::new().unwrap();
    let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
    create_dir_all(tmp.join("target/debug/.fingerprint/foo-1234"))?;
    write(tmp.join("target/debug/.fingerprint/foo-1234/lib-foo"), "fingerprint")?;
    create_dir_all(tmp.join("target/debug/incremental/foo-1234"))?;
    write(tmp.join("target/debug/incremental/foo-1234/cache"), "cache")?;
    // A test's own scratch directory that happens to be called `incremental`.
    create_dir_all(tmp.join("target/tmp/x/incremental"))?;
    write(tmp.join("target/tmp/x/incremental/data"), "test data")?;
    write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
    create_dir(tmp.join("src"))?;
    write(tmp.join("src/main.rs"), "fn main() {}")?;

    let options = Options::from_arg_strs(["mutants", "--copy-target=true"]);
    let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
    let dest = dest_tmpdir.path();

    assert!(!dest.join("target/debug/incremental").exists());
    assert!(dest.join("target/debug/.fingerprint/foo-1234/lib-foo").is_file());
    assert!(dest.join("target/tmp/x/incremental/data").is_file());
    Ok(())
}
```

- [x] **Step 2: watch them fail.**

```
cargo nextest run --all-features -E 'test(/skips_incremental_caches_beside_fingerprints/)'
```

Expected: 2 run, 2 fail on the `!...incremental...exists()` assertion.

- [x] **Step 3: implement.** In `src/copy_tree.rs`, above `copy_target_dir`:

```rust
/// True if `path` is cargo's incremental compilation cache for one profile: a directory
/// named `incremental` beside the profile's `.fingerprint` directory.
///
/// A copied cache isn't reused in the new build dir, whose path differs, so copying it
/// only spends disk. Measured in `docs/work/fallback-build-cost/design.md`, Measured 9.
/// Fingerprints don't refer to it, so cargo still sees the copied build as fresh.
```

INTENT: `fn is_incremental_cache(path: &Path) -> bool`. Check the file name first, and only then
stat `parent/.fingerprint`, so the walk does no extra stat for other entries. In
`copy_target_dir`, add a `filter_entry` to its `WalkBuilder` that drops a directory entry for
which this is true. In `copy_tree`, add `&& !is_incremental_cache(entry.path())` to the existing
`filter_entry`, gated on the entry being a directory.

- [x] **Step 4: watch them pass.** Same command. Expected: 2 passed. Then run the module's other
  tests:

```
cargo nextest run --all-features -E 'test(/copy_tree::/)'
```

Expected: every test passes. Quote the count.

- [x] **Step 5: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Skip incremental caches when seeding a build dir' -- src/copy_tree.rs
```

- [x] **Batch A end:** `df -h ~/repos`, then the whole suite with
  `cargo nextest run --all-features`. Report pass, fail and skip counts. Report the RED line of
  each new test.

---

## Task 4: report the overrides once per run (Batch B)

**Files:** modify `src/cargo.rs`, `src/main.rs` (`:690`, after `console.set_debug_log`),
`src/schemata/mod.rs` (`Report` `:292`, its construction `:716`), `tests/main.rs`.

- [x] **Step 1: write the failing unit tests** in `src/cargo.rs`'s `mod test`.

```rust
#[test]
fn env_overrides_names_each_variable_a_scratch_build_dir_overrides_or_removes() {
    let vars = [("CARGO_TARGET_DIR", "/shared"), ("CARGO_INCREMENTAL", "0")];
    let var = |name: &str| {
        vars.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| (*value).to_owned())
    };
    let overrides = env_overrides(&Options::default(), var);
    assert_eq!(
        overrides.overridden,
        BTreeMap::from([("CARGO_TARGET_DIR".to_owned(), "/shared".to_owned())])
    );
    assert_eq!(
        overrides.removed,
        BTreeMap::from([("CARGO_INCREMENTAL".to_owned(), "0".to_owned())])
    );
}

#[test]
fn env_overrides_is_empty_in_place_or_when_nothing_is_set() {
    let set = |name: &str| Some(format!("value of {name}"));
    let in_place = Options {
        in_place: true,
        ..Options::default()
    };
    assert_eq!(env_overrides(&in_place, set), EnvOverrides::default());
    assert_eq!(
        env_overrides(&Options::default(), |_| None),
        EnvOverrides::default()
    );
}
```

Add `use std::collections::BTreeMap;` to the test module if it is not there.

- [x] **Step 2: extend the schemata integration test** from Task 2,
  `incremental_switches_from_environment_are_not_inherited_by_schemata_build`. Keep the assert
  chain's output, and add after the existing assertions:

```rust
    let report: serde_json::Value = read_to_string(out.path().join("mutants.out/schemata.json"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        report["removed_env"],
        json!({"CARGO_BUILD_INCREMENTAL": "false", "CARGO_INCREMENTAL": "0"})
    );
```

Change the test's `.assert().success();` to capture the output, and add:

```rust
    let output = String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
        + &String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(output.contains("CARGO_INCREMENTAL=0"), "{output}");
```

where `let assert = run()...timeout(OUTER_TIMEOUT).assert().success();`.

- [x] **Step 3: keep the other integration tests hermetic.** The new console line goes to
  stderr whenever `CARGO_INCREMENTAL` or `CARGO_BUILD_INCREMENTAL` is set. Eight existing tests
  assert an empty or exact stderr, and the fork's CI sets `CARGO_INCREMENTAL: 0`
  (`.github/workflows/tests.yml:39`). The plan's review demonstrated the 8 failures. In
  `tests/integration_util/mod.rs` `run()`, add both names to the filter of stripped variables,
  beside `GITHUB_ACTION`, and extend the comment above it:

```rust
    // Also strip CARGO_INCREMENTAL and CARGO_BUILD_INCREMENTAL, which cargo-mutants
    // reports on the console when it removes them in build dirs. Tests about them set
    // them explicitly.
```

- [x] **Step 4: watch the new tests fail.**

```
cargo nextest run --all-features -E 'test(/env_overrides_/) | test(/incremental_switches_from_environment_are_not_inherited_by_schemata_build/)'
```

Expected: none run, because the unit tests fail to compile on `env_overrides`. This said "3 run",
which was wrong for the reason Task 2 Step 3 gives. Add `--test main` to the same filter to see the
integration test fail on `removed_env` being `null`.

- [x] **Step 5: implement** in `src/cargo.rs`, beside `INCREMENTAL_SWITCHES`:

```rust
/// Variables that a scratch build dir overrides with its own value.
const OVERRIDDEN_IN_BUILD_DIRS: [&str; 2] = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"];

/// What a run in scratch build dirs takes from cargo-mutants' environment and changes:
/// each variable that was set, with the value it had.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EnvOverrides {
    /// Set, and replaced by the build dir's own value.
    pub overridden: BTreeMap<String, String>,
    /// Set, and removed, so that cargo config and the profile decide.
    pub removed: BTreeMap<String, String>,
}

/// The variables in `var` that scratch build dirs override or remove; none in place.
///
/// `var` reads one variable, so that tests don't change the process environment.
```

INTENT: `pub(crate) fn env_overrides(options: &Options, var: impl Fn(&str) -> Option<String>) ->
EnvOverrides`. In place, return the default. `overridden` holds each name in
`OVERRIDDEN_IN_BUILD_DIRS` that `var` returns. `removed` holds each name that
`incremental_switches_to_remove` returns, with its value, so the report and the removal never
disagree.

Then, beside it:

```rust
/// Report, once per run, what the build dirs change in cargo-mutants' environment.
///
/// `build_dir_cargo_env` runs for every process, including every replayed test, so it
/// reports nothing itself.
```

INTENT: `pub(crate) fn report_env_overrides(overrides: &EnvOverrides)`. One
`debug!(overridden = ?overrides.overridden, removed = ?overrides.removed,
"build_dirs.env_overrides")` when either map is non-empty. When `removed` is non-empty, one
`info!` line naming each removed variable as `NAME=value`. It says that in scratch build dirs
cargo config and the profile now decide incremental compilation. It does not say the builds are
incremental, because a profile can still turn that off. It lists the opt-outs: `incremental = false` in the profile,
`CARGO_PROFILE_<NAME>_INCREMENTAL=false`, or `build.incremental = false` in cargo config. Read
the console's other `info!` lines for tone.

In `src/main.rs`, after `console.set_debug_log(...)` and before the schemata branch, call
`report_env_overrides(&env_overrides(&options, |name| env::var(name).ok()))`.

In `src/schemata/mod.rs`, add to `Report`:

```rust
    /// Variables inherited from cargo-mutants' environment that the build dirs removed,
    /// with the values they had, such as `CARGO_INCREMENTAL`. Empty in place.
    removed_env: BTreeMap<String, String>,
```

and set it at construction from
`env_overrides(options, |name| env::var(name).ok()).removed`.

- [x] **Step 6: watch them pass.** Same command. Expected: 3 passed.

- [x] **Step 7: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Report the build dirs environment overrides once per run' -- src/cargo.rs src/main.rs src/schemata/mod.rs tests/main.rs tests/integration_util/mod.rs
```

---

## Task 5: a full disk stops the run (Batch B)

**Files:** modify `src/cargo.rs` (`run_cargo`, new `ran_out_of_disk`), `src/schemata/run.rs`
(`run_step`), `src/lab.rs` (`Worker::run_queue`), `tests/main.rs`. Create
`testdata/disk_full_build/{Cargo_test.toml,build.rs,src/lib.rs}` and
`testdata/disk_full_literal/{Cargo_test.toml,src/lib.rs}`.

- [x] **Step 1: write the failing unit tests** in `src/cargo.rs`'s `mod test`. The messages are
  the ones Measured 13 captured, plus Windows' `ERROR_DISK_FULL` text.

```rust
#[test]
fn ran_out_of_disk_matches_each_platforms_message() {
    assert!(ran_out_of_disk(
        "error: could not write output to /t/deps/x.rcgu.o: No space left on device\n"
    ));
    assert!(ran_out_of_disk(
        "error: No space left on device (os error 28) at path \"/t/check\"\n"
    ));
    assert!(ran_out_of_disk(
        "error: failed to write /t/x: There is not enough space on the disk. (os error 112)\n"
    ));
}

#[test]
fn ran_out_of_disk_does_not_match_a_compile_error_or_nothing() {
    assert!(!ran_out_of_disk(
        "error[E0308]: mismatched types\n  --> src/lib.rs:2:5\n"
    ));
    assert!(!ran_out_of_disk(""));
}

/// A compile error quotes source lines. cargo-mutants' own source holds the message as a
/// literal, and its CI runs cargo-mutants on itself.
#[test]
fn ran_out_of_disk_does_not_match_a_quoted_source_line() {
    assert!(!ran_out_of_disk(indoc! {r#"
        error[E0308]: mismatched types
          --> src/cargo.rs:30:5
           |
        30 |     "No space left on device",
           |     ^^^^^^^^^^^^^^^^^^^^^^^^^ expected `u8`, found `&str`
    "#}));
    assert!(!ran_out_of_disk(indoc! {r#"
        help: remove the extra argument
           |
        4  -     report("No space left on device");
        4  +     report();
    "#}));
}

/// The macOS linker reports a full disk by its error number only.
#[test]
fn ran_out_of_disk_matches_the_macos_linker() {
    assert!(ran_out_of_disk(indoc! {"
        error: linking with `cc` failed: exit status: 1
          = note: ld: ftruncate() failed, errno=28 for '/t/deps/x-1234'
    "}));
}

/// With `--message-format=json`, which the schema's check and build use, a diagnostic is one
/// line. Its own message counts. Its rendered text and spans quote source, so they don't.
#[test]
fn ran_out_of_disk_reads_only_the_messages_of_a_json_compiler_message() {
    let full = json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "could not write output to /t/deps/x.rcgu.o: No space left on device",
            "children": [],
            "spans": [],
            "rendered": "error: could not write output to /t/deps/x.rcgu.o: No space left on device\n",
        },
    });
    assert!(ran_out_of_disk(&full.to_string()));
    let quoted = json!({
        "reason": "compiler-message",
        "message": {
            "level": "warning",
            "message": "unused variable: `expected`",
            "children": [{
                "level": "help",
                "message": "if this is intentional, prefix it with an underscore: `_expected`",
                "children": [],
                "spans": [],
                "rendered": null,
            }],
            "spans": [{"text": [{"text": "    let expected = \"No space left on device\";"}]}],
            "rendered": "warning: unused variable: `expected`\n --> src/lib.rs:4:9\n  |\n4 |     let expected = \"No space left on device\";\n",
        },
    });
    assert!(!ran_out_of_disk(&quoted.to_string()));
}
```

Add `use indoc::indoc;` and `use serde_json::json;` to the test module if they are not there.
Both crates are already dependencies.

- [x] **Step 2: create the testdata tree.**

`testdata/disk_full_build/Cargo_test.toml`:

```toml
[package]
name = "cargo-mutants-testdata-disk-full-build"
description = "A build script that fails as a full disk would when one mutant is applied"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
doctest = false
```

`testdata/disk_full_build/build.rs`:

```rust
//! Fail the build with a full disk's message when one mutant, `x * 2` to `x + 2` in
//! `double`, is applied, or when the schema holds it. Every other mutant builds, so a
//! run with two jobs shows whether the second worker stops.
//!
//! No other line of `src/lib.rs` mutates into `x + 2`.

use std::fs::read_to_string;
use std::process::exit;

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    // the classic way writes `x + /* ~ changed by cargo-mutants ~ */ 2`, and the schema
    // writes `x + 2`
    let source = read_to_string("src/lib.rs")
        .expect("read src/lib.rs")
        .replace("/* ~ changed by cargo-mutants ~ */ ", "");
    if source.contains("x + 2") {
        eprintln!("error: No space left on device (os error 28)");
        exit(1);
    }
}
```

This script matched `x + 2` on the raw source until 2026-10-05, which was wrong. The classic way
writes the mutant with `MUTATION_MARKER_COMMENT` after the operator (`src/main.rs`), so the
trigger never fired without schemata. Measured: `22 mutants tested: 2 missed, 20 caught`, and the
trigger mutant was caught by `double_three_is_six`.

`testdata/disk_full_build/src/lib.rs`:

```rust
pub fn double(x: u32) -> u32 {
    x * 2
}

pub fn triple(x: u32) -> u32 {
    x * 3
}

pub fn square(x: u32) -> u32 {
    x * x
}

pub fn larger(a: u32, b: u32) -> u32 {
    if a > b { a } else { b }
}

pub fn at_most_ten(x: u32) -> u32 {
    if x > 10 { 10 } else { x }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn double_three_is_six() {
        assert_eq!(double(3), 6);
    }

    #[test]
    fn triple_three_is_nine() {
        assert_eq!(triple(3), 9);
    }

    #[test]
    fn square_three_is_nine() {
        assert_eq!(square(3), 9);
    }

    #[test]
    fn larger_picks_either_argument() {
        assert_eq!(larger(3, 5), 5);
        assert_eq!(larger(5, 3), 5);
    }

    #[test]
    fn at_most_ten_clamps_above_and_keeps_below() {
        assert_eq!(at_most_ten(12), 10);
        assert_eq!(at_most_ten(4), 4);
    }
}
```

`testdata/disk_full_literal/Cargo_test.toml`:

```toml
[package]
name = "cargo-mutants-testdata-disk-full-literal"
description = "Source that holds a full disk's message as text, and two unviable mutants"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
doctest = false
```

`testdata/disk_full_literal/src/lib.rs`:

```rust
pub fn greeting(name: &str) -> String {
    // `+` to `-` doesn't compile, so the schema's check fails and quotes source.
    name.to_owned() + "!"
}

#[cfg(test)]
mod test {
    #[test]
    fn greeting_adds_an_exclamation_mark() {
        // Unused, so rustc warns and quotes this line, as a test of disk-full
        // handling might hold it.
        let expected = "No space left on device";
        assert_eq!(super::greeting("hi"), "hi!");
    }
}
```

Before writing the tests, check the tree with `cargo mutants --list -d <copy>` on a copy of
`disk_full_build`. Only `replace * with + in double` may produce `x + 2`. List the mutants and
record the count, and the trigger's position in `--no-shuffle` order. The `-j2` test's bound
`1..=4` assumes the trigger is among the first three. If it is later, set the upper bound to its
position plus one, and say so in the report.

- [x] **Step 3: write the failing integration test** in `tests/main.rs`.

```rust
/// A build that fails because the disk is full stops the run with an error, rather than
/// recording the mutant as unviable: a full disk must not hide a missed mutant.
fn assert_disk_full_stops_the_run(args: &[&str]) -> TempDir {
    let tmp = copy_of_testdata("disk_full_build");
    let out = tempdir().unwrap();
    let assert = run()
        .args(["mutants", "--no-times"])
        .args(args)
        .arg("-d")
        .arg(tmp.path())
        .arg("-o")
        .arg(out.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .failure();
    let output = String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
        + &String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(output.contains("the disk is full"), "{output}");
    let unviable = read_to_string(out.path().join("mutants.out/unviable.txt")).unwrap_or_default();
    assert_eq!(unviable, "");
    out
}

/// With two jobs, the worker that hits a full disk stops the other one too: the run
/// doesn't test every remaining mutant before it fails.
#[test]
fn a_build_that_runs_out_of_disk_stops_the_run_in_disk_full_build_tree_without_schemata() {
    let out = assert_disk_full_stops_the_run(&["--no-schemata", "-j2", "--no-shuffle"]);
    let started = mutant_logs(&out.path().join("mutants.out")).len();
    assert!(
        (1..=4).contains(&started),
        "the second worker stopped after the mutant it held: {started} mutants started"
    );
}

#[test]
fn a_build_that_runs_out_of_disk_stops_the_run_in_disk_full_build_tree_with_schemata() {
    assert_disk_full_stops_the_run(&["--schemata"]);
}

/// Source that holds a full disk's message as text is quoted in compile errors and
/// warnings. That's not the disk, so the run goes on.
#[test]
fn source_holding_the_disk_full_message_does_not_stop_the_run_in_disk_full_literal_tree() {
    for schemata in ["--no-schemata", "--schemata"] {
        let tmp = copy_of_testdata("disk_full_literal");
        let out = tempdir().unwrap();
        run()
            .args(["mutants", "--no-times", schemata, "-d"])
            .arg(tmp.path())
            .arg("-o")
            .arg(out.path())
            .timeout(OUTER_TIMEOUT)
            .assert()
            .success();
        let unviable = read_to_string(out.path().join("mutants.out/unviable.txt")).unwrap();
        assert_eq!(unviable.lines().count(), 2, "{schemata}: {unviable}");
    }
}
```

The count said 1 until 2026-10-05, which was wrong. `+` mutates to `-` and to `*`, and neither
compiles on a `String`. Measured on both paths: `4 mutants tested: 2 caught, 2 unviable`.

- [x] **Step 4: watch them fail.**

```
cargo nextest run --all-features -E 'test(/ran_out_of_disk_/) | test(/runs_out_of_disk_stops_the_run/) | test(/disk_full_literal_tree/)'
```

Expected: none run, because the unit tests fail to compile on `ran_out_of_disk`. Add
`--test main` to the same filter to see the 3 integration tests' RED on their own. This said "8
run" and "comment them out", which was wrong for the reason Task 2 Step 3 gives. The filter also
lacked `test(/disk_full_literal_tree/)` until 2026-10-05, so it selected 7 tests, not 8.
- `..._without_schemata` fails on `the disk is full`: today it records the trigger mutant
  unviable. It passes `.failure()` today, because two `>` to `>=` mutants in the tree are
  equivalent and missed, so the run exits 2. This said it failed at `.failure()`, which was
  wrong.
- `..._with_schemata` fails on `the disk is full`: today it already stops, with
  `cargo build of the schema failed (Failure(101)) without reporting compile errors`
  (`src/schemata/run.rs:359-363`), or it records the trigger mutant unviable. The new check
  must run before that one.
- `source_holding_..._disk_full_literal_tree` passes today. It is the guard against the false
  stop in Measured 14, which the plan's review demonstrated on a probe of this detector. Note
  in the report that it was green before and after.

After the detector exists, the `-j2` half of `..._without_schemata` still fails on the
started count until `run_queue` empties the queue. Watch that failure before Step 5's queue
change.

- [x] **Step 5: implement** in `src/cargo.rs`:

```rust
/// True if `text`, written by a failed cargo command, says the disk was full.
///
/// rustc, the archiver and cargo itself print the operating system's message for the
/// error: the same text on macOS and Linux, and another on Windows. Measured by filling a
/// disk image: `docs/work/fallback-build-cost/design.md`, Measured 13.
```

INTENT: `pub(crate) fn ran_out_of_disk(text: &str) -> bool`, true if any line counts. Per line:
- A line that starts with `{` and parses as JSON: when its `reason` is `compiler-message`, it
  counts if `message.message` or any `message.children[].message` holds a marker. Never read
  `rendered` or `spans`. Any other JSON line never counts. Follow the `serde_json::Value` style
  of `src/schemata/diagnostics.rs`.
- Any other line counts if it holds a marker and rustc is not quoting source on it. Quoted
  source, trimmed of leading space, starts with `|`, or is digits, then spaces, then one of `|`,
  `-`, `+`, `~`, then a space or the end of the line.
- Markers, one `const` array with a comment per platform: `No space left on device` and
  `(os error 28)` (macOS and Linux, `ENOSPC`), `There is not enough space on the disk` and
  `(os error 112)` (Windows, `ERROR_DISK_FULL`, whose text is localized and whose code is not).
  Separately, `errno=28` counts only on a line that also holds `ld:` (the macOS linker).

In `src/lab.rs` `Worker::run_queue` (`:400`): when `run_one_scenario` returns an error, empty the
work queue under its lock (replace the iterator with an empty one) before returning the error.
Every other worker then finishes the mutant it holds and finds the queue empty. Add to the doc
comment of `run_queue`:

```rust
    /// On an error, empty the queue first, so that the other workers stop after the
    /// mutant each holds, rather than testing every remaining mutant before the run fails.
```

In `run_cargo`:
- Before `Process::run`, record the log file's length, from
  `scenario_output.output_dir.join(scenario_output.log_path())`.
- After it, when `phase` is `Check` or `Build` and the status is not success, read the log from
  that offset to the end. If `ran_out_of_disk`, `bail!` with a message that starts
  `the disk is full:` and names the phase and the log path.
- Read from the offset so that an earlier phase's output cannot match.
- Doc comment line to add to `run_cargo`:

```rust
/// A check or build that fails because the disk is full is an error, not a result: as a
/// result it would make the mutant unviable, and a full disk would hide a missed mutant.
```

In `Runner::run_step`: after reading `text`, when `phase` is `Check` or `Build`, the status is
not success, and `ran_out_of_disk(&text)`, `bail!` the same way. Each step has its own log, so the
whole text is this step's.

- [x] **Step 6: watch them pass.** Same command. Expected: 9 passed. Execution added a ninth, `ran_out_of_disk_does_not_match_a_colored_quoted_source_line`, with the escape-stripping step in `plain_line_ran_out_of_disk` (design, Measured 14).

- [x] **Step 7: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add testdata/disk_full_build testdata/disk_full_literal
git commit -m 'Stop the run when a build fails because the disk is full' -- src/cargo.rs src/schemata/run.rs src/lab.rs tests/main.rs testdata/disk_full_build testdata/disk_full_literal
```

- [x] **Batch B end:** `df -h ~/repos`, then the whole suite twice: once as your shell is, and
  once with `CARGO_INCREMENTAL=0` exported, as the fork's CI runs it. Report counts for both and
  each RED line.

---

## Task 6: docs, version, full gate (Batch C)

**Files:** modify `NEWS.md`, `book/src/build-dirs.md`, `Cargo.toml` `:3`, `Cargo.lock`.

- [x] **Step 1: version.** `Cargo.toml` `version = "27.1.0+omnith.2"`. Run `cargo build` so
  `Cargo.lock` follows. In `NEWS.md`, change the first Unreleased bullet's `27.1.0+omnith.1` to
  `27.1.0+omnith.2`.

- [x] **Step 2: `NEWS.md`.** Add two bullets under `## Unreleased`, after the version bullet, in
  the file's style: one paragraph each, `Changed:` and `Fixed:`.
  - **Changed:** cargo commands in a scratch build dir no longer see `CARGO_INCREMENTAL` or
    `CARGO_BUILD_INCREMENTAL` from the environment, so a mutant tested on its own builds
    incrementally even where a shell or CI job turns incremental compilation off. On one
    package the fallback phase took 0.42 to 0.70 of its time. Each build dir holds an
    incremental cache while the run lasts, about 1.5 GB on that package. To keep it off, set
    `incremental = false` in the profile, `CARGO_PROFILE_<NAME>_INCREMENTAL=false`, or
    `build.incremental = false` in cargo config. CI jobs that set `CARGO_INCREMENTAL=0` to save
    disk, or for sccache, which does not cache incremental crates, should use one of these. A
    switch set to turn incremental on is kept. `--in-place` keeps the environment. Seeding a build dir no
    longer copies the incremental cache. The removal is printed once and recorded as
    `removed_env` in `schemata.json`.
  - **Fixed:** a check or build that fails because the disk is full now stops the run with an
    error, and the other jobs stop after the mutant each holds. It used to record the mutant as
    unviable, so a full disk could hide a missed mutant. Source that holds the same message as
    text does not trigger it.

- [x] **Step 3: `book/src/build-dirs.md`.** Under `## Target directories`, add a section
  `## Incremental compilation` with the same facts as the Changed bullet, and the reason: each
  build dir rebuilds the mutated package once per mutant, and a switch set for a whole shell
  makes each of those builds start from nothing. In `## Seeding build directories from the
  baseline`, add one sentence: the copy leaves out the incremental cache, which the new build
  dir cannot reuse. In `book/src/schemata.md`, where it lists the keys of `schemata.json`
  (near `:500-506`), add one sentence naming `removed_env`.

- [x] **Step 4: the whole gate.** `df -h ~/repos` first.

```
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

Expected: fmt clean, clippy clean, every test passes. Quote the counts.

- [x] **Step 5: commit.**

```
git commit -m 'Describe incremental scratch builds and the disk-full stop' -- NEWS.md book/src/build-dirs.md book/src/schemata.md Cargo.toml Cargo.lock
```

---

## Task 7: acceptance on jast-platform (Batch D)

This is design Acceptance criterion 5. It needs Docker, and about 12 GiB free at the start.

- [x] **Step 1: disk and setup.** `df -h ~/repos`. Stop if under 12 GiB.

```
git -C ~/repos/cargo-mutants worktree add <scratch>/fork-old 2f837e8
cd <scratch>/fork-old && CARGO_TARGET_DIR=<scratch>/fork-old/target cargo build --release
cd ~/repos/cargo-mutants-wt-fbc && cargo build --release
git -C ~/repos/om-jastusa/remote-build-platform-v2 worktree add --detach <scratch>/jast aadd3fe8
```

`<scratch>` is a directory the orchestrator names in the dispatch. The jast worktree is pinned to
`aadd3fe8`, the rev Measured 12 ran, so its counts and its 20 classic fallback mutants hold. Do not run
`cargo install`: other sessions run the installed `cargo mutants` and must not see a new
binary mid-run. Invoke each binary by path, as `<binary> mutants ...`.

- [x] **Step 2: Postgres and MinIO** for the jast worktree, on ports no other session uses.
  Other sessions hold 5432, 9000, 55434 and 59002. First check that the two ports are free:
  `lsof -i :55436 -i :59006` prints nothing. Then:

```
cd <scratch>/jast && JAST_DB_PORT=55436 JAST_MINIO_PORT=59006 COMPOSE_PROJECT_NAME=rbp-fbc just dev-db
cd <scratch>/jast && JAST_DB_PORT=55436 JAST_MINIO_PORT=59006 COMPOSE_PROJECT_NAME=rbp-fbc just dev-minio
```

- [x] **Step 3: five runs, back to back**, each in the jast worktree, with its ports and its own
  `--output`. Without the ports, the jast tests default to 5432 and 9000 and write into another
  session's Postgres and MinIO (`apps/backend-core/tests/pg.rs:53`, `tests/s3.rs:55`).

```
cd <scratch>/jast && JAST_DB_PORT=55436 JAST_MINIO_PORT=59006 <env> <binary> mutants \
  -p backend-core --features pg-tests,s3-tests -j2 \
  --file apps/backend-core/src/config.rs --file apps/backend-core/src/adapters/s3_object_store.rs \
  --output <scratch>/out-<run>
```

`<env>` is `CARGO_INCREMENTAL=0` or `env -u CARGO_INCREMENTAL`. The old binary is
`<scratch>/fork-old/target/release/cargo-mutants`. The new one is
`~/repos/cargo-mutants-wt-fbc/target/release/cargo-mutants`.

| Run | Binary | `CARGO_INCREMENTAL` |
|---|---|---|
| A1 | old | `0` |
| B1 | new | `0` |
| A2 | old | `0` |
| B2 | new | `0` |
| C | new | unset (`env -u CARGO_INCREMENTAL`) |

Before each run, `df -h ~/repos`. Stop and report if under 10 GiB: a full disk during an old-rev
run would record mutants unviable without a word. Record `uptime` after each run.

- [x] **Step 4: check each criterion and record the evidence.**
  - Outcomes identical across all five: one `name<TAB>summary` list per run from `outcomes.json`,
    then `diff`. Expected: no difference. Measured 12 had 337 mutants, 280 caught and 57 unviable.
    If one differs, rerun that mutant alone with both binaries (`--re '<exact name>'`, same
    environment). A difference that involves `Unviable` on a fallback mutant blocks: stop and
    report. A `Timeout` against `CaughtMutant` on an embedded mutant that the rerun resolves is
    load. Record it and go on.
  - Each run tested at least 20 fallback mutants the classic way.
  - In B1, B2 and C, every classically built fallback mutant's log has `-C incremental=` on the
    `backend_core` rustc line. In A1 and A2, none has.
  - `removed_env` is `{"CARGO_INCREMENTAL": "0"}` in B1 and B2, and `{}` in C.
  - B1 plus B2 `fallback_wall_seconds` is at most 0.75 of A1 plus A2. If it is above 0.75 with
    every check above passing, rerun one pair. If the rerun is also above 0.75, stop and report
    both pairs with their load averages to the orchestrator, which takes it to Kane. Do not
    change the threshold.

- [ ] **Step 5: tear down** what this task made, and report the free space before and after.

```
cd <scratch>/jast && COMPOSE_PROJECT_NAME=rbp-fbc docker compose --profile s3 down -v
git -C ~/repos/om-jastusa/remote-build-platform-v2 worktree remove --force <scratch>/jast
git -C ~/repos/cargo-mutants worktree remove --force <scratch>/fork-old
```

Keep the five `out-<run>/mutants.out/schemata.json` and `outcomes.json` files. Delete the rest of
each output directory.

Open: the compose project is down, with its volumes. The session's permission classifier refused
the two `worktree remove --force` lines and the trim of each `out-<run>`, so Batch D's implementer
left them for the orchestrator.

- [x] **Step 6: write `docs/work/fallback-build-cost/impl.md`.** It holds what shipped, the
  table of the five runs with their load averages, and each criterion with its evidence. It has no
  narrative. Commit it:

```
git add docs/work/fallback-build-cost/impl.md
git commit -m 'Record the fallback-build-cost acceptance runs' -- docs/work/fallback-build-cost/impl.md
```

---

## Task 8: pull request, reviews, merge (Batch E)

- [ ] **Step 1: push and open the pull request** against the fork, never upstream:

```
git push -u omnith feat/fbc-1-incremental-scratch-dirs
gh pr create --repo Omnith/cargo-mutants --base main --head feat/fbc-1-incremental-scratch-dirs --title 'Build fallback mutants incrementally, and stop on a full disk' --body-file <body>
```

The body follows jast-platform's pull request rules, which the orchestrator passes in the
dispatch. One sentence on what it does, a table of what landed, a table of what was found, review
focus as a list of paths.

- [ ] **Step 2: reviews.** The orchestrator dispatches a code and architecture review and then an
  adversarial review over the diff. Fold every CRITICAL, HIGH and MEDIUM finding on the same
  branch. Re-run the Task 6 gate after the folds.

- [ ] **Step 3: merge** with `gh pr merge --repo Omnith/cargo-mutants --merge --delete-branch`.
  Merge only when the checks that exist have all passed. Then update the main checkout:
  `git -C ~/repos/cargo-mutants pull omnith main`. Remove the worktree
  `~/repos/cargo-mutants-wt-fbc` and its target.

- [ ] **Step 4: follow-up in jast-platform**, a separate change there: bump the fork rev that
  `just _mutants-tool` pins to the merge commit. In the same change, consider
  `env -u CARGO_INCREMENTAL` in the mutation recipes, so that agents' runs do not print the
  removal line every time. The fork removes the variable either way.
