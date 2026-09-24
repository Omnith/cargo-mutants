// Copyright 2026 Martin Pool

//! Decide whether collecting coverage pays off, for `--test-selection=auto`.

#![warn(clippy::pedantic)]

use serde::Serialize;
use tracing::debug;

use crate::outcome::Phase;
use crate::schemata::jobs::Probe;
use crate::schemata::run::Pass;

/// The fraction of the time that running all the tests for every embedded mutant
/// would take, estimated as one run of the unmutated tests each, that
/// coverage-based selection is expected to save.
///
/// It's less than all of it because missed mutants still run all the tests, and
/// because without coverage, caught mutants stop at the first failed test. Measured
/// on backend-core, with tests stopped at the first failure: 0.58 with
/// `--shard 0/20` (91 embedded mutants, 81 caught), and 0.54 for 9 mutants (7
/// caught); rounded to 0.55.
pub(crate) const SAVED_FRACTION: f64 = 0.55;

/// How many times longer building the workspace's crates with coverage
/// instrumentation, and listing their tests, takes than the schema's rebuild of them.
///
/// Measured on backend-core: 25.7-26.2 s against a rebuild of 18.7-19.0 s.
pub(crate) const INSTRUMENTED_BUILD_FACTOR: f64 = 1.4;

/// Seconds that running one test alone while collecting coverage costs, besides the
/// test's own time: starting its process, writing its profile, and reading it with
/// `llvm-profdata`.
///
/// Measured on backend-core: 818 tests ran alone in 8.3-8.5 s with 8 at once, about
/// 0.08 s each, nearly all of it this overhead, since the whole suite runs in 3.8 s;
/// rounded up.
pub(crate) const ISOLATED_TEST_OVERHEAD_SECONDS: f64 = 0.1;

/// Measured costs that decide whether to collect coverage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct CostInputs {
    /// Embedded mutants to test.
    pub mutants: usize,
    /// Wall seconds that running all the tests adds for each mutant, allowing for
    /// the mutants tested at once (see [`mutant_seconds`]).
    pub mutant_seconds: f64,
    /// Estimated seconds to rebuild the workspace's crates (see [`rebuild_seconds`]).
    pub rebuild_seconds: f64,
    /// Number of tests, each run alone while collecting coverage.
    pub tests: usize,
    /// Seconds to run all the tests once with no mutant active.
    pub suite_seconds: f64,
    /// Number of tests run alone at once while collecting coverage.
    pub workers: usize,
}

/// The estimates, and whether to collect coverage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Decision {
    #[serde(flatten)]
    pub inputs: CostInputs,
    /// Running all the tests for every embedded mutant.
    pub all_tests_seconds: f64,
    /// The part of that which coverage-based selection is expected to save.
    pub saved_seconds: f64,
    /// Building the workspace's crates with coverage instrumentation and listing the
    /// tests.
    pub instrumented_build_seconds: f64,
    /// Running each test alone while collecting coverage.
    pub isolated_tests_seconds: f64,
    /// Collecting coverage: rebuilding with instrumentation and running each test
    /// alone.
    pub collect_seconds: f64,
    /// True if the saving is larger than the cost of collecting coverage.
    pub collect: bool,
}

/// Decide whether to collect coverage: only if the time it's expected to save
/// testing the embedded mutants is more than the time it takes.
///
/// Collecting coverage costs a rebuild of the workspace's crates with
/// instrumentation, reusing the dependencies already built for the schema, and a
/// run of each test alone. It saves most of the time spent running all the tests for
/// each mutant, but not all: missed mutants are still confirmed with all the tests.
#[expect(clippy::cast_precision_loss, reason = "counts are far below 2^52")]
pub(crate) fn decide(inputs: CostInputs) -> Decision {
    let all_tests_seconds = inputs.mutants as f64 * inputs.mutant_seconds;
    let saved_seconds = all_tests_seconds * SAVED_FRACTION;
    let isolated_tests_seconds = (inputs.tests as f64 * ISOLATED_TEST_OVERHEAD_SECONDS
        + inputs.suite_seconds)
        / inputs.workers.max(1) as f64;
    let instrumented_build_seconds = inputs.rebuild_seconds * INSTRUMENTED_BUILD_FACTOR;
    let collect_seconds = instrumented_build_seconds + isolated_tests_seconds;
    Decision {
        inputs,
        all_tests_seconds,
        saved_seconds,
        instrumented_build_seconds,
        isolated_tests_seconds,
        collect_seconds,
        collect: saved_seconds > collect_seconds,
    }
}

impl Decision {
    /// Emit the decision and its inputs as a `schemata.coverage.decision` event.
    pub(crate) fn trace(&self) {
        let CostInputs {
            mutants,
            mutant_seconds,
            rebuild_seconds,
            tests,
            suite_seconds,
            workers,
        } = self.inputs;
        debug!(
            mutants,
            mutant_seconds,
            rebuild_seconds,
            tests,
            suite_seconds,
            workers,
            all_tests_seconds = self.all_tests_seconds,
            saved_seconds = self.saved_seconds,
            instrumented_build_seconds = self.instrumented_build_seconds,
            isolated_tests_seconds = self.isolated_tests_seconds,
            collect_seconds = self.collect_seconds,
            collect = self.collect,
            "schemata.coverage.decision"
        );
    }
}

