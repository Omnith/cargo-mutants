// Copyright 2021-2026 Martin Pool

//! A `mutants.out` directory holding logs and other output.

use std::collections::{HashMap, hash_map::Entry};
use std::fs::{File, OpenOptions, create_dir, read_to_string, remove_dir_all, rename, write};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use fs4::fs_std::FileExt;
use jiff::Timestamp;
use path_slash::PathExt;
use serde::Serialize;
use tracing::{debug, error, info, trace, warn};

use crate::fail_fast::Rerun;
use crate::outcome::{LabOutcome, SummaryOutcome};
use crate::{Context, Mutant, Result, Scenario, ScenarioOutcome, check_interrupted};

const OUTDIR_NAME: &str = "mutants.out";
const ROTATED_NAME: &str = "mutants.out.old";
const LOCK_JSON: &str = "lock.json";
const LOCK_POLL: Duration = Duration::from_millis(100);
const OUTCOMES_JSON: &str = "outcomes.json";
/// Minimum time between rewrites of `outcomes.json` while mutants are being tested.
///
/// Every rewrite serializes all outcomes so far, under the lock shared by all workers, so
/// rewriting after every mutant costs O(n^2): about 7 seconds in total for 2000 mutants.
/// One second keeps that overhead to a few milliseconds per second of run time, while
/// the file still lags the run by less than the time taken to test a typical mutant.
/// The baseline outcome and the final (or interrupted) state are always written promptly.
const OUTCOMES_JSON_WRITE_INTERVAL: Duration = Duration::from_secs(1);
/// How many times to retry replacing a JSON file while another process holds it open.
///
/// With the linearly increasing delay below, this waits at most 0.3 s in total.
const PERSIST_RETRIES: u32 = 5;
const PERSIST_RETRY_DELAY: Duration = Duration::from_millis(20);
static CAUGHT_TXT: &str = "caught.txt";
static MISSED_TXT: &str = "missed.txt";
static TIMEOUT_TXT: &str = "timeout.txt";
static PREVIOUSLY_CAUGHT_TXT: &str = "previously_caught.txt";
static UNVIABLE_TXT: &str = "unviable.txt";

/// The contents of a `lock.json` written into the output directory and used as
/// a lock file to ensure that two cargo-mutants invocations don't try to write
/// to the same `mutants.out` simultneously.
#[derive(Debug, Serialize)]
struct LockFile {
    cargo_mutants_version: String,
    start_time: Timestamp,
    hostname: String,
    username: String,
}

impl LockFile {
    fn new() -> LockFile {
        LockFile {
            cargo_mutants_version: crate::VERSION.to_string(),
            start_time: Timestamp::now(),
            hostname: whoami::fallible::hostname().unwrap_or_default(),
            username: whoami::username(),
        }
    }

    /// Block until acquiring a file lock on `lock.json` in the given `mutants.out`
    /// directory.
    ///
    /// Return the `File` whose lifetime controls the file lock.
    pub fn acquire_lock(output_dir: &Path) -> Result<File> {
        let lock_path = output_dir.join(LOCK_JSON);
        let mut lock_file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .context("open or create lock.json in existing directory")?;
        let mut first = true;
        while !lock_file
            .try_lock_exclusive()
            .context("try to lock lock.json")?
        {
            if first {
                info!("Waiting for lock on {} ...", lock_path.to_slash_lossy());
                first = false;
            }
            check_interrupted()?;
            sleep(LOCK_POLL);
        }
        lock_file.set_len(0)?;
        lock_file
            .write_all(serde_json::to_string_pretty(&LockFile::new())?.as_bytes())
            .context("write lock.json")?;
        Ok(lock_file)
    }
}

/// A `mutants.out` directory holding logs and other output information.
#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
pub struct OutputDir {
    path: Utf8PathBuf,

    #[allow(unused)] // Lifetime controls the file lock
    lock_file: File,
    /// Lists of missed, caught, timed out, and unviable mutants, as text, one per line.
    missed_list: OutcomeList,
    caught_list: OutcomeList,
    timeout_list: OutcomeList,
    unviable_list: OutcomeList,
    /// The position of each mutant's name in the order they were discovered, in which
    /// the lists are sorted when the run ends.
    discovery_order: HashMap<String, usize>,
    /// The accumulated overall lab outcome.
    pub lab_outcome: LabOutcome,
    /// Log filenames which have already been used, and the number of times that each
    /// basename has been used.
    used_log_names: HashMap<String, usize>,
    /// When `outcomes.json` was last written, if ever.
    outcomes_json_written_at: Option<Instant>,
    /// True if `lab_outcome` has changed since `outcomes.json` was last written.
    outcomes_json_stale: bool,
    /// How to rerun a mutant whose tests are stopped early, for its log.
    rerun: Rerun,
}

