// Copyright 2026 Martin Pool

//! A breakdown of where time went during a lab run.
//!
//! This is computed from the per-phase durations already recorded in a
//! [`LabOutcome`], so it has no effect on how mutants are tested.

use std::cmp::Reverse;
use std::time::Duration;

use tracing::debug;

use crate::outcome::{LabOutcome, Phase, SummaryOutcome};

/// How many of the slowest mutant scenarios are included in the breakdown.
pub const SLOWEST_SCENARIOS: usize = 5;

/// Summary statistics for one phase across all mutant scenarios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseStats {
    pub phase: Phase,
    /// Number of mutant scenarios that ran this phase.
    pub count: usize,
    pub total: Duration,
    pub median: Duration,
    pub p95: Duration,
    pub max: Duration,
}

/// Time spent on mutants that had a given summary outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeTime {
    pub outcome: SummaryOutcome,
    /// Number of mutants with this outcome.
    pub count: usize,
    /// Sum of all phase durations for those mutants.
    pub total: Duration,
}

/// One of the slowest mutant scenarios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowScenario {
    /// The scenario name, as shown on the console, including line and column.
    pub name: String,
    pub outcome: SummaryOutcome,
    /// Sum of all phase durations for this scenario.
    pub total: Duration,
}

/// Where the time went during a lab run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingBreakdown {
    /// Statistics for each phase run by mutant scenarios (excluding the baseline),
    /// in phase order, omitting phases that never ran.
    pub phases: Vec<PhaseStats>,
    /// Duration of each phase of the baseline, if it was run.
    pub baseline: Vec<(Phase, Duration)>,
    /// Time attributed to each summary outcome of mutant scenarios, in a fixed order,
    /// omitting outcomes that never occurred.
    pub outcomes: Vec<OutcomeTime>,
    /// Up to [`SLOWEST_SCENARIOS`] mutant scenarios, slowest first.
    pub slowest: Vec<SlowScenario>,
    /// Total time cargo was running, summed across all scenarios including the baseline.
    pub cargo_busy: Duration,
}

impl TimingBreakdown {
    /// Compute the breakdown from all the scenario outcomes recorded so far.
    pub fn from_lab_outcome(lab_outcome: &LabOutcome) -> TimingBreakdown {
        let mut baseline: Vec<(Phase, Duration)> = Vec::new();
        let mut phase_durations: [Vec<Duration>; 3] = Default::default();
        let mut outcomes: Vec<OutcomeTime> = OUTCOME_ORDER
            .iter()
            .map(|outcome| OutcomeTime {
                outcome: outcome.clone(),
                count: 0,
                total: Duration::ZERO,
            })
            .collect();
        let mut slowest: Vec<SlowScenario> = Vec::new();
        let mut cargo_busy = Duration::ZERO;
        for scenario_outcome in &lab_outcome.outcomes {
            let phase_results = scenario_outcome.phase_results();
            let total: Duration = phase_results.iter().map(|pr| pr.duration).sum();
            cargo_busy += total;
            if !scenario_outcome.scenario.is_mutant() {
                baseline.extend(phase_results.iter().map(|pr| (pr.phase, pr.duration)));
                continue;
            }
            for pr in phase_results {
                phase_durations[phase_index(pr.phase)].push(pr.duration);
            }
            let summary = scenario_outcome.summary();
            let outcome_time = outcomes
                .iter_mut()
                .find(|ot| ot.outcome == summary)
                .expect("every summary outcome is in OUTCOME_ORDER");
            outcome_time.count += 1;
            outcome_time.total += total;
            slowest.push(SlowScenario {
                name: scenario_outcome.scenario.to_string(),
                outcome: summary,
                total,
            });
        }
        // Stable sort, so ties stay in the order they finished.
        slowest.sort_by_key(|slow| Reverse(slow.total));
        slowest.truncate(SLOWEST_SCENARIOS);
        outcomes.retain(|ot| ot.count > 0);
        let phases = PHASE_ORDER
            .into_iter()
            .zip(phase_durations)
            .filter(|(_, durations)| !durations.is_empty())
            .map(|(phase, mut durations)| {
                durations.sort_unstable();
                PhaseStats {
                    phase,
                    count: durations.len(),
                    total: durations.iter().sum(),
                    median: percentile(&durations, 50),
                    p95: percentile(&durations, 95),
                    max: *durations.last().expect("durations is not empty"),
                }
            })
            .collect();
        TimingBreakdown {
            phases,
            baseline,
            outcomes,
            slowest,
            cargo_busy,
        }
    }

