# Improving performance

Most of the runtime for cargo-mutants is spent in running the program test suite
and in running incremental builds: both are done once per viable mutant.

So, anything you can do to make the `cargo build` and `cargo test` suite faster
will have a multiplicative effect on `cargo mutants` run time, and of course
will also make normal development more pleasant.

<https://matklad.github.io/2021/09/04/fast-rust-builds.html> has good general advice on making Rust builds and tests faster.

## What cargo-mutants does by default

These are on by default, because on the trees measured they gave the same outcome for every mutant compared as building and testing each mutant separately:

- [Mutant schemata](schemata.md): the tree is built once for all the mutants that can be embedded, rather than once per mutant. On a 104-mutant sample of one crate, with `-j2`, a run took 450-464 seconds rather than 1,014-1,226 seconds.
- [Coverage-based test selection](schemata.md#coverage-based-test-selection): each mutant runs only the tests that execute its code. On the same sample this cut the run from 346-351 seconds to 189-194 seconds; on the whole crate, 2,074 mutants, from 58 minutes to 9 minutes. It's used when collecting coverage is [expected to save more time than it takes](schemata.md#choosing-automatically), which it doesn't for a handful of mutants. It needs the `llvm-tools` rustup component (see below); without it, all tests run.
- Mutants whose compile errors [prove them unviable](schemata.md#mutants-that-cant-compile) are recorded without building them again.
- [Stopping each test binary at the first failed test](fail-fast.md). This cut the test phase of caught mutants by 34-40%. The log then names the failing test and gives a command to rerun that mutant with complete output.
- Without `--jobs`, the number of mutants tested at once is [chosen by measuring](schemata.md#how-many-mutants-are-tested-at-once).
- With several jobs, extra build directories are [seeded](build-dirs.md#seeding-build-directories-from-the-baseline) from the baseline's build.

Some options turn schemata off, so every mutant is built separately: `--test-tool=nextest`, `--in-place`, `--check`, and `--baseline=skip`. cargo-mutants says so when this happens.

To get more from these defaults, install llvm-tools for the toolchain that builds your tree, by running this in the tree:

```sh
rustup component add llvm-tools
```

## In CI

- Install llvm-tools, as above, or with `components: llvm-tools` in [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain). Otherwise every mutant runs all the tests, and cargo-mutants says so once near the start of the output.
- Don't use `--in-place` or `--baseline=skip` just to save time: they turn schemata off, and with schemata the tree is copied and built once, and the baseline adds only one run of the tests.
- Leave `--jobs` unset unless you've measured: cargo-mutants then times the tests running 1, 2, 4, ... copies at once and picks the number that gives the most throughput, which took about 19 seconds on one crate. More jobs don't always help, because the limit might not be the CPUs. For example, on macOS, `XprotectService` scans each newly written executable or script before it runs, in one process, and on one crate that limited throughput beyond `-j2`.
- When [sharding](shards.md), each shard builds its own schema.
- The [timing breakdown](output.md#timing-breakdown) at the end of the output shows where the time went.

## Find out where the time goes

Before changing anything, look at the [timing breakdown](output.md#timing-breakdown) printed at the end of a run (and written to `mutants.out/debug.log`). It shows whether builds or tests dominate, how much time went to unviable mutants and timeouts, and the slowest mutants. Change one thing at a time and compare two runs over the same set of mutants (for example with the same [`--shard`](shards.md)), since build times on a busy machine vary from run to run.

## Don't build test harnesses for binaries without tests

`cargo test` builds every binary target twice: once as the binary itself (for integration tests that run it) and once as a unit-test harness. When a mutant changes a library that many binaries depend on, every one of those is rebuilt and relinked, even if the binaries contain no tests.

If a binary has no `#[test]` functions, set `test = false` on its `[[bin]]` target in `Cargo.toml`:

```toml
[[bin]]
name = "migrate"
path = "src/bin/migrate.rs"
test = false
```

On one workspace with nine test-free binaries this cut each mutant's incremental build by about 17% in wall time and 23% in CPU time, without removing any tests.

## Stop each test binary on the first failure

Most mutants are caught by one or a few tests, but the test harness on its own still runs every other test in that binary, and waits for the slowest one. cargo-mutants [stops the tests at the first failure](fail-fast.md) by default. [nextest](nextest.md) also stops at the first failure, but turns schemata off, and adds a fixed overhead of around a second to each build on some trees.

## Don't copy large ignored files into build directories

cargo-mutants copies the source tree into each build directory. By default it copies files that are ignored by git, because some builds depend on them. If your tree contains large ignored directories, such as tool caches (`.terraform`, `node_modules`) or data files, the copy can be several gigabytes per build directory. On filesystems that support reflinks this is cheap, but elsewhere it can take a long time and a lot of disk. If your build doesn't need ignored files, set [`gitignore = true`](build-dirs.md#gitignore) in `.cargo/mutants.toml`.

## Avoid doctests

Rust doctests are pretty slow, because every doctest example becomes a separate
test binary. If you're using doctests only as testable documentation and not to
assert correctness of the code, you can skip them with `cargo mutants --
--all-targets`.

## Choosing a cargo profile

[Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html) provide a way to configure compiler settings including several that influence build and runtime performance.

By default, cargo-mutants will use the default profile selected for `cargo test`, which is also called `test`. This includes debug symbols but disables optimization.

You can select a different profile using the `--profile` option or the `profile` configuration key.

You may wish to define a `mutants` profile in `Cargo.toml`, such as:

```toml
[profile.mutants]
inherits = "test"
debug = "none"
```

and then configure this as the default in `.cargo/mutants.toml`:

```toml
profile = "mutants"
```

Turning off debug symbols will make the builds faster, at the expense of possibly giving less useful output when a test fails. In general, since mutants are expected to cause tests to fail, debug symbols may not be worth cost.

If your project's tests take a long time to run then it may be worth experimenting with increasing the `opt` level or other optimization parameters in the profile, to trade off longer builds for faster test runs.

cargo-mutants now shows the breakdown of build versus test time which may help you work out if this will help: if the tests are much slower than the build it's worth trying more more compiler optimizations.

## Ramdisks

cargo-mutants causes the Rust toolchain (and, often, the program under test) to read and write _many_ temporary files. Setting the temporary directory onto a ramdisk can improve performance significantly. This is particularly important with parallel builds, which might otherwise hit disk bandwidth limits.

See your OS's documentation for how to configure a ramdisk.

To temporarily configure a ramdisk on Linux as an experiment:

```shell
sudo mkdir /ram
sudo mount -t tmpfs /ram /ram
sudo chmod 1777 /ram
env TMPDIR=/ram cargo mutants
```

Some Rust build directories can be multiple gigabytes in size, and if you use `cargo mutants -j` there will be several directories of that size. Be careful that the ramdisk does not use so much memory that it causes the system to swap.

## Using faster linkers

Because cargo-mutants does many incremental builds, link time is important, especially if the test suite is relatively fast.

Using a non-default linker can give a significant performance improvement. The exact amount will depend on the project.

Using the [Mold linker](https://github.com/rui314/mold) on Unix can give a 20% performance improvement, depending on the tree.

On Linux, the [Wild linker](https://github.com/davidlattimore/wild) can give a significant performance improvement, potentially even better than Mold. On one tree, using Wild cut the time to run cargo-mutants by more than half.
