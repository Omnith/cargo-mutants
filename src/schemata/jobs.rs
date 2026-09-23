// Copyright 2026 Martin Pool

//! Choose how many embedded mutants to test at once, when `--jobs` isn't given.
//!
//! How many concurrent test runs a machine can usefully hold depends on the tests
//! more than the number of CPUs: a suite that mostly waits for subprocesses or
//! timers uses little CPU but may contend for something else, like a system
//! service that every process start goes through. So rather than guess from the
//! CPU count or load, the tests are run with no mutant active, first alone and
//! then with twice as many copies at once each time, and concurrency grows only
//! while that raises throughput by at least [`MIN_GAIN`].

#![warn(clippy::pedantic)]

use serde::Serialize;

/// The smallest fractional throughput gain from doubling the number of jobs that
/// makes it worth doing.
///
/// On `backend-core`, going from one to two jobs raised throughput by 43%, and from
/// two to four by 1-2%, with run-to-run differences of a few percent; 15% is well
/// clear of both.
pub(crate) const MIN_GAIN: f64 = 0.15;

/// The most jobs to probe, on a machine with `cpus` CPUs, to test `mutants` mutants.
///
/// That's half the CPUs: the probe runs the tests with no mutant active, but some
/// mutants make a test spin until it times out, or until an arithmetic overflow,
/// using a whole CPU. Leaving headroom keeps those from starving each other, and
/// other tests, into spurious timeouts.
pub(crate) fn max_jobs(cpus: usize, mutants: usize) -> usize {
    (cpus / 2).max(1).min(mutants)
}

/// The measured throughput of some number of concurrent test runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Probe {
    pub jobs: usize,
    /// Wall time for all of them to finish.
    pub seconds: f64,
    /// True if they all passed.
    pub passed: bool,
}

impl Probe {
    #[expect(clippy::cast_precision_loss, reason = "numbers of jobs are small")]
    fn runs_per_second(&self) -> f64 {
        self.jobs as f64 / self.seconds.max(f64::MIN_POSITIVE)
    }
}

/// The next number of jobs to probe after `probes`, or `None` if probing is done.
///
/// `max_jobs` bounds the jobs probed.
pub(crate) fn next_probe(probes: &[Probe], max_jobs: usize) -> Option<usize> {
    let Some(last) = probes.last() else {
        return Some(1);
    };
    if !last.passed || chosen(probes) != last.jobs {
        return None;
    }
    let next = last.jobs * 2;
    (next <= max_jobs).then_some(next)
}

/// The number of jobs to use, given the probes run so far.
///
/// That's the last number that passed and gained enough throughput over the one
/// before; if the tests fail when run concurrently, perhaps because they write to
/// the same files, it's 1.
pub(crate) fn chosen(probes: &[Probe]) -> usize {
    if probes.iter().any(|probe| !probe.passed) {
        return 1;
    }
    let mut best: Option<&Probe> = None;
    for probe in probes {
        match best {
            Some(b) if probe.runs_per_second() < b.runs_per_second() * (1.0 + MIN_GAIN) => break,
            _ => best = Some(probe),
        }
    }
    best.map_or(1, |b| b.jobs)
}

#[cfg(test)]
mod test {
    use super::*;

    fn probe(jobs: usize, seconds: f64) -> Probe {
        Probe {
            jobs,
            seconds,
            passed: true,
        }
    }

    /// Probe with the given wall time for each number of jobs, as the caller does.
    fn run_probes(seconds_for_jobs: impl Fn(usize) -> Probe, max_jobs: usize) -> Vec<Probe> {
        let mut probes = Vec::new();
        while let Some(jobs) = next_probe(&probes, max_jobs) {
            probes.push(seconds_for_jobs(jobs));
        }
        probes
    }

    #[test]
    fn max_jobs_is_half_the_cpus_but_at_least_one_and_at_most_the_mutants() {
        assert_eq!(max_jobs(8, 1000), 4);
        assert_eq!(max_jobs(8, 3), 3);
        assert_eq!(max_jobs(1, 1000), 1);
        assert_eq!(max_jobs(3, 1000), 1);
    }

    #[test]
    fn next_probe_and_chosen_pick_two_jobs_when_throughput_plateaus_after_two() {
        // Times measured on backend-core: the suite takes 3.2 s alone, 4.5 s with
        // two copies, 9 s with four.
        let probes = run_probes(
            |jobs| match jobs {
                1 => probe(1, 3.2),
                2 => probe(2, 4.5),
                4 => probe(4, 9.0),
                _ => panic!("probed {jobs} jobs after throughput stopped rising"),
            },
            8,
        );
        assert_eq!(probes.iter().map(|p| p.jobs).collect::<Vec<_>>(), [1, 2, 4]);
        assert_eq!(chosen(&probes), 2);
    }

    #[test]
    #[expect(clippy::cast_precision_loss, reason = "numbers of jobs are small")]
    fn next_probe_and_chosen_keep_one_job_when_one_saturates_the_machine() {
        let probes = run_probes(|jobs| probe(jobs, 3.0 * jobs as f64), 8);
        assert_eq!(probes.len(), 2);
        assert_eq!(chosen(&probes), 1);
    }

    #[test]
    fn next_probe_and_chosen_stop_at_max_jobs_when_throughput_keeps_rising() {
        let probes = run_probes(|jobs| probe(jobs, 3.0), 6);
        assert_eq!(probes.iter().map(|p| p.jobs).collect::<Vec<_>>(), [1, 2, 4]);
        assert_eq!(chosen(&probes), 4);
        assert_eq!(next_probe(&[], 1), Some(1));
        assert_eq!(next_probe(&[probe(1, 3.0)], 1), None);
    }

    #[test]
    fn next_probe_and_chosen_use_one_job_when_concurrent_tests_fail() {
        let probes = run_probes(
            |jobs| Probe {
                jobs,
                seconds: 3.0,
                passed: jobs < 4,
            },
            8,
        );
        assert_eq!(probes.iter().map(|p| p.jobs).collect::<Vec<_>>(), [1, 2, 4]);
        assert_eq!(chosen(&probes), 1);
    }
}