impl OutputDir {
    /// Create a new `mutants.out` output directory, within the given directory.
    ///
    /// If `in_dir` does not exist, it's created too, so that users can name a new directory
    /// with `--output`.
    ///
    /// If the directory already exists, it's rotated to `mutants.out.old`. If that directory
    /// exists, it's deleted.
    ///
    /// If the directory already exists and `lock.json` exists and is locked, this waits for
    /// the lock to be released. The returned `OutputDir` holds a lock for its lifetime.
    pub fn new(in_dir: &Utf8Path) -> Result<OutputDir> {
        if !in_dir.exists() {
            create_dir(in_dir)
                .with_context(|| format!("create output parent directory {in_dir:?}"))?;
        }
        let output_dir = in_dir.join(OUTDIR_NAME);
        if output_dir.exists() {
            LockFile::acquire_lock(output_dir.as_ref())?;
            // Now release the lock for a bit while we move the directory. This might be
            // slightly racy.
            // TODO: Move the lock outside the directory, <https://github.com/sourcefrog/cargo-mutants/issues/402>.

            let rotated = in_dir.join(ROTATED_NAME);
            if rotated.exists() {
                remove_dir_all(&rotated).with_context(|| format!("remove {rotated:?}"))?;
            }
            rename(&output_dir, &rotated)
                .with_context(|| format!("move {output_dir:?} to {rotated:?}"))?;
        }
        create_dir(&output_dir)
            .with_context(|| format!("create output directory {output_dir:?}"))?;
        let lock_file = LockFile::acquire_lock(output_dir.as_std_path())
            .context("create lock.json lock file")?;
        let log_dir = output_dir.join("log");
        create_dir(&log_dir).with_context(|| format!("create log directory {log_dir:?}"))?;
        let diff_dir = output_dir.join("diff");
        create_dir(diff_dir).context("create diff dir")?;

        Ok(OutputDir {
            missed_list: OutcomeList::create(&output_dir, MISSED_TXT)?,
            caught_list: OutcomeList::create(&output_dir, CAUGHT_TXT)?,
            timeout_list: OutcomeList::create(&output_dir, TIMEOUT_TXT)?,
            unviable_list: OutcomeList::create(&output_dir, UNVIABLE_TXT)?,
            discovery_order: HashMap::new(),
            path: output_dir,
            lab_outcome: LabOutcome::new(Timestamp::now()),
            lock_file,
            used_log_names: HashMap::new(),
            outcomes_json_written_at: None,
            outcomes_json_stale: false,
            rerun: Rerun::default(),
        })
    }

    /// Allocate a sequence number and the output files for a scenario.
    pub fn start_scenario(&mut self, scenario: &Scenario) -> Result<ScenarioOutput> {
        let scenario_name = match scenario {
            Scenario::Baseline => "baseline".into(),
            Scenario::Mutant(mutant) => mutant.log_file_name_base(),
        };
        let basename = self.unique_log_basename(scenario_name);
        let mut scenario_output = ScenarioOutput::new(&self.path, scenario, &basename)?;
        scenario_output.rerun_command = scenario.mutant().map(|m| self.rerun.command(m));
        Ok(scenario_output)
    }

    /// Remember the order in which `mutants` were discovered, in which the lists of
    /// mutants by outcome are finally written.
    pub fn set_discovery_order(&mut self, mutants: &[Mutant]) {
        self.discovery_order.clear();
        for (i, mutant) in mutants.iter().enumerate() {
            self.discovery_order.entry(mutant.name(true)).or_insert(i);
        }
    }

    /// Rewrite the lists of mutants by outcome in the order the mutants were discovered,
    /// rather than the order in which they finished.
    fn sort_outcome_lists(&mut self) -> Result<()> {
        for list in [
            &mut self.missed_list,
            &mut self.caught_list,
            &mut self.timeout_list,
            &mut self.unviable_list,
        ] {
            list.sort(&self.discovery_order)?;
        }
        Ok(())
    }

    /// Say how to rerun a mutant whose tests are stopped early, in its log.
    pub fn set_rerun(&mut self, rerun: Rerun) {
        self.rerun = rerun;
    }

    /// Open a log for a step that is not a scenario, such as a schemata check pass.
    pub fn start_log(&mut self, name: &str) -> Result<ScenarioOutput> {
        let basename = self.unique_log_basename(name.to_owned());
        ScenarioOutput::open(&self.path, &basename, None, name)
    }

