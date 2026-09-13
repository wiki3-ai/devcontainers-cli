//! Locating a container runtime's CLI executable.
//!
//! This exists because a GUI-launched app cannot rely on `PATH`. A
//! Finder/Launchpad-spawned process inherits launchd's minimal
//! environment (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains none of
//! the directories the container CLIs install into — `/usr/local/bin`,
//! `/opt/homebrew/bin`, or an app bundle's `Contents/Resources/bin`.
//! `Command::new("docker")` then fails with `No such file or directory`
//! even though the same command works perfectly in a developer shell.
//!
//! So: probe a list of known install locations first, then fall back to
//! walking `PATH` for any remaining case (Linux, a custom prefix, a
//! version-manager shim).
//!
//! On Windows the executable carries an `.exe` extension, so the `PATH`
//! walk tries both spellings — see [`executable_names`].

use std::path::{Path, PathBuf};

/// Outcome of looking for a runtime's CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableProbe {
    /// Whether a runnable binary was found.
    pub installed: bool,
    /// Absolute path of the resolved binary, when one was found.
    pub path: Option<PathBuf>,
}

impl ExecutableProbe {
    /// The resolved path as a `&str`, for handing to a CLI wrapper.
    pub fn path_str(&self) -> Option<&str> {
        self.path.as_deref().and_then(Path::to_str)
    }
}

/// Look for `binary_name` in `standard_paths`, then in the directories of
/// `path_env`.
///
/// `standard_paths` are checked in order, so callers should list the most
/// preferred location first. `path_env` is passed in rather than read
/// from the environment so this stays a pure function and testable.
pub fn probe_binary(
    binary_name: &str,
    standard_paths: &[&Path],
    path_env: Option<&str>,
) -> ExecutableProbe {
    for candidate in standard_paths {
        if is_runnable(candidate) {
            return ExecutableProbe {
                installed: true,
                path: Some(candidate.to_path_buf()),
            };
        }
    }
    if let Some(path_env) = path_env {
        if let Some(found) = find_in_path_dirs(
            binary_name,
            &split_path_env(path_env),
            std::env::consts::EXE_SUFFIX,
        ) {
            return ExecutableProbe {
                installed: true,
                path: Some(found),
            };
        }
    }
    ExecutableProbe {
        installed: false,
        path: None,
    }
}

/// Convenience wrapper: probe `binary_name` over `standard_paths` and the
/// process's current `PATH`.
pub fn probe_binary_in_env(binary_name: &str, standard_paths: &[PathBuf]) -> ExecutableProbe {
    let refs: Vec<&Path> = standard_paths.iter().map(PathBuf::as_path).collect();
    let path_env = std::env::var("PATH").ok();
    probe_binary(binary_name, &refs, path_env.as_deref())
}

