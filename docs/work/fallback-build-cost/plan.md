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
- Commit with `git commit -m '<msg>' -- <paths>`. List every path.
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

- [ ] **Step 1: write the failing tests** at the end of `src/process.rs`, beside `mod test`.

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

- [ ] **Step 2: run them and watch them fail to compile on the missing `Env`.**

```
cargo nextest run --all-features -E 'test(/env_test::/)'
```

Expected: error `cannot find type Env`, or `unresolved import super::Env`.

- [ ] **Step 3: add `Env` to `src/process.rs`**, above `pub struct Process`.

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

- [ ] **Step 4: change every caller to build an `Env`**, with `set` holding what it passed before
  and `remove` empty. Behaviour is unchanged in this task. Callers:
  - `run_cargo` passes `&Env { set: build_dir_cargo_env(...), remove: Vec::new() }` for now.
  - `Runner::cargo_env` and `Runner::test_env` keep returning `Vec<(String, String)>` for now.
    `run_step` and `run_test_command` wrap theirs. Task 2 changes the return types.
  - `collect.rs` `:512` wraps its `env`.

- [ ] **Step 5: run the three tests and watch them pass.**

```
cargo nextest run --all-features -E 'test(/env_test::/)'
```

Expected: 3 passed.

- [ ] **Step 6: commit.**

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

- [ ] **Step 1: write the failing unit test** in `src/cargo.rs`'s `mod test`, beside
  `build_dir_cargo_env_sets_cargo_target_dir_to_own_target_except_in_place`.

```rust
#[test]
fn build_dir_cargo_env_removes_the_incremental_switches_except_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let build_dir = BuildDir::in_place(tmp.path().try_into().unwrap()).unwrap();
    assert_eq!(
        build_dir_cargo_env(&build_dir, &Options::default()).remove,
        ["CARGO_INCREMENTAL", "CARGO_BUILD_INCREMENTAL"]
    );
    let in_place = Options {
        in_place: true,
        ..Options::default()
    };
    assert_eq!(
        build_dir_cargo_env(&build_dir, &in_place).remove,
        Vec::<String>::new()
    );
}
```

The existing test reads the set half: change its `.into_iter()` to `.set.into_iter()`. It
asserts the same values as before.

- [ ] **Step 2: write the failing integration tests** in `tests/main.rs`, after
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
/// it rebuilds the mutated package once per mutant.
#[test]
fn incremental_switches_from_environment_are_not_inherited_by_build_dirs() {
    let tmp = copy_of_testdata("small_well_tested");
    let out = tempdir().unwrap();
    run()
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_BUILD_INCREMENTAL", "false")
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

- [ ] **Step 3: watch them fail.**

```
cargo nextest run --all-features -E 'test(/build_dir_cargo_env_removes_the_incremental_switches/) | test(/incremental_switches_from_environment/) | test(/incremental_off_in_the_profile/) | test(/coverage_build_is_not_incremental/)'
```

Expected: 5 run.
- The unit test fails to compile on the missing `.remove`. Record the error.
- `incremental_switches_..._build_dirs` and `..._schemata_build` fail on the `-C incremental=`
  assertion.
- `incremental_off_in_the_profile_...` passes today, because nothing is removed yet. It is the
  guard for the opt-out, so this pass is expected. Say so in the report.
- `coverage_build_is_not_incremental_...` fails on the coverage assertion. With both switches
  unset, the profile turns incremental on for the coverage build as well as the schema's. If
  llvm-tools is missing it prints `SKIPPED` and proves nothing. Install them with
  `rustup component add llvm-tools` before you run it.

- [ ] **Step 4: implement.**

In `src/cargo.rs`, beside `build_dir_cargo_env`:

```rust
/// Variables that turn incremental compilation off for every cargo command. A scratch
/// build dir removes them, so that cargo config and the profile decide.
///
/// Cargo reads `CARGO_INCREMENTAL` first, then `build.incremental`, whose environment form
/// is `CARGO_BUILD_INCREMENTAL`, then the profile. Measured in
/// `docs/work/fallback-build-cost/design.md`, Measured 10.
const REMOVED_IN_BUILD_DIRS: [&str; 2] = ["CARGO_INCREMENTAL", "CARGO_BUILD_INCREMENTAL"];
```

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
/// `remove` holds [`REMOVED_IN_BUILD_DIRS`], unless mutants are tested in place. A scratch
/// build dir rebuilds the mutated package once per mutant, so a switch that a shell or a
/// CI job sets to save disk would make every one of those builds start from nothing.
///
/// In place, there's only one build dir, which is the user's own tree, so their settings
/// are kept.
```

INTENT: return `Env`. `set` is today's vector. `remove` is `REMOVED_IN_BUILD_DIRS` as owned
strings when `!options.in_place`, else empty. Delete the per-call `Overriding ...` debug event
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

- [ ] **Step 5: watch them pass.** Same command as Step 3. Expected: 5 passed, or 4 and one
  `SKIPPED` only if llvm-tools cannot be installed. Report that case.

- [ ] **Step 6: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Remove inherited incremental switches in scratch build dirs' -- src/cargo.rs src/schemata/run.rs src/schemata/coverage/collect.rs tests/main.rs
```

---

## Task 3: seeding skips incremental caches (Batch A)

**Files:** modify `src/copy_tree.rs` (`copy_target_dir` `:148`, `copy_tree`'s `filter_entry`
`:237-248`, `mod test`).

- [ ] **Step 1: write the failing tests** in `src/copy_tree.rs`'s `mod test`, after
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
    write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
    create_dir(tmp.join("src"))?;
    write(tmp.join("src/main.rs"), "fn main() {}")?;

    let options = Options::from_arg_strs(["mutants", "--copy-target=true"]);
    let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
    let dest = dest_tmpdir.path();

    assert!(!dest.join("target/debug/incremental").exists());
    assert!(dest.join("target/debug/.fingerprint/foo-1234/lib-foo").is_file());
    Ok(())
}
```

- [ ] **Step 2: watch them fail.**

```
cargo nextest run --all-features -E 'test(/skips_incremental_caches_beside_fingerprints/)'
```

Expected: 2 run, 2 fail on the `!...incremental...exists()` assertion.

- [ ] **Step 3: implement.** In `src/copy_tree.rs`, above `copy_target_dir`:

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

- [ ] **Step 4: watch them pass.** Same command. Expected: 2 passed. Then run the module's other
  tests:

```
cargo nextest run --all-features -E 'test(/copy_tree::/)'
```

Expected: every test passes. Quote the count.

- [ ] **Step 5: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Skip incremental caches when seeding a build dir' -- src/copy_tree.rs
```

