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
        for dir in split_path_env(path_env) {
            let candidate = dir.join(binary_name);
            if is_runnable(&candidate) {
                return ExecutableProbe {
                    installed: true,
                    path: Some(candidate),
                };
            }
        }
    }
    ExecutableProbe {
        installed: false,
        path: None,
    }
}

/// Convenience wrapper: probe `binary_name` over `standard_paths` and the
/// process's current `PATH`.
pub fn probe_binary_in_env(binary_name: &str, standard_paths: &[&str]) -> ExecutableProbe {
    let paths: Vec<PathBuf> = standard_paths.iter().map(PathBuf::from).collect();
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
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
}
