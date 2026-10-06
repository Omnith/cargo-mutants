// Copyright 2023 - 2026 Martin Pool

//! Copy a source tree, with some exclusions, to a new temporary directory.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
#[cfg(not(windows))]
use filetime::{FileTime, set_file_mtime};
use ignore::WalkBuilder;
use tempfile::TempDir;
use tracing::{debug, warn};

use crate::options::Options;
use crate::{Console, Result, check_interrupted};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::copy_symlink;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::copy_symlink;

static VCS_DIRS: &[&str] = &[".git", ".hg", ".bzr", ".svn", "_darcs", ".jj", ".pijul"];

/// Copy a file's contents, attempting to use reflink if supported.
///
/// The mtime and permissions of the copy depend on the platform and the copy method,
/// so callers should use [`copy_file_with_mtime_now`] or [`copy_file_preserving_metadata`].
///
/// Returns the number of bytes copied.
#[cfg(not(target_env = "musl"))] // https://github.com/sourcefrog/cargo-mutants/issues/581, musl copy file syscall is non-standard.
fn copy_file(src: &Path, dest: &Path, reflink_supported: &AtomicBool) -> Result<u64> {
    // Try reflink first if we haven't determined it's not supported
    if reflink_supported.load(Ordering::Relaxed) {
        match reflink::reflink(src, dest) {
            Ok(()) => {
                let metadata = fs::metadata(dest)
                    .with_context(|| format!("Failed to get metadata for {}", dest.display()))?;
                return Ok(metadata.len());
            }
            Err(e) => {
                // On Windows, reflink can fail without returning ErrorKind::Unsupported,
                // so we give up on reflinks after any error to avoid repeated failures.
                reflink_supported.store(false, Ordering::Relaxed);
                debug!("Reflink failed: {}, falling back to regular copy", e);
            }
        }
    }

    // Fall back to regular copy
    fs::copy(src, dest)
        .with_context(|| format!("Failed to copy {} to {}", src.display(), dest.display()))
}

#[cfg(target_env = "musl")] // https://github.com/sourcefrog/cargo-mutants/issues/581
#[mutants::skip]
fn copy_file(src: &Path, dest: &Path, _reflink_supported: &AtomicBool) -> Result<u64> {
    fs::copy(src, dest)
        .with_context(|| format!("Failed to copy {} to {}", src.display(), dest.display()))
}

/// Copy a source file, and set the mtime of the copy to now, regardless of the copy method.
///
/// This matters for two reasons:
///
/// 1. clonefile(2) on macOS and `fs::copy` on some platforms preserve the source
///    mtime, which can be days old; macOS's /usr/libexec/dirhelper periodically deletes
///    files in `/var/folders/<uid>/T/` with mtime older than `CLEAN_FILES_OLDER_THAN_DAYS`
///    (default 3), which silently unlinks copied source files mid-run.
///
/// 2. Build dirs seeded from the baseline's `target/` (see [`copy_target_dir`]) rely on
///    every copied source file being newer than every copied build product, so that cargo
///    rebuilds every workspace package in the new dir rather than reusing artifacts that
///    embed the absolute paths of another build dir.
///
/// Returns the number of bytes copied.
fn copy_file_with_mtime_now(
    src: &Path,
    dest: &Path,
    reflink_supported: &AtomicBool,
) -> Result<u64> {
    let bytes = copy_file(src, dest, reflink_supported)?;
    set_mtime(dest, SystemTime::now())?;
    Ok(bytes)
}

/// Set the mtime of a file, even if it's read-only.
#[cfg(not(windows))]
fn set_mtime(path: &Path, mtime: SystemTime) -> Result<()> {
    set_file_mtime(path, FileTime::from_system_time(mtime))
        .with_context(|| format!("set_file_mtime {}", path.display()))
}

/// Set the mtime of a file, even if it's read-only.
///
/// `filetime` opens the file for writing, which is refused for read-only files on Windows.
/// Setting file times only needs `FILE_WRITE_ATTRIBUTES`, which is allowed on them.
#[cfg(windows)]
fn set_mtime(path: &Path, mtime: SystemTime) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .open(path)
        .and_then(|file| file.set_modified(mtime))
        .with_context(|| format!("set mtime of {}", path.display()))
}