- [ ] **Batch A end:** `df -h ~/repos`, then the whole suite with
  `cargo nextest run --all-features`. Report pass, fail and skip counts. Report the RED line of
  each new test.

---

## Task 4: report the overrides once per run (Batch B)

**Files:** modify `src/cargo.rs`, `src/main.rs` (`:690`, after `console.set_debug_log`),
`src/schemata/mod.rs` (`Report` `:292`, its construction `:716`), `tests/main.rs`.

- [ ] **Step 1: write the failing unit tests** in `src/cargo.rs`'s `mod test`.

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

- [ ] **Step 2: extend the schemata integration test** from Task 2,
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

- [ ] **Step 3: watch them fail.**

```
cargo nextest run --all-features -E 'test(/env_overrides_/) | test(/incremental_switches_from_environment_are_not_inherited_by_schemata_build/)'
```

Expected: 3 run. The unit tests fail to compile on `env_overrides`. The integration test fails on
`removed_env` being `null`.

- [ ] **Step 4: implement** in `src/cargo.rs`, beside `REMOVED_IN_BUILD_DIRS`:

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
EnvOverrides`. In place, return the default. Otherwise look up each name in the two consts and
keep those `var` returns.

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
`info!` line naming each removed variable as `NAME=value`. It says that scratch build dirs build
incrementally without it, and lists the opt-outs: `incremental = false` in the profile,
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

- [ ] **Step 5: watch them pass.** Same command. Expected: 3 passed.

- [ ] **Step 6: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Report the build dirs environment overrides once per run' -- src/cargo.rs src/main.rs src/schemata/mod.rs tests/main.rs
```

---

## Task 5: a full disk stops the run (Batch B)

**Files:** modify `src/cargo.rs` (`run_cargo`, new `ran_out_of_disk`), `src/schemata/run.rs`
(`run_step`), `tests/main.rs`. Create `testdata/disk_full_build/Cargo_test.toml`,
`testdata/disk_full_build/build.rs`, `testdata/disk_full_build/src/lib.rs`.

- [ ] **Step 1: write the failing unit tests** in `src/cargo.rs`'s `mod test`. The messages are
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
```

- [ ] **Step 2: create the testdata tree.**

`testdata/disk_full_build/Cargo_test.toml`:

```toml
[package]
name = "cargo-mutants-testdata-disk-full-build"
description = "A build script that fails as a full disk would whenever src/lib.rs is mutated"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
doctest = false
```

`testdata/disk_full_build/build.rs`:

```rust
//! Fail the build with a full disk's message whenever `src/lib.rs` is mutated.
//!
//! The unmutated tree builds, so the baseline passes. Every mutant of `double`, and the
//! schema, changes the line this looks for.