    /// Return `name`, or if it's already been used, `name` with a unique suffix.
    fn unique_log_basename(&mut self, name: String) -> String {
        match self.used_log_names.entry(name.clone()) {
            Entry::Occupied(mut e) => {
                let index = e.get_mut();
                *index += 1;
                format!("{name}_{index:03}")
            }
            Entry::Vacant(e) => {
                e.insert(0);
                name
            }
        }
    }

    /// Return the path of the `mutants.out` directory.
    #[allow(unused)]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// Write `outcomes.json` from the current in-memory state.
    ///
    /// Called multiple times as the lab runs, and once at the end.
    fn write_lab_outcome(&mut self) -> Result<()> {
        let start = Instant::now();
        replace_json_file(&self.path, OUTCOMES_JSON, &self.lab_outcome)?;
        self.outcomes_json_written_at = Some(Instant::now());
        self.outcomes_json_stale = false;
        debug!(
            n_outcomes = self.lab_outcome.outcomes.len(),
            elapsed = ?start.elapsed(),
            "wrote outcomes.json"
        );
        Ok(())
    }

    /// Add the result of testing one scenario.
    ///
    /// `outcomes.json` is rewritten at most once per [`OUTCOMES_JSON_WRITE_INTERVAL`], except
    /// that the baseline outcome is written immediately. Any outcomes not yet written are
    /// written by [`OutputDir::finish`], or when the `OutputDir` is dropped.
    pub fn add_scenario_outcome(&mut self, scenario_outcome: &ScenarioOutcome) -> Result<()> {
        self.lab_outcome.add(scenario_outcome.to_owned());
        self.outcomes_json_stale = true;
        let due = self
            .outcomes_json_written_at
            .is_none_or(|t| t.elapsed() >= OUTCOMES_JSON_WRITE_INTERVAL);
        if (due || !scenario_outcome.scenario.is_mutant())
            && let Err(err) = self.write_lab_outcome()
        {
            // The outcomes stay marked as unwritten, so a later write or `finish` retries;
            // only a failure to write the final state is an error.
            warn!("Failed to update outcomes.json, will retry: {err:#}");
        }
        let scenario = &scenario_outcome.scenario;
        if let Scenario::Mutant(mutant) = scenario {
            let list = match scenario_outcome.summary() {
                SummaryOutcome::MissedMutant => &mut self.missed_list,
                SummaryOutcome::CaughtMutant => &mut self.caught_list,
                SummaryOutcome::Timeout => &mut self.timeout_list,
                SummaryOutcome::Unviable => &mut self.unviable_list,
                _ => return Ok(()),
            };
            list.add(mutant.name(true))?;
        }
        Ok(())
    }

    pub fn open_debug_log(&self) -> Result<File> {
        let debug_log_path = self.path.join("debug.log");
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&debug_log_path)
            .with_context(|| format!("open {debug_log_path}"))
    }

    pub fn write_mutants_list(&self, mutants: &[Mutant]) -> Result<()> {
        write(
            self.path.join("mutants.json"),
            crate::list::mutants_to_json_string(mutants),
        )
        .context("write mutants.json")
    }

    /// Mark the lab as finished and return the outcome.
    pub fn finish(mut self) -> Result<LabOutcome> {
        // Maybe it's weird to hold it in the directory object? Should we have a
        // higher level "accumulator" object?
        self.lab_outcome.end_time = Some(Timestamp::now());
        self.write_lab_outcome()?;
        self.sort_outcome_lists()?;
        // Can't move out of a type that implements Drop, so leave an empty outcome behind.
        // It's not stale, so it won't be written.
        Ok(std::mem::replace(
            &mut self.lab_outcome,
            LabOutcome::new(Timestamp::now()),
        ))
    }

    pub fn write_previously_caught(&self, caught: &[String]) -> Result<()> {
        let path = self.path.join(PREVIOUSLY_CAUGHT_TXT);
        let mut b = String::with_capacity(caught.iter().map(|l| l.len() + 1).sum());
        for l in caught {
            b.push_str(l);
            b.push('\n');
        }
        File::options()
            .create_new(true)
            .write(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b.as_bytes()))
            .with_context(|| format!("Write {path:?}"))
    }
}

impl Drop for OutputDir {
    /// Write any outcomes not yet in `outcomes.json`, and sort the lists of mutants.
    ///
    /// This happens when the run is interrupted or fails, and so `finish` is not called.
    fn drop(&mut self) {
        if self.outcomes_json_stale
            && let Err(err) = self.write_lab_outcome()
        {
            error!("Failed to write outcomes.json: {err:#}");
        }
        if let Err(err) = self.sort_outcome_lists() {
            error!("Failed to sort lists of mutants: {err:#}");
        }
    }
}