/// Copy a file, giving the copy the same mtime and permissions as the source.
///
/// Neither reflinks nor `fs::copy` preserve the mtime on Linux, and reflinks on Linux
/// don't preserve permissions, so both are set explicitly.
///
/// Returns the number of bytes copied.
fn copy_file_preserving_metadata(
    src: &Path,
    dest: &Path,
    reflink_supported: &AtomicBool,
) -> Result<u64> {
    let metadata = fs::metadata(src)
        .with_context(|| format!("Failed to get metadata for {}", src.display()))?;
    let bytes = copy_file(src, dest, reflink_supported)?;
    let mtime = metadata
        .modified()
        .with_context(|| format!("Failed to get mtime of {}", src.display()))?;
    set_mtime(dest, mtime)?;
    fs::set_permissions(dest, metadata.permissions())
        .with_context(|| format!("set_permissions {}", dest.display()))?;
    Ok(bytes)
}

/// True if `path` is cargo's incremental compilation cache for one profile: a directory
/// named `incremental` beside the profile's `.fingerprint` directory.
///
/// A copied cache isn't reused in the new build dir, whose path differs, so copying it
/// only spends disk. Measured in `docs/work/fallback-build-cost/design.md`, Measured 9.
/// Fingerprints don't refer to it, so cargo still sees the copied build as fresh.
fn is_incremental_cache(path: &Path) -> bool {
    // the name first, so that other entries cost no extra stat
    path.file_name().is_some_and(|name| name == "incremental")
        && path
            .parent()
            .is_some_and(|parent| parent.join(".fingerprint").is_dir())
}

/// Copy a cargo `target/` directory into a new build directory, so that cargo can reuse its
/// build products.
///
/// Unlike [`copy_tree`], this copies every file, and it preserves file mtimes and
/// permissions so that cargo sees the copied build products as up to date. Symlinks are
/// copied as symlinks and never followed.
///
/// `dest` must not already exist, which ensures this only writes into a new directory.
pub fn copy_target_dir(src: &Utf8Path, dest: &Utf8Path, console: &Console) -> Result<()> {
    let start = Instant::now();
    let mut total_bytes = 0;
    let mut total_files = 0;
    let reflink_supported = AtomicBool::new(true);
    fs::create_dir(dest).with_context(|| format!("Failed to create directory {dest:?}"))?;
    console.start_copy(dest);
    let walk = WalkBuilder::new(src)
        .standard_filters(false) // copy hidden and ignored files
        .follow_links(false)
        .filter_entry(|entry| {
            !(entry.file_type().is_some_and(|ft| ft.is_dir()) && is_incremental_cache(entry.path()))
        })
        .build();
    for entry in walk {
        check_interrupted()?;
        let entry = entry?;
        if entry.depth() == 0 {
            continue; // the root, already created
        }
        let relative_path = entry
            .path()
            .strip_prefix(src)
            .expect("entry path is in src");
        let dest_path = dest.as_std_path().join(relative_path);
        let ft = entry.file_type().with_context(|| {
            format!(
                "Expected file to have a file type: {}",
                entry.path().display()
            )
        })?;
        if ft.is_file() {
            total_bytes +=
                copy_file_preserving_metadata(entry.path(), &dest_path, &reflink_supported)?;
            total_files += 1;
            console.copy_progress(dest, total_bytes);
        } else if ft.is_dir() {
            fs::create_dir(&dest_path)
                .with_context(|| format!("Failed to create directory {}", dest_path.display()))?;
        } else if ft.is_symlink() {
            copy_symlink(
                ft,
                entry
                    .path()
                    .try_into()
                    .context("Convert filename to UTF-8")?,
                dest_path
                    .as_path()
                    .try_into()
                    .context("Convert filename to UTF-8")?,
            )?;
        } else {
            warn!("Unexpected file type: {:?}", entry.path());
        }
    }
    console.finish_copy(dest);
    let reflink_used = reflink_supported.load(Ordering::Relaxed);
    debug!(%src, %dest, total_bytes, total_files, reflink_used, elapsed = ?start.elapsed(), "Seeded target dir");
    Ok(())
}

