# Copying the tree

By default, cargo-mutants copies your tree to a temporary directory before mutating and building it. This behavior is turned of by the [`--in-place`](in-place.md) option, which builds mutated code in the original source directory.

When the [`--jobs`](parallelism.md) option is used, one build directory is created per job.

Some filters are applied while copying the tree, which can be configured by options.

## Target directories

Each build directory builds into its own `target/` directory. cargo-mutants sets `CARGO_TARGET_DIR` for the cargo commands it runs in a build directory, which overrides any target directory you've configured with `CARGO_TARGET_DIR` or `CARGO_BUILD_TARGET_DIR` in the environment, or `build.target-dir` in cargo config. A target directory shared by all the build directories would let concurrent jobs overwrite each other's build products, and test one mutant with another's code. As a consequence, build products already in your configured target directory aren't reused. The tests that cargo runs also see this `CARGO_TARGET_DIR`.

With [`--in-place`](in-place.md), there's only one build directory, your source tree, and your configured target directory is used as usual.

## Incremental compilation

In a build directory that cargo-mutants creates, the cargo commands it runs don't see `CARGO_INCREMENTAL` or `CARGO_BUILD_INCREMENTAL` from the environment when the value turns incremental compilation off. Cargo config and the profile decide instead. A profile that inherits from `test` or `dev` has incremental compilation on. The tests that cargo runs also don't see a removed switch.

The reason is speed. A build directory rebuilds the mutated package once for each mutant that it builds on its own. A switch set for a whole shell or CI job makes each of those builds start from nothing. On one package, the fallback mutants took 0.42 to 0.70 of the time they took with incremental compilation off. Every outcome was the same.

The cost is disk. Each build directory holds an incremental cache while the run lasts, about 1.5 GB on that package.

To keep incremental compilation off in build directories, use one of these:

* `incremental = false` in the profile that cargo-mutants builds with.
* `CARGO_PROFILE_<NAME>_INCREMENTAL=false` in the environment, where `<NAME>` is that profile.
* `build.incremental = false` in cargo config.

A CI job that sets `CARGO_INCREMENTAL=0` to save disk should use one of these. So should a job that sets it for sccache, which does not cache incremental crates.

A switch that turns incremental compilation on is kept: `CARGO_INCREMENTAL=1` or `CARGO_BUILD_INCREMENTAL=true`. Removing it would turn incremental compilation off for a profile that says `incremental = false`.

When cargo-mutants removes a switch, it prints one line at the start of the run. The line names the variable and its value. `schemata.json` records the removed variables as `removed_env`. `mutants.out/debug.log` records the variables that build directories override or remove as a `build_dirs.env_overrides` event.

The coverage build of [test selection](schemata.md#coverage-based-test-selection) stays non-incremental. It sets `CARGO_INCREMENTAL=0` itself, because it runs once per run.

With [`--in-place`](in-place.md), your environment is used as it is.

## Seeding build directories from the baseline

When `--jobs` is more than 1 and the [baseline](baseline.md) passes, cargo-mutants makes the extra build directories after the baseline, and copies the baseline build's `target/` directory into each of them. This means each job can start with an incremental build rather than building all your dependencies from scratch. The copy leaves out cargo's incremental cache, the `incremental` directory beside each `.fingerprint` directory, because the new build directory can't reuse it.

Every package in your workspace is still rebuilt once in each new build directory, because build products can contain the absolute path of the directory where they were built (for example from `env!("CARGO_MANIFEST_DIR")` or `file!()`). cargo-mutants gives the copied source files a newer modification time than the copied build products, so that cargo sees the workspace packages as out of date. Dependencies from outside the workspace are reused as they are.

On filesystems that support reflinks (such as APFS, Btrfs, and XFS) the copy is fast and uses little extra disk space. On other filesystems the whole `target/` directory is copied, which takes about as much disk space as building it from scratch would, but may take some time.

The `target/` directory is only copied if it's a real directory inside the baseline build directory: if `target` is a symlink, for example because it was copied from your tree with [`copy_target`](#gitignore), the extra build directories are built from scratch.

With `--baseline=skip` the extra build directories are built from scratch as before.

If seeding a build directory fails, for example because the disk is full, cargo-mutants prints a warning and builds that directory from scratch.

Some unusual trees aren't fully rebuilt after seeding, and should turn it off with `--seed-target=false`:

* A workspace package whose source files are all symlinks to files outside the tree: those files keep their old modification time, so cargo may reuse the package as it was built in the baseline directory.
* A workspace package whose build script only asks to be rerun when environment variables change (`cargo::rerun-if-env-changed`) and writes out absolute paths: its output is reused from the baseline build.

Seeding is on by default. It can be turned off with `--seed-target=false`, or by setting `seed_target = false` in `.cargo/mutants.toml`.

## Troubleshooting tree copies

If the baseline tests fail in the copied directory it is a good first debugging step to try building with `--in-place`.

## `.git` and other version control directories

By default, files or directories matching these patterns are not copied, because they can be large and typically are not needed to build the source:

    .git
    .hg
    .jj
    .bzr
    .svn
    _darcs
    .pijul

If your tree's build or tests require the VCS directory then it can be copied with `--copy-vcs=true` or by setting `copy_vcs = true` in `.cargo/mutants.toml`.

## `.gitignore`

The `--gitignore=true` command line option or `gitignore = true` in `.cargo/mutants.toml` enables gitignore filtering, meaning that files matching gitignore patterns will be excluded from copying from the source tree to the build directory.

This option will make copying slightly faster (and use less temporary space) if your tree contains a large number of ignored files that aren't needed to build the source for mutation testing.

gitignore filtering is only used within trees containing a `.git` directory.

The filter, based on the [`ignore` crate](https://docs.rs/ignore/), also respects global git ignore configuration in the home directory, as well as `.gitignore` files within the tree.

The `target/` directory is excluded by default, regardless of gitignore settings, to avoid copying large build artifacts that are typically not needed for mutation testing. This can be overridden with `--copy-target=true` if your tests depend on existing build artifacts, or by setting `copy_target = true` in `.cargo/mutants.toml`. The copy leaves out cargo's incremental cache, as [seeding](#seeding-build-directories-from-the-baseline) does.

Note that if you set `--gitignore=true` and `--copy-target=true` and your `target/` is excluded by gitignore files (which is common) then it will not be copied.

The default for gitignore filtering is off. Prior to cargo-mutants 25.0.2, `gitignore` was on by default.

## `mutants.out`

`mutants.out` and `mutants.out.old` are never copied, even if they're not covered by `.gitignore`.