/// A text file listing the mutants with one outcome, one per line.
///
/// Names are appended as mutants finish, so that other programs can follow progress,
/// and the list is rewritten in the order the mutants were discovered when the run
/// ends, so that it doesn't depend on which mutants finished first.
#[derive(Debug)]
struct OutcomeList {
    file: File,
    file_name: &'static str,
    names: Vec<String>,
    /// True if names were added since the list was last sorted.
    unsorted: bool,
}

impl OutcomeList {
    fn create(output_dir: &Utf8Path, file_name: &'static str) -> Result<OutcomeList> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_dir.join(file_name))
            .with_context(|| format!("create {file_name}"))?;
        Ok(OutcomeList {
            file,
            file_name,
            names: Vec::new(),
            unsorted: false,
        })
    }

    fn add(&mut self, name: String) -> Result<()> {
        writeln!(self.file, "{name}").with_context(|| format!("write to {}", self.file_name))?;
        self.names.push(name);
        self.unsorted = true;
        Ok(())
    }

    /// Rewrite the list with names in the order of their position in `discovery_order`,
    /// followed by any names not in it, in the order they were added.
    fn sort(&mut self, discovery_order: &HashMap<String, usize>) -> Result<()> {
        if !self.unsorted {
            return Ok(());
        }
        self.names
            .sort_by_key(|name| discovery_order.get(name).copied().unwrap_or(usize::MAX));
        let mut text = String::with_capacity(self.names.iter().map(|name| name.len() + 1).sum());
        for name in &self.names {
            text.push_str(name);
            text.push('\n');
        }
        // The file is opened for appending, so after truncating it, this writes from the
        // start. Rewriting in place, rather than replacing the file, works on Windows
        // while the file is open.
        self.file
            .set_len(0)
            .and_then(|()| self.file.write_all(text.as_bytes()))
            .with_context(|| format!("rewrite {}", self.file_name))?;
        self.unsorted = false;
        Ok(())
    }
}

/// Write `value` as pretty JSON to the file `name` in `dir`, atomically replacing any
/// existing file, so that readers only ever see a complete file.
pub fn replace_json_file(dir: &Utf8Path, name: &str, value: &impl Serialize) -> Result<()> {
    let mut builder = tempfile::Builder::new();
    let prefix = format!(".{name}.");
    builder.prefix(&prefix).suffix(".tmp");
    // Same mode as `File::create`, which is then reduced by the umask, rather than
    // tempfile's default of 0o600.
    #[cfg(unix)]
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
    let mut temp = builder
        .tempfile_in(dir)
        .with_context(|| format!("create temporary {name}"))?;
    let mut writer = BufWriter::new(&mut temp);
    serde_json::to_writer_pretty(&mut writer, value).with_context(|| format!("write {name}"))?;
    writer.flush().with_context(|| format!("write {name}"))?;
    drop(writer);
    let path = dir.join(name);
    let mut retries = 0;
    while let Err(err) = temp.persist(&path) {
        if !replace_may_succeed_later(&err.error) || retries == PERSIST_RETRIES {
            return Err(err.error).with_context(|| format!("replace {name}"));
        }
        retries += 1;
        debug!(retries, error = %err.error, "{name} is busy, retrying");
        temp = err.file;
        sleep(PERSIST_RETRY_DELAY * retries);
    }
    Ok(())
}