/// Copy a source tree, with some exclusions, to a new temporary directory.
pub fn copy_tree(
    from_path: &Utf8Path,
    name_base: &str,
    options: &Options,
    console: &Console,
) -> Result<TempDir> {
    let start = Instant::now();
    let mut total_bytes = 0;
    let mut total_files = 0;
    let reflink_supported = AtomicBool::new(true);
    let temp_dir = tempfile::Builder::new()
        .prefix(name_base)
        .tempdir()
        .context("create temp dir")?;
    let dest = temp_dir
        .path()
        .try_into()
        .context("Convert path to UTF-8")?;
    console.start_copy(dest);
    let mut walk_builder = WalkBuilder::new(from_path);
    let copy_vcs = options.copy_vcs; // for lifetime
    let from_path_owned = from_path.to_owned(); // for lifetime in closure
    let copy_target = options.copy_target;
    walk_builder
        .git_ignore(options.gitignore)
        .git_exclude(options.gitignore)
        .git_global(options.gitignore)
        .hidden(false) // copy hidden files
        .ignore(false) // don't use .ignore
        .require_git(true) // stop at git root; only read gitignore files inside git trees
        .filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            let is_top_level_target = name == "target"
                && entry
                    .path()
                    .parent()
                    .is_some_and(|p| p == from_path_owned.as_path());
            name != "mutants.out"
                && name != "mutants.out.old"
                && (copy_target || !is_top_level_target)
                && (copy_vcs || !VCS_DIRS.contains(&name.as_ref()))
                && !(entry.file_type().is_some_and(|ft| ft.is_dir())
                    && is_incremental_cache(entry.path()))
        });
    debug!(?walk_builder);
    for entry in walk_builder.build() {
        check_interrupted()?;
        let entry = entry?;
        let relative_path = entry
            .path()
            .strip_prefix(from_path)
            .expect("entry path is in from_path");
        let dest_path: Utf8PathBuf = temp_dir
            .path()
            .join(relative_path)
            .try_into()
            .context("Convert path to UTF-8")?;
        let ft = entry.file_type().with_context(|| {
            format!(
                "Expected file to have a file type: {}",
                entry.path().display()
            )
        })?;
        if ft.is_file() {
            let bytes_copied = copy_file_with_mtime_now(
                entry.path(),
                dest_path.as_std_path(),
                &reflink_supported,
            )?;
            total_bytes += bytes_copied;
            total_files += 1;
            console.copy_progress(dest, total_bytes);
        } else if ft.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .with_context(|| format!("Failed to create directory {dest_path:?}"))?;
        } else if ft.is_symlink() {
            copy_symlink(
                ft,
                entry
                    .path()
                    .try_into()
                    .context("Convert filename to UTF-8")?,
                &dest_path,
            )?;
        } else {
            warn!("Unexpected file type: {:?}", entry.path());
        }
    }
    console.finish_copy(dest);
    let reflink_used = reflink_supported.load(Ordering::Relaxed);
    debug!(?total_bytes, ?total_files, ?reflink_used, temp_dir = ?temp_dir.path(), elapsed = ?start.elapsed(), "Copied source tree");
    Ok(temp_dir)
}

#[cfg(test)]
mod test {
    // TODO: Maybe run these with $HOME set to a temp dir so that global git config has no effect?

    #[cfg(unix)]
    use std::fs::{Permissions, read_dir, read_link};
    use std::fs::{
        create_dir, create_dir_all, read_to_string, set_permissions, symlink_metadata, write,
    };
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use camino::Utf8PathBuf;
    use filetime::{FileTime, set_file_mtime};
    use tempfile::TempDir;

    use crate::Result;
    use crate::console::Console;
    use crate::options::Options;

    use super::{
        copy_file_preserving_metadata, copy_file_with_mtime_now, copy_target_dir, copy_tree,
    };

    /// Test for regression of <https://github.com/sourcefrog/cargo-mutants/issues/450>
    #[test]
    fn copy_tree_with_parent_ignoring_star() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = tmp_dir.path();
        write(tmp.join(".gitignore"), "*\n")?;