/// Estimate the seconds to rebuild the workspace's crates, from the schema's check
/// and build passes.
///
/// If the check-and-drop loop built more than once, its last build rebuilt only the
/// workspace's crates, after dropping mutants. Otherwise the schema's whole build is
/// used, which also built the dependencies, and so overestimates the rebuild.
pub(crate) fn rebuild_seconds(check_passes: &[Pass], build_passes: &[Pass]) -> f64 {
    match check_passes {
        [_, .., last] if last.phase == Phase::Build && last.success => last.seconds,
        _ => check_passes
            .iter()
            .chain(build_passes)
            .map(|pass| pass.seconds)
            .sum(),
    }
}

/// Wall seconds that running all the tests adds for each mutant, from `concurrent`,
/// a run of the unmutated tests of all `selections` in as many copies at once as
/// mutants will be tested, or else `suite_seconds`, one such run alone.
///
/// Each mutant runs the tests of only one selection, so the time is shared among
/// them, as if they took equally long.
#[expect(clippy::cast_precision_loss, reason = "counts are far below 2^52")]
pub(crate) fn mutant_seconds(
    concurrent: Option<&Probe>,
    suite_seconds: f64,
    selections: usize,
) -> f64 {
    let per_run = concurrent.map_or(suite_seconds, |probe| {
        probe.seconds / probe.jobs.max(1) as f64
    });
    per_run / selections.max(1) as f64
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "expected values are sums and halvings of the inputs, computed exactly"
)]
mod test {
    use super::*;

    /// Costs measured on backend-core, a crate whose 818 tests take 3.8 s, of which
    /// two copies run at once in 6.5 s, and whose schema took 70 s to build at once.
    fn backend_core(mutants: usize) -> CostInputs {
        CostInputs {
            mutants,
            mutant_seconds: 6.5 / 2.0,
            rebuild_seconds: 70.0,
            tests: 818,
            suite_seconds: 3.8,
            workers: 8,
        }
    }

    #[test]
    fn decide_estimates_collection_as_the_instrumented_rebuild_plus_the_isolated_tests() {
        let decision = decide(backend_core(50));
        assert!(
            decision.instrumented_build_seconds >= decision.inputs.rebuild_seconds,
            "{decision:#?}"
        );
        assert_eq!(
            decision.collect_seconds,
            decision.instrumented_build_seconds + decision.isolated_tests_seconds
        );
    }

    #[test]
    fn decide_skips_coverage_for_ten_mutants_of_backend_core() {
        let decision = decide(backend_core(9));
        assert!(!decision.collect, "{decision:#?}");
    }

    #[test]
    fn decide_collects_coverage_for_a_hundred_mutants_of_backend_core() {
        let decision = decide(backend_core(91));
        assert!(decision.collect, "{decision:#?}");
    }

    #[test]
    fn decide_collects_coverage_for_ten_mutants_of_a_slow_suite() {
        let decision = decide(CostInputs {
            mutants: 10,
            mutant_seconds: 60.0,
            suite_seconds: 60.0,
            tests: 200,
            ..backend_core(10)
        });
        assert!(decision.collect, "{decision:#?}");
    }

    #[test]
    fn decide_collects_exactly_when_the_saving_exceeds_the_cost_which_grows_with_mutants() {
        let decisions = (0..300)
            .map(|n| decide(backend_core(n)))
            .collect::<Vec<_>>();
        for decision in &decisions {
            assert_eq!(
                decision.collect,
                decision.saved_seconds > decision.collect_seconds,
                "{decision:#?}"
            );
            assert!(
                decision.saved_seconds < decision.all_tests_seconds || decision.inputs.mutants == 0
            );
        }
        let first = decisions
            .iter()
            .position(|d| d.collect)
            .expect("some count collects");
        assert!(decisions[first..].iter().all(|d| d.collect));
    }

    fn pass(iteration: usize, phase: Phase, seconds: f64, success: bool) -> Pass {
        Pass {
            iteration,
            phase,
            seconds,
            success,
            errors: 0,
            unattributed_errors: 0,
            dropped: 0,
            embedded_after: 0,
            logs: Vec::new(),
        }
    }

    #[test]
    fn rebuild_seconds_is_the_last_build_of_the_check_loop_after_dropping_mutants() {
        // The first pass also built the dependencies; after dropping mutants, only
        // the workspace's crates were rebuilt.
        let check = [
            pass(1, Phase::Build, 70.0, false),
            pass(2, Phase::Build, 30.0, true),
        ];
        let build = [pass(1, Phase::Build, 1.0, true)];
        assert_eq!(rebuild_seconds(&check, &build), 30.0);
    }

    #[test]
    fn rebuild_seconds_is_the_whole_schema_build_when_it_built_once() {
        let check = [pass(1, Phase::Build, 70.0, true)];
        let build = [pass(1, Phase::Build, 1.0, true)];
        assert_eq!(rebuild_seconds(&check, &build), 71.0);
        let check = [
            pass(1, Phase::Check, 20.0, false),
            pass(2, Phase::Check, 5.0, true),
        ];
        let build = [pass(1, Phase::Build, 50.0, true)];
        assert_eq!(rebuild_seconds(&check, &build), 75.0);
    }

    #[test]
    fn mutant_seconds_divides_a_concurrent_run_by_its_jobs_and_the_selections() {
        let probe = Probe {
            jobs: 2,
            seconds: 4.5,
            passed: true,
        };
        assert_eq!(mutant_seconds(Some(&probe), 3.2, 1), 2.25);
        assert_eq!(mutant_seconds(Some(&probe), 3.2, 3), 0.75);
        assert_eq!(mutant_seconds(None, 3.2, 2), 1.6);
    }
}