/// True if replacing a file failed in a way that retrying might fix.
///
/// On Windows the rename is refused while another process has the old file open
/// without delete sharing, as many readers do, with either "access denied" or a
/// sharing violation.
fn replace_may_succeed_later(err: &std::io::Error) -> bool {
    /// Windows' `ERROR_SHARING_VIOLATION`.
    const ERROR_SHARING_VIOLATION: i32 = 32;
    err.kind() == ErrorKind::PermissionDenied
        || (cfg!(windows) && err.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

/// Return the string names of mutants previously caught in this output directory, including
/// unviable mutants.
///
/// Returns an empty vec if there are none.
pub fn load_previously_caught(output_parent_dir: &Utf8Path) -> Result<Vec<String>> {
    let mut r = Vec::new();
    for filename in [CAUGHT_TXT, UNVIABLE_TXT, PREVIOUSLY_CAUGHT_TXT] {
        let p = output_parent_dir.join(OUTDIR_NAME).join(filename);
        trace!(?p, "read previously caught");
        if p.is_file() {
            r.extend(
                read_to_string(&p)
                    .with_context(|| format!("Read previously caught mutants from {p:?}"))?
                    .lines()
                    .map(str::to_string),
            );
        }
    }
    Ok(r)
}

/// Where to write output about a particular Scenario.
#[allow(clippy::module_name_repetitions)]
pub struct ScenarioOutput {
    pub output_dir: Utf8PathBuf,
    log_path: Utf8PathBuf,
    pub log_file: File,
    /// File holding the diff of the mutated file, only if it's a mutation.
    pub diff_path: Option<Utf8PathBuf>,
    /// For a mutant, a command that tests only it, without stopping its tests early.
    pub rerun_command: Option<String>,
}

impl ScenarioOutput {
    fn new(output_dir: &Utf8Path, scenario: &Scenario, basename: &str) -> Result<Self> {
        let diff_path = if scenario.is_mutant() {
            Some(Utf8PathBuf::from(format!("diff/{basename}.diff")))
        } else {
            None
        };
        ScenarioOutput::open(output_dir, basename, diff_path, &scenario.to_string())
    }

    /// Create the log file, starting with a header message.
    fn open(
        output_dir: &Utf8Path,
        basename: &str,
        diff_path: Option<Utf8PathBuf>,
        header: &str,
    ) -> Result<Self> {
        let log_path = Utf8PathBuf::from(format!("log/{basename}.log"));
        let log_file = File::options()
            .append(true)
            .create_new(true)
            .read(true)
            .open(output_dir.join(&log_path))?;
        let mut scenario_output = Self {
            output_dir: output_dir.to_owned(),
            log_path,
            log_file,
            diff_path,
            rerun_command: None,
        };
        scenario_output.message(header)?;
        Ok(scenario_output)
    }

    pub fn log_path(&self) -> &Utf8Path {
        &self.log_path
    }

    pub fn write_diff(&mut self, diff: &str) -> Result<()> {
        self.message(&format!("mutation diff:\n{diff}"))?;
        let diff_path = self.diff_path.as_ref().expect("should know the diff path");
        write(self.output_dir.join(diff_path), diff.as_bytes())
            .with_context(|| format!("write diff to {diff_path}"))
    }

    /// Open a new handle reading from the start of the log file.
    pub fn open_log_read(&self) -> Result<File> {
        let path = self.output_dir.join(&self.log_path);
        OpenOptions::new()
            .read(true)
            .open(&path)
            .with_context(|| format!("reopen {path} for read"))
    }

    /// Open a new handle that appends to the log file, so that it can be passed to a subprocess.
    pub fn open_log_append(&self) -> Result<File> {
        let path = self.output_dir.join(&self.log_path);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("reopen {path} for append"))
    }

    /// Write a message, with a marker.
    pub fn message(&mut self, message: &str) -> Result<()> {
        write!(self.log_file, "\n*** {message}\n").context("write message to log")
    }
}

pub fn clean_filename(s: &str) -> String {
    s.replace('/', "__")
        .chars()
        .map(|c| match c {
            '\\' | ' ' | ':' | '<' | '>' | '?' | '*' | '|' | '"' => '_',
            c => c,
        })
        .collect::<String>()
}

#[cfg(test)]
mod test {
    use std::fs::write;

    use indoc::indoc;
    use itertools::Itertools;
    use pretty_assertions::assert_eq;
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn replace_may_succeed_later_for_permission_denied_or_windows_sharing_violation() {
        assert!(replace_may_succeed_later(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(!replace_may_succeed_later(&std::io::Error::from(
            ErrorKind::NotFound
        )));
        // 32 is ERROR_SHARING_VIOLATION on Windows, and something else elsewhere.
        assert_eq!(
            replace_may_succeed_later(&std::io::Error::from_raw_os_error(32)),
            cfg!(windows)
        );
    }

    fn minimal_source_tree() -> TempDir {
        let tmp = tempdir().unwrap();
        let path = tmp.path();
        write(
            path.join("Cargo.toml"),
            indoc! { br#"
                # enough for a test
                [package]
                name = "cargo-mutants-minimal-test-tree"
                version = "0.0.0"
                "#
            },
        )
        .unwrap();
        create_dir(path.join("src")).unwrap();
        write(path.join("src/lib.rs"), b"fn foo() {}").unwrap();
        tmp
    }

    fn list_recursive(path: &Path) -> Vec<String> {
        walkdir::WalkDir::new(path)
            .sort_by_file_name()
            .into_iter()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .strip_prefix(path)
                    .unwrap()
                    .to_slash_lossy()
                    .to_string()
            })
            .collect_vec()
    }

    #[test]
    fn clean_filename_removes_special_characters() {
        assert_eq!(
            clean_filename("1/2\\3:4<5>6?7*8|9\"0"),
            "1__2_3_4_5_6_7_8_9_0"
        );
    }

    #[test]
    fn create_output_dir() {
        let tmp = minimal_source_tree();
        let tmp_path: &Utf8Path = tmp.path().try_into().unwrap();
        let workspace = Workspace::open(tmp_path).unwrap();
        let output_dir = OutputDir::new(workspace.root()).unwrap();
        assert_eq!(
            list_recursive(tmp.path()),
            &[
                "",
                "Cargo.toml",
                "mutants.out",
                "mutants.out/caught.txt",
                "mutants.out/diff",
                "mutants.out/lock.json",
                "mutants.out/log",
                "mutants.out/missed.txt",
                "mutants.out/timeout.txt",
                "mutants.out/unviable.txt",
                "src",
                "src/lib.rs",
            ]
        );
        assert_eq!(output_dir.path(), workspace.root().join("mutants.out"));
        assert!(output_dir.path().join("lock.json").is_file());
    }

    #[test]
    fn rotate() {
        let temp_dir = TempDir::new().unwrap();
        let temp_dir_path = Utf8Path::from_path(temp_dir.path()).unwrap();

        // Create an initial output dir with one log.
        let mut output_dir = OutputDir::new(temp_dir_path).unwrap();
        let scenario_output = output_dir.start_scenario(&Scenario::Baseline).unwrap();
        assert!(temp_dir_path.join("mutants.out/log/baseline.log").is_file());
        drop(output_dir); // release the lock.
        drop(scenario_output);

        // The second time we create it in the same directory, the old one is moved away.
        let mut output_dir = OutputDir::new(temp_dir_path).unwrap();
        output_dir.start_scenario(&Scenario::Baseline).unwrap();
        assert!(
            temp_dir
                .path()
                .join("mutants.out.old/log/baseline.log")
                .is_file()
        );
        assert!(
            temp_dir
                .path()
                .join("mutants.out/log/baseline.log")
                .is_file()
        );
        drop(output_dir);

        // The third time (and later), the .old directory is removed.
        let mut output_dir = OutputDir::new(temp_dir_path).unwrap();
        output_dir.start_scenario(&Scenario::Baseline).unwrap();
        assert!(
            temp_dir
                .path()
                .join("mutants.out/log/baseline.log")
                .is_file()
        );
        assert!(
            temp_dir
                .path()
                .join("mutants.out.old/log/baseline.log")
                .is_file()
        );
        assert!(
            temp_dir
                .path()
                .join("mutants.out.old/log/baseline.log")
                .is_file()
        );
    }

    #[test]
    fn track_previously_caught() {
        let temp_dir = TempDir::new().unwrap();
        let parent = Utf8Path::from_path(temp_dir.path()).unwrap();

        let example = "src/process.rs:213:9: replace ProcessStatus::is_success -> bool with true
src/process.rs:248:5: replace get_command_output -> Result<String> with Ok(String::new())
";

        // Read from an empty dir: succeeds.
        assert_eq!(
            load_previously_caught(parent).expect("load succeeds"),
            [] as [String; 0]
        );

        let output_dir = OutputDir::new(parent).unwrap();
        assert_eq!(
            load_previously_caught(parent).expect("load succeeds"),
            [] as [String; 0]
        );

        write(parent.join("mutants.out/caught.txt"), example.as_bytes()).unwrap();
        let previously_caught = load_previously_caught(parent).expect("load succeeds");
        assert_eq!(
            previously_caught.iter().collect_vec(),
            example.lines().collect_vec()
        );

        // make a new output dir, moving away the old one, and write this
        drop(output_dir);
        let output_dir = OutputDir::new(parent).unwrap();
        output_dir
            .write_previously_caught(&previously_caught)
            .unwrap();
        assert_eq!(
            read_to_string(parent.join("mutants.out/caught.txt")).expect("read caught.txt"),
            ""
        );
        assert!(parent.join("mutants.out/previously_caught.txt").is_file());
        let now = load_previously_caught(parent).expect("load succeeds");
        assert_eq!(now.iter().collect_vec(), example.lines().collect_vec());
    }

    fn read_outcomes_json(output_dir: &Utf8Path) -> serde_json::Value {
        let json = read_to_string(output_dir.join("outcomes.json")).expect("read outcomes.json");
        serde_json::from_str(&json).expect("parse outcomes.json")
    }

    fn some_mutants() -> Vec<Mutant> {
        crate::visit::mutate_source_str(include_str!("outcome.rs"), &crate::Options::default())
            .unwrap()
    }

    fn baseline_outcome(output_dir: &mut OutputDir) -> ScenarioOutcome {
        use crate::outcome::{Phase, PhaseResult};
        use crate::process::Exit;
        let scenario_output = output_dir.start_scenario(&Scenario::Baseline).unwrap();
        let mut outcome = ScenarioOutcome::new(&scenario_output, Scenario::Baseline);
        outcome.add_phase_result(PhaseResult {
            phase: Phase::Test,
            duration: Duration::from_secs(1),
            process_status: Exit::Success,
            argv: vec!["cargo".into(), "test".into()],
        });
        outcome
    }

    #[test]
    fn add_scenario_outcome_writes_baseline_to_outcomes_json_immediately() {
        let temp_dir = TempDir::new().unwrap();
        let mut output_dir = OutputDir::new(temp_dir.path().try_into().unwrap()).unwrap();
        let mutants = some_mutants();
        // A mutant outcome first, so that the baseline is not the first write.
        let mutant_outcome = realistic_mutant_outcome(&mut output_dir, mutants[0].clone());
        output_dir.add_scenario_outcome(&mutant_outcome).unwrap();
        let baseline = baseline_outcome(&mut output_dir);
        output_dir.add_scenario_outcome(&baseline).unwrap();

        let json = read_outcomes_json(output_dir.path());
        assert_eq!(json["outcomes"].as_array().unwrap().len(), 2);
        assert_eq!(json["outcomes"][1]["scenario"], "Baseline");
    }

    /// Interrupted or failed runs drop the `OutputDir` without calling `finish`: all the
    /// outcomes recorded so far must still reach `outcomes.json`.
    #[test]
    fn dropping_output_dir_writes_all_outcomes_to_outcomes_json() {
        let temp_dir = TempDir::new().unwrap();
        let mut output_dir = OutputDir::new(temp_dir.path().try_into().unwrap()).unwrap();
        let path = output_dir.path().to_owned();
        let baseline = baseline_outcome(&mut output_dir);
        output_dir.add_scenario_outcome(&baseline).unwrap();
        let mutants = some_mutants();
        for mutant in &mutants {
            let outcome = realistic_mutant_outcome(&mut output_dir, mutant.clone());
            output_dir.add_scenario_outcome(&outcome).unwrap();
        }
        drop(output_dir);

        let json = read_outcomes_json(&path);
        assert_eq!(
            json["outcomes"].as_array().unwrap().len(),
            mutants.len() + 1
        );
        assert_eq!(json["total_mutants"], mutants.len());
        assert_eq!(json["caught"], mutants.len());
        assert_eq!(json["end_time"], serde_json::Value::Null);
    }

    /// A reader polling `outcomes.json` while it is rewritten always sees a complete file,
    /// and no temporary files are left behind.
    #[test]
    fn write_lab_outcome_never_exposes_partial_outcomes_json() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let temp_dir = TempDir::new().unwrap();
        let mut output_dir = OutputDir::new(temp_dir.path().try_into().unwrap()).unwrap();
        // Make the file big enough that writing it takes a while.
        for mutant in some_mutants().into_iter().cycle().take(500) {
            let outcome = realistic_mutant_outcome(&mut output_dir, mutant);
            output_dir.lab_outcome.add(outcome);
        }
        output_dir.write_lab_outcome().unwrap();
        let json_path = output_dir.path().join("outcomes.json");
        let done = AtomicBool::new(false);
        let n_reads = thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut n_reads = 0;
                while !done.load(Ordering::Relaxed) {
                    let json = read_to_string(&json_path).expect("outcomes.json always exists");
                    serde_json::from_str::<serde_json::Value>(&json)
                        .expect("outcomes.json is always complete");
                    n_reads += 1;
                }
                n_reads
            });
            for _ in 0..20 {
                output_dir.write_lab_outcome().unwrap();
            }
            done.store(true, Ordering::Relaxed);
            reader.join().unwrap()
        });
        assert!(n_reads > 0);

        let mut names = std::fs::read_dir(output_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("outcomes"))
            .collect_vec();
        names.sort();
        assert_eq!(names, ["outcomes.json"]);
    }

    /// `outcomes.json` is created with the same mode as other output files, so an atomic
    /// replace doesn't make it private to the user (or world-writable).
    #[cfg(unix)]
    #[test]
    fn outcomes_json_permissions_match_other_output_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let mut output_dir = OutputDir::new(temp_dir.path().try_into().unwrap()).unwrap();
        let path = output_dir.path().to_owned();
        let baseline = baseline_outcome(&mut output_dir);
        output_dir.add_scenario_outcome(&baseline).unwrap();

        let mode = |name: &str| path.join(name).metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("outcomes.json"), mode("missed.txt"));
    }

    /// Mutants finish in whatever order concurrent workers test them, but the lists of
    /// mutants by outcome end up in the order the mutants were discovered, both when the
    /// run finishes and when it's interrupted.
    #[test]
    fn set_discovery_order_sorts_caught_txt_whatever_order_mutants_finish() {
        let mutants = some_mutants();
        assert!(mutants.len() > 2);
        let expected = mutants.iter().map(|m| m.name(true) + "\n").join("");
        for finish in [true, false] {
            let temp_dir = TempDir::new().unwrap();
            let mut output_dir = OutputDir::new(temp_dir.path().try_into().unwrap()).unwrap();
            let path = output_dir.path().to_owned();
            output_dir.set_discovery_order(&mutants);
            for mutant in mutants.iter().rev() {
                let outcome = realistic_mutant_outcome(&mut output_dir, mutant.clone());
                output_dir.add_scenario_outcome(&outcome).unwrap();
            }
            if finish {
                output_dir.finish().unwrap();
            } else {
                drop(output_dir);
            }
            assert_eq!(
                read_to_string(path.join(CAUGHT_TXT)).unwrap(),
                expected,
                "finish={finish}"
            );
        }
    }

    /// Build a realistic outcome for a mutant: a successful build followed by a failing test.
    fn realistic_mutant_outcome(output_dir: &mut OutputDir, mutant: Mutant) -> ScenarioOutcome {
        use crate::outcome::{Phase, PhaseResult};
        use crate::process::Exit;
        let scenario = Scenario::Mutant(mutant);
        let scenario_output = output_dir.start_scenario(&scenario).unwrap();
        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario);
        let argv = |phase: &str| {
            [
                "/home/user/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo",
                phase,
                "--verbose",
                "--package=cargo-mutants-testdata-internal@0.0.0",
                "--no-fail-fast",
            ]
            .map(str::to_owned)
            .to_vec()
        };
        outcome.add_phase_result(PhaseResult {
            phase: Phase::Build,
            duration: Duration::from_millis(1234),
            process_status: Exit::Success,
            argv: argv("build"),
        });
        outcome.add_phase_result(PhaseResult {
            phase: Phase::Test,
            duration: Duration::from_millis(5678),
            process_status: Exit::Failure(101),
            argv: argv("test"),
        });
        outcome
    }

    /// Measure the cumulative cost of `add_scenario_outcome` over a large run.
    ///
    /// Run with `cargo test --release -- --ignored --nocapture add_scenario_outcome_cost`.
    #[test]
    #[ignore = "measurement, not a correctness check"]
    fn add_scenario_outcome_cost_for_2000_mutants() {
        use std::time::Instant;
        const N: usize = 2000;
        let mutants = [
            include_str!("lab.rs"),
            include_str!("output.rs"),
            include_str!("outcome.rs"),
        ]
        .iter()
        .flat_map(|code| crate::visit::mutate_source_str(code, &crate::Options::default()).unwrap())
        .collect_vec();
        assert!(!mutants.is_empty());
        let temp_dir = TempDir::new().unwrap();
        let mut output_dir = OutputDir::new(temp_dir.path().try_into().unwrap()).unwrap();
        let outcomes = mutants
            .iter()
            .cycle()
            .take(N)
            .map(|mutant| realistic_mutant_outcome(&mut output_dir, mutant.clone()))
            .collect_vec();
        let mut total = Duration::ZERO;
        let mut max = Duration::ZERO;
        for outcome in &outcomes {
            let start = Instant::now();
            output_dir.add_scenario_outcome(outcome).unwrap();
            let elapsed = start.elapsed();
            total += elapsed;
            max = max.max(elapsed);
        }
        let json_path = output_dir.path().join("outcomes.json");
        let start = Instant::now();
        output_dir.finish().unwrap();
        let finish = start.elapsed();
        let final_len = json_path.metadata().unwrap().len();
        println!(
            "{N} outcomes added as fast as possible: add_scenario_outcome total {total:?}, \
             max per call {max:?}; finish {finish:?}; \
             final outcomes.json {final_len} bytes ({} bytes/outcome)",
            final_len / N as u64
        );
    }
}
