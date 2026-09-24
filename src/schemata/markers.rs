// Copyright 2026 Martin Pool

//! Read the facts that the schema's helper module records at runtime, as files in a
//! marker directory: which mutants were reached, which processes ran schema code
//! without the mutant id variable, and which sites ran with no mutant active. See
//! [`super::generate::helper_module`].

#![warn(clippy::pedantic)]

use std::collections::{BTreeSet, HashSet};
use std::fs::{read_dir, read_to_string, remove_file};

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use tempfile::TempDir;

use super::generate::MutantId;
use crate::Result;

/// What the processes that ran with no mutant active recorded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BaselineRuns {
    /// Number of processes that saw the mutant id variable, set to 0, and ran schema
    /// code. If there are none, it's not known whether tests see the variable.
    pub processes: usize,
    /// Ids of the mutants whose sites ran in any of them.
    pub ran: HashSet<MutantId>,
}

/// A temporary directory where schema processes record facts, removed on drop.
///
/// It's outside the build directory so tests don't see it, and only the current
/// user can write to it.
#[derive(Debug)]
pub(crate) struct Markers {
    _dir: TempDir,
    path: Utf8PathBuf,
}

impl Markers {
    pub(crate) fn new() -> Result<Markers> {
        let dir = TempDir::with_prefix("cargo-mutants-schemata-markers-")
            .context("create schemata marker directory")?;
        let path = Utf8PathBuf::try_from(dir.path().to_owned())
            .context("schemata marker directory path is not UTF-8")?;
        Ok(Markers { _dir: dir, path })
    }

    pub(crate) fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// True if the code of mutant `id` ran, in a process where it was active.
    pub(crate) fn reached(&self, id: MutantId) -> bool {
        self.path.join(format!("reached-{id}")).exists()
    }

    /// What processes that saw the mutant id variable set to 0 recorded: which
    /// mutants' sites ran with no mutant active.
    ///
    /// Each process appends lines of ids, separated by spaces, to its own file.
    pub(crate) fn ran_in_baseline(&self) -> Result<BaselineRuns> {
        let mut runs = BaselineRuns::default();
        for entry in read_dir(&self.path).with_context(|| format!("read {}", self.path))? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("baseline-") {
                runs.processes += 1;
                let text = read_to_string(entry.path())?;
                runs.ran.extend(
                    text.split_whitespace()
                        .filter_map(|id| id.parse::<MutantId>().ok()),
                );
            }
        }
        Ok(runs)
    }

    /// Forget what processes that ran with no mutant active recorded, before the
    /// baseline is run again with a different schema, so that [`Self::ran_in_baseline`]
    /// only reports that run.
    ///
    /// Records of processes that ran without the mutant id are kept: the same tests run
    /// again, and a build script that ran schema code might not run again.
    pub(crate) fn clear_baseline_runs(&self) -> Result<()> {
        for entry in read_dir(&self.path).with_context(|| format!("read {}", self.path))? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("baseline-") {
                remove_file(entry.path())
                    .with_context(|| format!("remove {}", entry.path().display()))?;
            }
        }
        Ok(())
    }

    /// The executables of processes that ran schema code without the mutant id
    /// variable, so ran the unmutated code whatever the mutant.
    pub(crate) fn env_cleared_executables(&self) -> Result<BTreeSet<String>> {
        let mut executables = BTreeSet::new();
        for entry in read_dir(&self.path).with_context(|| format!("read {}", self.path))? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("unset-") {
                executables.insert(read_to_string(entry.path())?);
            }
        }
        Ok(executables)
    }
}

#[cfg(test)]
mod test {
    use std::fs::write;

    use super::*;

    #[test]
    fn markers_reached_and_env_cleared_executables_read_marker_files() {
        let markers = Markers::new().unwrap();
        assert!(!markers.reached(3));
        assert!(markers.env_cleared_executables().unwrap().is_empty());
        write(markers.path().join("reached-3"), "").unwrap();
        write(markers.path().join("unset-100"), "/t/debug/tool").unwrap();
        write(markers.path().join("unset-101"), "/t/debug/tool").unwrap();
        write(markers.path().join("unset-102"), "/t/debug/other").unwrap();
        assert!(markers.reached(3));
        assert!(!markers.reached(4));
        assert_eq!(
            markers.env_cleared_executables().unwrap(),
            BTreeSet::from(["/t/debug/other".to_owned(), "/t/debug/tool".to_owned()])
        );
    }

    #[test]
    fn markers_ran_in_baseline_reads_sites_recorded_by_each_process() {
        let markers = Markers::new().unwrap();
        assert_eq!(markers.ran_in_baseline().unwrap(), BaselineRuns::default());
        // Other markers aren't baseline processes.
        write(markers.path().join("reached-3"), "").unwrap();
        write(markers.path().join("unset-100"), "/t/debug/tool").unwrap();
        assert_eq!(markers.ran_in_baseline().unwrap(), BaselineRuns::default());
        write(markers.path().join("baseline-101"), "1 2 \n7 \n").unwrap();
        write(markers.path().join("baseline-102"), "2 \n9 \n").unwrap();
        // A process that saw the id but had run no site when it was killed.
        write(markers.path().join("baseline-103"), "").unwrap();
        assert_eq!(
            markers.ran_in_baseline().unwrap(),
            BaselineRuns {
                processes: 3,
                ran: HashSet::from([1, 2, 7, 9]),
            }
        );
    }

    #[test]
    fn markers_clear_baseline_runs_forgets_baseline_processes_only() {
        let markers = Markers::new().unwrap();
        write(markers.path().join("baseline-101"), "1 2 \n").unwrap();
        write(markers.path().join("unset-100"), "/t/debug/tool").unwrap();
        markers.clear_baseline_runs().unwrap();
        assert_eq!(markers.ran_in_baseline().unwrap(), BaselineRuns::default());
        assert_eq!(
            markers.env_cleared_executables().unwrap(),
            BTreeSet::from(["/t/debug/tool".to_owned()])
        );
    }
}
