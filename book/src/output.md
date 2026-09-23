# Display and output

cargo-mutants writes a list of missed or timed-out mutants to stderr, and optionally mutants that were caught (with `--caught`) or failed to build (with `--unviable`) to stdout. It writes error or debug messages to stderr.

The following options control what is printed to stdout and stderr.

`-v`, `--caught`: Also print mutants that were caught by tests.

`-V`, `--unviable`: Also print mutants that failed `cargo build`.

`--no-times`: Don't print elapsed times, or the timing breakdown at the end of the run. (This is intended mostly to make the output more stable for testing.)

## Timing breakdown

Unless `--no-times` is given, cargo-mutants prints a short breakdown of where the time went just before the final summary line, for example:

```text
Time: 31m cargo busy in 16m wall with 2 workers (97% utilized); baseline 11.0s build + 9.4s test
  build 104 mutants: median 10.3s, p95 18.0s, max 72.0s, total 19m
  test 91 mutants: median 9.2s, p95 11.3s, max 60.0s, total 12m
  by outcome: 78 caught 25m, 11 missed 4m, 13 unviable 52s, 2 timeout 2m
  slowest: 72.0s src/big.rs:4:5: replace build_table -> Table with Default::default() (unviable)
           71.2s src/net.rs:120:9: replace retry -> bool with false (timeout)
           40.1s src/parse.rs:88:21: replace < with <= in scan (caught)
```

- **cargo busy** is the total time spent running cargo, summed across all scenarios including the baseline. Comparing it to the wall-clock time multiplied by the number of worker threads (`--jobs`, capped at the number of mutants) shows how well the workers were kept busy. The remainder is time spent outside cargo, such as copying the tree, or workers waiting while the baseline runs.
- The per-phase lines cover only mutants, not the baseline. Percentiles use the nearest-rank method, so each value is the duration of some real mutant.
- **by outcome** shows how much time went to each kind of result. Time spent on unviable mutants and timeouts gives no useful signal about your tests, so if it's large, consider [skipping](skip.md) some functions or adjusting [timeouts](timeouts.md).
- **slowest** lists the three slowest mutants, including all their phases.

The same breakdown is always written to `mutants.out/debug.log` as structured `debug`-level events, even with `--no-times`, and it includes the five slowest mutants. The messages are `timing.lab`, `timing.phase`, `timing.baseline_phase`, `timing.outcome`, and `timing.slowest`, with durations in fields ending in `_secs`.

The breakdown isn't written to `outcomes.json`, because it can be computed from the `phase_results` already stored there for each scenario.

## Colors

`--colors=always|never|auto`: Control whether to use colors in output. The default is `auto`, which will write colors if the output is a terminal that supports colors. Color support is detected independently for stdout and stderr, so you should still see colors on stderr if stdout is redirected.

The same values can be set with the `CARGO_TERM_COLOR` environment variable, which is respected by many Cargo commands.

cargo-mutants also respects the `NO_COLOR` and [`CLICOLOR_FORCE`](https://bixense.com/clicolors/) environment variables. If they are set to a value other than `0` then colors will be disabled or enabled regardless of any other settings.

## Debug trace

`-L`, `--level`, and `$CARGO_MUTANTS_TRACE_LEVEL`: set the verbosity of trace output to stderr. The default is `info`, and it can be increased to `debug` or `trace`.