/// Whether `path` exists, is a file, and carries an execute bit.
///
/// Deliberately avoids `std::fs::canonicalize` on the candidate itself:
/// a broken symlink should not be treated as runnable, but we also don't
/// want to resolve away from the probed location.
fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Look for `binary_name` across `dirs`, trying the executable suffix too.
///
/// Split out from [`probe_binary`] with the suffix as a parameter so the
/// Windows semantics can be tested anywhere, not only on Windows.
fn find_in_path_dirs(binary_name: &str, dirs: &[PathBuf], suffix: &str) -> Option<PathBuf> {
    for dir in dirs {
        for name in executable_names(binary_name, suffix) {
            let candidate = dir.join(&name);
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// File names to try for `binary_name`, in order.
///
/// On Windows the CLI is `docker.exe`, so `dir.join("docker")` would never
/// match and the runtime would look uninstalled. The bare name stays first so
/// an extensionless shim still wins on platforms that have one, and a name
/// that already carries the suffix is not doubled up.
fn executable_names(binary_name: &str, suffix: &str) -> Vec<String> {
    let mut names = vec![binary_name.to_string()];
    if !suffix.is_empty() && !binary_name.ends_with(suffix) {
        names.push(format!("{binary_name}{suffix}"));
    }
    names
}

/// Split a `PATH` value into non-empty entries.
fn split_path_env(path_env: &str) -> Vec<PathBuf> {
    std::env::split_paths(path_env).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_a_runnable_binary_in_the_standard_paths() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakectl");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let probe = probe_binary("fakectl", &[bin.as_path()], None);
        assert!(probe.installed);
        assert_eq!(probe.path.as_deref(), Some(bin.as_path()));
    }

    #[test]
    fn falls_back_to_the_path_env() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakectl");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_env = dir.path().to_string_lossy().to_string();
        let probe = probe_binary("fakectl", &[], Some(&path_env));
        assert!(probe.installed, "should be found via PATH");
        assert_eq!(probe.path.as_deref(), Some(bin.as_path()));
    }

    #[test]
    fn standard_paths_win_over_path_env() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [a.path(), b.path()] {
            let bin = dir.join("fakectl");
            fs::write(&bin, b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        let path_env = b.path().to_string_lossy().to_string();
        let probe = probe_binary(
            "fakectl",
            &[a.path().join("fakectl").as_path()],
            Some(&path_env),
        );
        assert_eq!(
            probe.path.as_deref(),
            Some(a.path().join("fakectl").as_path()),
            "the first standard path should win"
        );
    }

    #[test]
    fn a_non_executable_file_is_not_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakectl");
        fs::write(&bin, b"not a program").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();

            let probe = probe_binary("fakectl", &[bin.as_path()], None);
            assert!(!probe.installed, "mode 0644 is not executable");
        }
    }

    #[test]
    fn a_directory_is_not_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("fakectl");
        fs::create_dir(&sub).unwrap();
        let probe = probe_binary("fakectl", &[sub.as_path()], None);
        assert!(!probe.installed);
    }

    #[test]
    fn reports_not_installed_when_nothing_matches() {
        let probe = probe_binary(
            "definitely-not-a-real-binary",
            &[],
            Some("/nonexistent-dir"),
        );
        assert!(!probe.installed);
        assert!(probe.path.is_none());
        assert!(probe.path_str().is_none());
    }

    /// Write a fake executable named `name` into `dir` and mark it runnable.
    fn fake_binary(dir: &Path, name: &str) -> PathBuf {
        let bin = dir.join(name);
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    #[test]
    fn finds_an_exe_suffixed_binary_when_there_is_no_bare_one() {
        // Windows ships `docker.exe`, so joining the bare name would miss it
        // and the runtime would report itself uninstalled. Exercised here with
        // an explicit suffix so it runs on any platform.
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path(), "docker.exe");
        let dirs = vec![dir.path().to_path_buf()];

        assert_eq!(find_in_path_dirs("docker", &dirs, ".exe"), Some(bin));
        // The pre-fix behaviour: with no suffix there is nothing to find.
        assert_eq!(find_in_path_dirs("docker", &dirs, ""), None);
    }

    #[test]
    fn prefers_the_bare_name_over_the_suffixed_one() {
        let dir = tempfile::tempdir().unwrap();
        let bare = fake_binary(dir.path(), "docker");
        let _suffixed = fake_binary(dir.path(), "docker.exe");
        let dirs = vec![dir.path().to_path_buf()];
        assert_eq!(find_in_path_dirs("docker", &dirs, ".exe"), Some(bare));
    }

    #[test]
    fn does_not_double_a_suffix_the_name_already_has() {
        assert_eq!(executable_names("docker.exe", ".exe"), vec!["docker.exe"]);
        assert_eq!(
            executable_names("docker", ".exe"),
            vec!["docker", "docker.exe"]
        );
        // Off Windows the suffix is empty, so only the bare name is tried.
        assert_eq!(executable_names("docker", ""), vec!["docker"]);
    }

    #[test]
    fn the_path_walk_searches_every_directory() {
        // A CLI installed into a later `PATH` entry must still be found — the
        // first directory simply not having it is not a failure.
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let bin = fake_binary(second.path(), "docker.exe");
        let dirs = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        assert_eq!(find_in_path_dirs("docker", &dirs, ".exe"), Some(bin));
    }
}