use std::fs::read_to_string;
use std::process::exit;

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    let source = read_to_string("src/lib.rs").expect("read src/lib.rs");
    if !source.contains("\n    x * 2\n") {
        eprintln!("error: No space left on device (os error 28)");
        exit(1);
    }
}
```

`testdata/disk_full_build/src/lib.rs`:

```rust
pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod test {
    #[test]
    fn double_two_is_four() {
        assert_eq!(super::double(2), 4);
    }
}
```

- [ ] **Step 3: write the failing integration test** in `tests/main.rs`.

```rust
/// A build that fails because the disk is full stops the run with an error, rather than
/// recording the mutant as unviable: a full disk must not hide a missed mutant.
#[test]
fn a_build_that_runs_out_of_disk_stops_the_run_in_disk_full_build_tree() {
    for schemata in ["--no-schemata", "--schemata"] {
        let tmp = copy_of_testdata("disk_full_build");
        let out = tempdir().unwrap();
        let assert = run()
            .args(["mutants", "--no-times", schemata, "-d"])
            .arg(tmp.path())
            .arg("-o")
            .arg(out.path())
            .timeout(OUTER_TIMEOUT)
            .assert()
            .failure();
        let output = String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
            + &String::from_utf8_lossy(&assert.get_output().stderr);
        assert!(output.contains("the disk is full"), "{schemata}: {output}");
        let unviable =
            read_to_string(out.path().join("mutants.out/unviable.txt")).unwrap_or_default();
        assert_eq!(unviable, "", "{schemata}");
    }
}
```

- [ ] **Step 4: watch them fail.**

```
cargo nextest run --all-features -E 'test(/ran_out_of_disk_/) | test(/runs_out_of_disk_stops_the_run/)'
```

Expected: 3 run. The unit tests fail to compile on `ran_out_of_disk`. Comment them out for a
moment if you need the integration test's RED on its own. It must fail at `.failure()`, because
today the run exits 0 with every mutant unviable. Record that output, then restore the unit tests.

- [ ] **Step 5: implement** in `src/cargo.rs`:

```rust
/// True if `text`, written by a failed cargo command, says the disk was full.
///
/// rustc, the archiver and cargo itself print the operating system's message for the
/// error: the same text on macOS and Linux, and another on Windows. Measured by filling a
/// disk image: `docs/work/fallback-build-cost/design.md`, Measured 13.
```

INTENT: `pub(crate) fn ran_out_of_disk(text: &str) -> bool`. True if `text` contains
`No space left on device` or `There is not enough space on the disk`. Name both strings in one
`const` array with a one-line comment for each platform.

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

- [ ] **Step 6: watch them pass.** Same command. Expected: 3 passed.

- [ ] **Step 7: commit.**

```
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git commit -m 'Stop the run when a build fails because the disk is full' -- src/cargo.rs src/schemata/run.rs tests/main.rs testdata/disk_full_build
```

- [ ] **Batch B end:** `df -h ~/repos`, then the whole suite. Report counts and each RED line.

---

## Task 6: docs, version, full gate (Batch C)

**Files:** modify `NEWS.md`, `book/src/build-dirs.md`, `Cargo.toml` `:3`, `Cargo.lock`.

- [ ] **Step 1: version.** `Cargo.toml` `version = "27.1.0+omnith.2"`. Run `cargo build` so
  `Cargo.lock` follows. In `NEWS.md`, change the first Unreleased bullet's `27.1.0+omnith.1` to
  `27.1.0+omnith.2`.

- [ ] **Step 2: `NEWS.md`.** Add two bullets under `## Unreleased`, after the version bullet, in
  the file's style: one paragraph each, `Changed:` and `Fixed:`.
  - **Changed:** cargo commands in a scratch build dir no longer see `CARGO_INCREMENTAL` or
    `CARGO_BUILD_INCREMENTAL` from the environment, so a mutant tested on its own builds
    incrementally even where a shell or CI job turns incremental compilation off. On one
    package the fallback phase took 0.42 to 0.70 of its time. Each build dir holds an
    incremental cache while the run lasts, about 1.5 GB on that package. To keep it off, set
    `incremental = false` in the profile, `CARGO_PROFILE_<NAME>_INCREMENTAL=false`, or
    `build.incremental = false` in cargo config. CI jobs that set `CARGO_INCREMENTAL=0` to save
    disk should use one of these. `--in-place` keeps the environment. Seeding a build dir no
    longer copies the incremental cache. The removal is printed once and recorded as
    `removed_env` in `schemata.json`.
  - **Fixed:** a check or build that fails because the disk is full now stops the run with an
    error. It used to record the mutant as unviable, so a full disk could hide a missed mutant.