    /// The fraction of available worker time that cargo was busy.
    ///
    /// Returns `None` if the wall-clock time is zero or the capacity overflows.
    pub fn utilization(&self, workers: usize, wall: Duration) -> Option<f64> {
        let capacity = wall.checked_mul(u32::try_from(workers).ok()?)?;
        if capacity.is_zero() {
            None
        } else {
            Some(self.cargo_busy.as_secs_f64() / capacity.as_secs_f64())
        }
    }

    /// Emit the breakdown as structured debug events, which land in `debug.log`.
    ///
    /// The messages and field names are stable, so the log can be searched and parsed:
    ///
    /// - `timing.lab`: `cargo_busy_secs`, `wall_secs`, `workers`, and `utilization`
    ///   (a fraction, omitted if the wall time is zero).
    /// - `timing.phase`, once per phase run by mutants: `phase`, `count`, `total_secs`,
    ///   `median_secs`, `p95_secs`, `max_secs`.
    /// - `timing.baseline_phase`, once per baseline phase: `phase`, `secs`.
    /// - `timing.outcome`, once per summary outcome: `outcome`, `count`, `total_secs`.
    /// - `timing.slowest`, once per slow mutant: `rank` (from 1), `scenario`, `outcome`,
    ///   `total_secs`.
    pub fn trace(&self, workers: usize, wall: Duration) {
        debug!(
            cargo_busy_secs = self.cargo_busy.as_secs_f64(),
            wall_secs = wall.as_secs_f64(),
            workers,
            utilization = self.utilization(workers, wall),
            "timing.lab"
        );
        for stats in &self.phases {
            debug!(
                phase = stats.phase.name(),
                count = stats.count,
                total_secs = stats.total.as_secs_f64(),
                median_secs = stats.median.as_secs_f64(),
                p95_secs = stats.p95.as_secs_f64(),
                max_secs = stats.max.as_secs_f64(),
                "timing.phase"
            );
        }
        for (phase, duration) in &self.baseline {
            debug!(
                phase = phase.name(),
                secs = duration.as_secs_f64(),
                "timing.baseline_phase"
            );
        }
        for outcome_time in &self.outcomes {
            debug!(
                outcome = ?outcome_time.outcome,
                count = outcome_time.count,
                total_secs = outcome_time.total.as_secs_f64(),
                "timing.outcome"
            );
        }
        for (rank, slow) in (1..).zip(&self.slowest) {
            debug!(
                rank,
                scenario = slow.name,
                outcome = ?slow.outcome,
                total_secs = slow.total.as_secs_f64(),
                "timing.slowest"
            );
        }
    }
}

/// Phases in the order they run, which is the order they're reported.
const PHASE_ORDER: [Phase; 3] = [Phase::Check, Phase::Build, Phase::Test];

fn phase_index(phase: Phase) -> usize {
    match phase {
        Phase::Check => 0,
        Phase::Build => 1,
        Phase::Test => 2,
    }
}

/// Order in which outcomes are reported: the same order as the final summary line.
const OUTCOME_ORDER: [SummaryOutcome; 6] = [
    SummaryOutcome::CaughtMutant,
    SummaryOutcome::MissedMutant,
    SummaryOutcome::Unviable,
    SummaryOutcome::Timeout,
    SummaryOutcome::Success,
    SummaryOutcome::Failure,
];