        let a = Utf8PathBuf::try_from(tmp.join("a")).unwrap();
        create_dir(&a)?;
        write(a.join("Cargo.toml"), "[package]\nname = a")?;
        let src = a.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["--gitignore=true"]);
        let dest_tmpdir = copy_tree(&a, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(dest.join("Cargo.toml").is_file());
        assert!(dest.join("src").is_dir());
        assert!(dest.join("src/main.rs").is_file());

        Ok(())
    }

    /// With `gitignore` set to `true`, but no `.git`, don't exclude anything.
    #[test]
    fn copy_with_gitignore_but_without_git_dir() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        write(tmp.join(".gitignore"), "foo\n")?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;
        write(tmp.join("foo"), "bar")?;

        let options = Options::from_arg_strs(["--gitignore=true"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(
            dest.join("foo").is_file(),
            "foo should be copied because gitignore is not used without .git"
        );

        Ok(())
    }

    /// With `gitignore` set to `false` (the default), patterns in that file have no effect.
    #[test]
    fn copy_without_gitignore_by_default() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        write(tmp.join(".gitignore"), "foo\n")?;
        create_dir(tmp.join(".git"))?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;
        write(tmp.join("foo"), "bar")?;

        let options = Options::from_arg_strs(["mutants"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        // gitignore didn't exclude `foo` because gitignore is false by default
        assert!(dest.join("foo").is_file());

        Ok(())
    }

    /// With `gitignore` set to `true`, in a tree with `.git`, `.gitignore` is respected.
    #[test]
    fn copy_with_gitignore_and_git_dir() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        write(tmp.join(".gitignore"), "foo\n")?;
        create_dir(tmp.join(".git"))?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;
        write(tmp.join("foo"), "bar")?;

        let options = Options::from_arg_strs(["mutants", "--gitignore=true"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(
            !dest.join("foo").is_file(),
            "foo should have been excluded by gitignore"
        );

        Ok(())
    }

    #[test]
    fn copy_with_gitignore_true_in_config_and_git_dir_excludes_ignored_files() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        write(tmp.join(".gitignore"), "foo\n")?;
        create_dir(tmp.join(".git"))?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;
        write(tmp.join("foo"), "bar")?;

        let options = Options::from_arg_strs_and_config(["mutants"], "gitignore=true");
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(
            !dest.join("foo").is_file(),
            "foo should have been excluded by gitignore"
        );

        Ok(())
    }

    /// With `gitignore` set to `false`, patterns in that file have no effect.
    #[test]
    fn copy_without_gitignore() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        write(tmp.join(".gitignore"), "foo\n")?;
        create_dir(tmp.join(".git"))?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;
        write(tmp.join("foo"), "bar")?;

        let options = Options::from_arg_strs(["mutants", "--gitignore=false"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        // gitignore didn't exclude `foo`
        assert!(dest.join("foo").is_file());

        Ok(())
    }

    #[test]
    fn dont_copy_git_dir_or_mutants_out_by_default() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        create_dir(tmp.join(".git"))?;
        write(tmp.join(".git/foo"), "bar")?;
        create_dir(tmp.join("mutants.out"))?;
        write(tmp.join("mutants.out/foo"), "bar")?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["mutants"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(!dest.join(".git").is_dir(), ".git should not be copied");
        assert!(
            !dest.join(".git/foo").is_file(),
            ".git/foo should not be copied"
        );
        assert!(
            !dest.join("mutants.out").exists(),
            "mutants.out should not be copied"
        );
        assert!(!dest.join("target").exists(), "target should not be copied");
        assert!(
            dest.join("Cargo.toml").is_file(),
            "Cargo.toml should be copied"
        );

        Ok(())
    }

    #[test]
    fn copy_git_dir_when_requested() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        create_dir(tmp.join(".git"))?;
        write(tmp.join(".git/foo"), "bar")?;
        create_dir(tmp.join("mutants.out"))?;
        write(tmp.join("mutants.out/foo"), "bar")?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["mutants", "--copy-vcs=true"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(dest.join(".git").is_dir(), ".git should be copied");
        assert!(dest.join(".git/foo").is_file(), ".git/foo should be copied");
        assert!(
            !dest.join("mutants.out").exists(),
            "mutants.out should not be copied"
        );
        assert!(
            dest.join("Cargo.toml").is_file(),
            "Cargo.toml should be copied"
        );

        Ok(())
    }

    #[test]
    fn dont_copy_target_dir_by_default_when_copy_target_false() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        create_dir(tmp.join("target"))?;
        write(tmp.join("target/foo"), "bar")?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["mutants"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(
            !dest.join("target").exists(),
            "target should not be copied by default"
        );
        assert!(
            dest.join("Cargo.toml").is_file(),
            "Cargo.toml should be copied"
        );

        Ok(())
    }

    #[test]
    fn copy_target_dir_when_requested() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();
        create_dir(tmp.join("target"))?;
        write(tmp.join("target/foo"), "bar")?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["mutants", "--copy-target=true"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();
        assert!(
            dest.join("target").exists(),
            "target should be copied when --copy-target=true"
        );
        assert!(
            dest.join("target/foo").is_file(),
            "target/foo should be copied when --copy-target=true"
        );
        assert!(
            dest.join("Cargo.toml").is_file(),
            "Cargo.toml should be copied"
        );

        Ok(())
    }

    #[test]
    fn copy_non_top_level_target_files() -> Result<()> {
        let tmp_dir = TempDir::new().unwrap();
        let tmp = Utf8PathBuf::try_from(tmp_dir.path().to_owned()).unwrap();

        // Create top-level target directory (should be excluded)
        create_dir(tmp.join("target"))?;
        write(tmp.join("target/build_artifact"), "should not be copied")?;

        // Create non-top-level target file and directory (should be copied)
        let testdata = tmp.join("testdata");
        create_dir(&testdata)?;
        write(testdata.join("target"), "should be copied")?;

        let subdir = tmp.join("subdir");
        create_dir(&subdir)?;
        create_dir(subdir.join("target"))?;
        write(subdir.join("target/file"), "should be copied")?;

        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        let src = tmp.join("src");
        create_dir(&src)?;
        write(src.join("main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["mutants"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();

        // Top-level target should be excluded
        assert!(
            !dest.join("target").exists(),
            "top-level target directory should not be copied"
        );

        // Non-top-level target files/dirs should be included
        assert!(
            dest.join("testdata/target").is_file(),
            "testdata/target file should be copied"
        );
        assert!(
            dest.join("subdir/target").is_dir(),
            "subdir/target directory should be copied"
        );
        assert!(
            dest.join("subdir/target/file").is_file(),
            "subdir/target/file should be copied"
        );

        Ok(())
    }

    /// An mtime long before any test runs.
    const OLD_MTIME: FileTime = FileTime::from_unix_time(1_000_000_000, 0);

    fn mtime(path: &Path) -> FileTime {
        FileTime::from_last_modification_time(&symlink_metadata(path).unwrap())
    }

    #[test]
    fn copy_file_with_mtime_now_sets_mtime_to_now_with_and_without_reflink() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src.rs");
        write(&src, "fn main() {}")?;
        set_file_mtime(&src, OLD_MTIME)?;
        for reflink in [true, false] {
            let dest = tmp.path().join(format!("dest-{reflink}.rs"));
            let before = FileTime::now();
            copy_file_with_mtime_now(&src, &dest, &AtomicBool::new(reflink))?;
            assert!(
                mtime(&dest).unix_seconds() >= before.unix_seconds(),
                "copied source file should have mtime of now (reflink={reflink})"
            );
        }
        Ok(())
    }

    #[test]
    fn copy_file_preserving_metadata_keeps_mtime_with_and_without_reflink() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("libfoo.rlib");
        write(&src, "artifact")?;
        set_file_mtime(&src, OLD_MTIME)?;
        for reflink in [true, false] {
            let dest = tmp.path().join(format!("dest-{reflink}.rlib"));
            let bytes = copy_file_preserving_metadata(&src, &dest, &AtomicBool::new(reflink))?;
            assert_eq!(bytes, 8);
            assert_eq!(mtime(&dest), OLD_MTIME, "reflink={reflink}");
        }
        Ok(())
    }

    /// Read-only source files are common, for example in Perforce workspaces, and on
    /// Windows setting the mtime of a read-only file needs care.
    #[test]
    fn copy_file_with_mtime_now_and_copy_file_preserving_metadata_accept_read_only_source()
    -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("read_only.rs");
        write(&src, "fn main() {}")?;
        set_file_mtime(&src, OLD_MTIME)?;
        let mut permissions = symlink_metadata(&src)?.permissions();
        permissions.set_readonly(true);
        set_permissions(&src, permissions)?;
        for reflink in [true, false] {
            let now_dest = tmp.path().join(format!("now-{reflink}.rs"));
            let before = FileTime::now();
            copy_file_with_mtime_now(&src, &now_dest, &AtomicBool::new(reflink))?;
            assert!(mtime(&now_dest).unix_seconds() >= before.unix_seconds());
            let kept_dest = tmp.path().join(format!("kept-{reflink}.rs"));
            copy_file_preserving_metadata(&src, &kept_dest, &AtomicBool::new(reflink))?;
            assert_eq!(mtime(&kept_dest), OLD_MTIME, "reflink={reflink}");
            assert!(symlink_metadata(&kept_dest)?.permissions().readonly());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn copy_file_preserving_metadata_keeps_executable_permission() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("build-script-build");
        write(&src, "binary")?;
        set_permissions(&src, Permissions::from_mode(0o755))?;
        for reflink in [true, false] {
            let dest = tmp.path().join(format!("dest-{reflink}"));
            copy_file_preserving_metadata(&src, &dest, &AtomicBool::new(reflink))?;
            let mode = symlink_metadata(&dest)?.permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "reflink={reflink}");
        }
        Ok(())
    }

    #[test]
    fn copy_target_dir_copies_nested_and_hidden_files_preserving_mtime() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let src = root.join("target");
        create_dir_all(src.join("debug/.fingerprint/foo-1234"))?;
        create_dir_all(src.join("debug/deps"))?;
        let files = [
            ".rustc_info.json",
            "debug/.fingerprint/foo-1234/lib-foo",
            "debug/deps/libfoo-1234.rlib",
        ];
        for name in files {
            write(src.join(name), name)?;
            set_file_mtime(src.join(name), OLD_MTIME)?;
        }
        let dest = root.join("new/target");
        create_dir(root.join("new"))?;

        copy_target_dir(&src, &dest, &Console::new())?;

        for name in files {
            assert_eq!(read_to_string(dest.join(name))?, name);
            assert_eq!(mtime(dest.join(name).as_std_path()), OLD_MTIME, "{name}");
        }
        Ok(())
    }

    #[test]
    fn copy_target_dir_skips_incremental_caches_beside_fingerprints() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let src = root.join("target");
        let profile_dirs = ["debug", "x86_64-unknown-linux-gnu/debug"];
        for profile_dir in profile_dirs {
            let profile_dir = src.join(profile_dir);
            create_dir_all(profile_dir.join(".fingerprint/foo-1234"))?;
            write(
                profile_dir.join(".fingerprint/foo-1234/lib-foo"),
                "fingerprint",
            )?;
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
        write(
            tmp.join("target/debug/.fingerprint/foo-1234/lib-foo"),
            "fingerprint",
        )?;
        create_dir_all(tmp.join("target/debug/incremental/foo-1234"))?;
        write(tmp.join("target/debug/incremental/foo-1234/cache"), "cache")?;
        // A test's own scratch directory that happens to be called `incremental`.
        create_dir_all(tmp.join("target/tmp/x/incremental"))?;
        write(tmp.join("target/tmp/x/incremental/data"), "test data")?;
        write(tmp.join("Cargo.toml"), "[package]\nname = a")?;
        create_dir(tmp.join("src"))?;
        write(tmp.join("src/main.rs"), "fn main() {}")?;

        let options = Options::from_arg_strs(["mutants", "--copy-target=true"]);
        let dest_tmpdir = copy_tree(&tmp, "a", &options, &Console::new())?;
        let dest = dest_tmpdir.path();

        assert!(!dest.join("target/debug/incremental").exists());
        assert!(
            dest.join("target/debug/.fingerprint/foo-1234/lib-foo")
                .is_file()
        );
        assert!(dest.join("target/tmp/x/incremental/data").is_file());
        Ok(())
    }

    #[test]
    fn copy_target_dir_refuses_to_write_into_existing_dest() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        create_dir(root.join("target"))?;
        write(root.join("target/a"), "new")?;
        create_dir(root.join("existing"))?;

        assert!(
            copy_target_dir(
                &root.join("target"),
                &root.join("existing"),
                &Console::new()
            )
            .is_err()
        );
        assert!(!root.join("existing/a").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn copy_target_dir_copies_symlinks_as_symlinks_without_following_them() -> Result<()> {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        create_dir(root.join("outside"))?;
        write(root.join("outside/secret"), "outside")?;
        let src = root.join("target");
        create_dir(&src)?;
        symlink("../outside", src.join("dir_link"))?;
        symlink("../outside/secret", src.join("file_link"))?;
        create_dir(root.join("new"))?;
        let dest = root.join("new/target");

        copy_target_dir(&src, &dest, &Console::new())?;

        for (name, link_target) in [
            ("dir_link", "../outside"),
            ("file_link", "../outside/secret"),
        ] {
            let link = dest.join(name);
            assert!(
                symlink_metadata(&link)?.file_type().is_symlink(),
                "{name} is a symlink"
            );
            assert_eq!(read_link(&link)?, Path::new(link_target));
        }
        assert_eq!(
            read_dir(root.join("outside"))?.count(),
            1,
            "nothing was written through the symlink"
        );
        Ok(())
    }
}