- [ ] **Step 3: `book/src/build-dirs.md`.** Under `## Target directories`, add a section
  `## Incremental compilation` with the same facts as the Changed bullet, and the reason: each
  build dir rebuilds the mutated package once per mutant, and a switch set for a whole shell
  makes each of those builds start from nothing. In `## Seeding build directories from the
  baseline`, add one sentence: the copy leaves out the incremental cache, which the new build
  dir cannot reuse.

- [ ] **Step 4: the whole gate.** `df -h ~/repos` first.

```
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

Expected: fmt clean, clippy clean, every test passes. Quote the counts.

- [ ] **Step 5: commit.**

```
git commit -m 'Describe incremental scratch builds and the disk-full stop' -- NEWS.md book/src/build-dirs.md Cargo.toml Cargo.lock
```

---

## Task 7: acceptance on jast-platform (Batch D)

This is design Acceptance criterion 5. It needs Docker, and about 12 GiB free at the start.

- [ ] **Step 1: disk and setup.** `df -h ~/repos`. Stop if under 12 GiB.

```
git -C ~/repos/cargo-mutants worktree add <scratch>/fork-old 2f837e8
cd <scratch>/fork-old && CARGO_TARGET_DIR=<scratch>/fork-old/target cargo build --release
cd ~/repos/cargo-mutants-wt-fbc && cargo build --release
git -C ~/repos/om-jastusa/remote-build-platform-v2 worktree add --detach <scratch>/jast origin/main
```

`<scratch>` is a directory the orchestrator names in the dispatch. Do not run
`cargo install`: other sessions run the installed `cargo mutants` and must not see a new
binary mid-run. Invoke each binary by path, as `<binary> mutants ...`.

- [ ] **Step 2: Postgres and MinIO** for the jast worktree, on ports no other session uses:

```
cd <scratch>/jast && JAST_DB_PORT=55436 JAST_MINIO_PORT=59006 COMPOSE_PROJECT_NAME=rbp-fbc just dev-db
cd <scratch>/jast && JAST_DB_PORT=55436 JAST_MINIO_PORT=59006 COMPOSE_PROJECT_NAME=rbp-fbc just dev-minio
```

- [ ] **Step 3: five runs, back to back**, each with that environment and its own `--output`.

```
<binary> mutants -p backend-core --features pg-tests,s3-tests -j2 \
  --file apps/backend-core/src/config.rs --file apps/backend-core/src/adapters/s3_object_store.rs \
  --output <scratch>/out-<run>
```

| Run | Binary | `CARGO_INCREMENTAL` |
|---|---|---|
| A1 | old | `0` |
| B1 | new | `0` |
| A2 | old | `0` |
| B2 | new | `0` |
| C | new | unset (`env -u CARGO_INCREMENTAL`) |

Record `uptime` after each run.

- [ ] **Step 4: check each criterion and record the evidence.**
  - Outcomes identical across all five: one `name<TAB>summary` list per run from `outcomes.json`,
    then `diff`. Expected: no difference. Measured 12 had 337 mutants, 280 caught and 57 unviable.
  - Each run tested at least 20 fallback mutants the classic way.
  - In B1, B2 and C, every classically built fallback mutant's log has `-C incremental=` on the
    `backend_core` rustc line. In A1 and A2, none has.
  - `removed_env` is `{"CARGO_INCREMENTAL": "0"}` in B1 and B2, and `{}` in C.
  - B1 plus B2 `fallback_wall_seconds` is at most 0.75 of A1 plus A2. If it is above 0.75 with
    every check above passing, rerun one pair before you conclude, and report both.

- [ ] **Step 5: tear down** what this task made, and report the free space before and after.

```
cd <scratch>/jast && COMPOSE_PROJECT_NAME=rbp-fbc docker compose --profile s3 down -v
git -C ~/repos/om-jastusa/remote-build-platform-v2 worktree remove --force <scratch>/jast
git -C ~/repos/cargo-mutants worktree remove --force <scratch>/fork-old
```

Keep the five `out-<run>/mutants.out/schemata.json` and `outcomes.json` files. Delete the rest of
each output directory.

- [ ] **Step 6: write `docs/work/fallback-build-cost/impl.md`.** It holds what shipped, the
  table of the five runs with their load averages, and each criterion with its evidence. It has no
  narrative. Commit it:

```
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
  `just _mutants-tool` pins to the merge commit.