/// Return the `pct` percentile of `sorted` using the nearest-rank method.
///
/// Every result is an observed value, so there's no interpolation between samples.
///
/// # Panics
///
/// If `sorted` is empty.
fn percentile(sorted: &[Duration], pct: usize) -> Duration {
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use crate::Options;
    use crate::outcome::{LabOutcome, Phase, PhaseResult, ScenarioOutcome, SummaryOutcome};
    use crate::process::Exit;
    use crate::scenario::Scenario;
    use crate::visit::mutate_source_str;
    use jiff::Timestamp;

    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn scenario_outcome(scenario: Scenario, phases: &[(Phase, u64, Exit)]) -> ScenarioOutcome {
        ScenarioOutcome {
            output_dir: "mutants.out".into(),
            log_path: "log".into(),
            diff_path: None,
            scenario,
            phase_results: phases
                .iter()
                .map(|&(phase, duration, process_status)| PhaseResult {
                    phase,
                    duration: secs(duration),
                    process_status,
                    argv: Vec::new(),
                })
                .collect(),
        }
    }

    fn mutant_scenarios(n: usize) -> Vec<Scenario> {
        let code =
            "fn a() -> u32 { 1 }\nfn b() -> u32 { 2 }\nfn c() -> u32 { 3 }\nfn d() -> u32 { 4 }\n";
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        assert!(mutants.len() >= n);
        mutants.into_iter().take(n).map(Scenario::Mutant).collect()
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let one_to_ten: Vec<Duration> = (1..=10).map(secs).collect();
        assert_eq!(percentile(&one_to_ten, 50), secs(5));
        assert_eq!(percentile(&one_to_ten, 95), secs(10));
        let one_to_twenty: Vec<Duration> = (1..=20).map(secs).collect();
        assert_eq!(percentile(&one_to_twenty, 50), secs(10));
        assert_eq!(percentile(&one_to_twenty, 95), secs(19));
        assert_eq!(percentile(&[secs(7)], 50), secs(7));
        assert_eq!(percentile(&[secs(7)], 95), secs(7));
    }

    #[test]
    fn from_lab_outcome_of_empty_lab_is_empty() {
        let breakdown = TimingBreakdown::from_lab_outcome(&LabOutcome::new(Timestamp::now()));
        assert_eq!(
            breakdown,
            TimingBreakdown {
                phases: Vec::new(),
                baseline: Vec::new(),
                outcomes: Vec::new(),
                slowest: Vec::new(),
                cargo_busy: Duration::ZERO,
            }
        );
    }

    #[test]
    fn from_lab_outcome_excludes_baseline_from_mutant_stats() {
        let mut lab_outcome = LabOutcome::new(Timestamp::now());
        lab_outcome.add(scenario_outcome(
            Scenario::Baseline,
            &[
                (Phase::Build, 100, Exit::Success),
                (Phase::Test, 50, Exit::Success),
            ],
        ));
        let mut scenarios = mutant_scenarios(2).into_iter();
        lab_outcome.add(scenario_outcome(
            scenarios.next().unwrap(),
            &[
                (Phase::Build, 2, Exit::Success),
                (Phase::Test, 3, Exit::Failure(101)),
            ],
        ));
        lab_outcome.add(scenario_outcome(
            scenarios.next().unwrap(),
            &[
                (Phase::Build, 4, Exit::Success),
                (Phase::Test, 5, Exit::Failure(101)),
            ],
        ));

        let breakdown = TimingBreakdown::from_lab_outcome(&lab_outcome);

        assert_eq!(
            breakdown.baseline,
            [(Phase::Build, secs(100)), (Phase::Test, secs(50))]
        );
        assert_eq!(
            breakdown.phases,
            [
                PhaseStats {
                    phase: Phase::Build,
                    count: 2,
                    total: secs(6),
                    median: secs(2),
                    p95: secs(4),
                    max: secs(4),
                },
                PhaseStats {
                    phase: Phase::Test,
                    count: 2,
                    total: secs(8),
                    median: secs(3),
                    p95: secs(5),
                    max: secs(5),
                },
            ]
        );
        assert_eq!(
            breakdown.outcomes,
            [OutcomeTime {
                outcome: SummaryOutcome::CaughtMutant,
                count: 2,
                total: secs(14),
            }]
        );
        assert!(breakdown.slowest.iter().all(|s| s.name != "baseline"));
        assert_eq!(breakdown.cargo_busy, secs(150 + 14));
    }

    #[test]
    fn from_lab_outcome_attributes_time_to_unviable_and_timeout() {
        let mut lab_outcome = LabOutcome::new(Timestamp::now());
        let scenarios = mutant_scenarios(4);
        let names: Vec<String> = scenarios.iter().map(ToString::to_string).collect();
        let mut scenarios = scenarios.into_iter();
        lab_outcome.add(scenario_outcome(
            scenarios.next().unwrap(),
            &[(Phase::Build, 4, Exit::Failure(101))],
        ));
        lab_outcome.add(scenario_outcome(
            scenarios.next().unwrap(),
            &[
                (Phase::Build, 1, Exit::Success),
                (Phase::Test, 20, Exit::Timeout),
            ],
        ));
        lab_outcome.add(scenario_outcome(
            scenarios.next().unwrap(),
            &[
                (Phase::Build, 1, Exit::Success),
                (Phase::Test, 1, Exit::Success),
            ],
        ));
        lab_outcome.add(scenario_outcome(
            scenarios.next().unwrap(),
            &[
                (Phase::Build, 2, Exit::Success),
                (Phase::Test, 3, Exit::Failure(101)),
            ],
        ));

        let breakdown = TimingBreakdown::from_lab_outcome(&lab_outcome);

        assert_eq!(
            breakdown.outcomes,
            [
                OutcomeTime {
                    outcome: SummaryOutcome::CaughtMutant,
                    count: 1,
                    total: secs(5),
                },
                OutcomeTime {
                    outcome: SummaryOutcome::MissedMutant,
                    count: 1,
                    total: secs(2),
                },
                OutcomeTime {
                    outcome: SummaryOutcome::Unviable,
                    count: 1,
                    total: secs(4),
                },
                OutcomeTime {
                    outcome: SummaryOutcome::Timeout,
                    count: 1,
                    total: secs(21),
                },
            ]
        );
        assert_eq!(
            breakdown.slowest,
            [
                SlowScenario {
                    name: names[1].clone(),
                    outcome: SummaryOutcome::Timeout,
                    total: secs(21),
                },
                SlowScenario {
                    name: names[3].clone(),
                    outcome: SummaryOutcome::CaughtMutant,
                    total: secs(5),
                },
                SlowScenario {
                    name: names[0].clone(),
                    outcome: SummaryOutcome::Unviable,
                    total: secs(4),
                },
                SlowScenario {
                    name: names[2].clone(),
                    outcome: SummaryOutcome::MissedMutant,
                    total: secs(2),
                },
            ]
        );
        assert_eq!(breakdown.baseline, []);
        assert_eq!(breakdown.cargo_busy, secs(32));
    }

    #[test]
    fn from_lab_outcome_keeps_only_slowest_scenarios_count() {
        let mut lab_outcome = LabOutcome::new(Timestamp::now());
        for (i, scenario) in (1..).zip(mutant_scenarios(SLOWEST_SCENARIOS + 2)) {
            lab_outcome.add(scenario_outcome(
                scenario,
                &[(Phase::Build, i, Exit::Failure(101))],
            ));
        }
        let totals: Vec<Duration> = TimingBreakdown::from_lab_outcome(&lab_outcome)
            .slowest
            .iter()
            .map(|s| s.total)
            .collect();
        assert_eq!(totals, [secs(7), secs(6), secs(5), secs(4), secs(3)]);
    }

    #[test]
    fn utilization_is_cargo_busy_over_workers_times_wall() {
        let breakdown = TimingBreakdown {
            phases: Vec::new(),
            baseline: Vec::new(),
            outcomes: Vec::new(),
            slowest: Vec::new(),
            cargo_busy: secs(18),
        };
        assert_eq!(breakdown.utilization(2, secs(10)), Some(0.9));
        assert_eq!(breakdown.utilization(2, Duration::ZERO), None);
    }
}
