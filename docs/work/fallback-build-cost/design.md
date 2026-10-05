# fallback-build-cost

**Pre-design.** Session A writes Approach, Interfaces / contracts, Acceptance criteria and Out of
scope. Problem, User outcome, Measured and Dependencies are known.

## Problem

Fallback mutants are most of a full run's wall-clock time. A fallback mutant is one the schema
cannot embed, so the fork tests it the classic way, with a build of its own. On
`jast-platform`'s `backend-core` invocation, fallback testing took 55% of the wall (Measured 1).

The cost is not in the mutants that fail to compile. The check pass proves those unviable together
and cheaply. It is in a small number of mutants that compile but cannot be embedded, mostly ones
inside a `const` item (Measured 2). Each one rebuilds the whole package's test targets: the
library, every binary and every integration test (Measured 3).

Kane asked for two changes, 2026-10-05:

1. **Build only what a fallback mutant needs.** Coverage already records which tests reach each
   mutant. A fallback build could be limited to the targets that hold those tests, rather than
   `cargo test --no-run --package=<pkg>`.
2. **Handle `const`-context mutants differently.** They are the largest fallback class by time.
   Session A decides how. A runtime switch cannot choose a value the compiler must compute, so
   the options are to build several together, to embed through a non-`const` form where the item
   allows it, or something else the design finds.

## User outcome

A maintainer running the full gate waits for the mutants that need testing, not for repeated
whole-package rebuilds. The outcome counts are unchanged.

## Measured

All from the fork at `27.1.0+omnith.1`, rev `2f837e8`, run by `jast-platform`'s `just merge-gate`
on 2026-10-05 (start `16:49:48Z`) on branch `feat/bor-1-outage-requeue` at `07e57895`. Read from
`target/mutants-core/mutants.out/` of that worktree.

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

The 47 `const_context` mutants: median build 37.6 s, median test 2.1 s, from `outcomes.json`
`phase_results`. By file: `s3_object_store.rs` 12, `config.rs` 8, `clamav_scanner.rs` 8,
`derivation.rs` 8, others 11. One source line, `config.rs:675`,
`const SOURCE_MAX_BYTES_DEFAULT: u64 = 64 * 1024 * 1024 * 1024;`, gives six.

**3. What a fallback build compiles.** `log/apps__backend-core__src__config.rs_line_675_col_67_001.log`:

```
*** .../cargo test --no-run --profile=mutants --verbose --package=backend-core@0.1.0 --features=pg-tests,s3-tests
Running `rustc --crate-name backend_core ... apps/backend-core/src/lib.rs ...
Running `rustc --crate-name worker ... src/bin/worker.rs ...
Running `rustc --crate-name mint_key ... src/bin/mint_key.rs ...
Running `rustc --crate-name reindex ... src/bin/reindex.rs ...
Running `rustc --crate-name s3 ... tests/s3.rs ...
Running `rustc --crate-name provision_probe ... src/bin/provision_probe.rs ...
Running `rustc --crate-name port_dispatch_cost ... tests/port_dispatch_cost.rs ...
Running `rustc --crate-name claim_smoke ... src/bin/claim_smoke.rs ...
Running `rustc --crate-name emit_openapi ... src/bin/emit_openapi.rs ...
```

**4. The other invocations of the same run.** Fallback is a smaller share where the package is
smaller. `schemata.json` per invocation:

```
yunyun   wall 344 s   fallback_wall  46.8 s   unsupported_genre 4 → 80.8 s
format   wall 174 s   fallback_wall  74.9 s   const_context 16 → 78.4 s, source_read_by_tests 12 → 58.1 s
codegen  wall  61 s
dedup    wall  26 s
```

Re-derive any figure: read the named key from `mutants.out/schemata.json`, or sum
`phase_results[].duration` by phase in `outcomes.json` for the mutants named in
`fallback_mutants` with the reason.

## Dependencies

None in this repository. `jast-platform`'s `mutants-test-waits` item cuts the embedded-test half of
the same run. The two are independent.
