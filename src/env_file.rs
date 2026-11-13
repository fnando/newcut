//! The env file: `NAME=value` lines (`$NEWCUT_CONFIG_DIR/env`, or
//! `--env-file`) loaded into the environment before anything reads it, so
//! the config's `"$NAME"` secrets and newcut's own variables can live there
//! instead of in your shell. Variables already set win over the file.
//!
//! It's read once, at startup, before any other thread exists, and never
//! reloaded: changing the environment while other threads may read it is
//! undefined behavior (why `std::env::set_var` is `unsafe`), and the daemon
//! runs a thread per service. Reloading would mean keeping the file's
//! values apart from the environment, in a map `config::secret::resolve`
//! also reads.

use std::path::{Path, PathBuf};

/// The `--env-file` given in `args` (the last one, as clap would take it).
#[must_use]
pub fn arg(args: impl IntoIterator<Item = String>) -> Option<PathBuf> {
    let mut args = args.into_iter();
    let mut found = None;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--env-file=") {
            found = Some(value.into());
        } else if arg == "--env-file" {
            found = args.next().map(Into::into);
        }
    }
    found
}

/// The env file used without `--env-file`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    crate::paths::Dirs::from_env()
        .ok()
        .map(|dirs| dirs.config_dir.join("env"))
}

/// Load the env file: `given` (it must be there), or else the default one
/// when it's there. Returns the file loaded. Call it before any other
/// thread starts: it sets environment variables.
///
/// # Errors
///
/// When `given` can't be read, or a file has a line that isn't
/// `NAME=value`.
pub fn load(given: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let path = match given {
        Some(path) => path.to_path_buf(),
        None => match default_path() {
            Some(path) if path.is_file() => path,
            _ => return Ok(None),
        },
    };
    dotenvy::from_path(&path).map_err(|e| format!("the env file {}: {e}", path.display()))?;
    Ok(Some(path))
}

/// A warning when other users can read the env file (it holds secrets).
#[must_use]
pub fn readable(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "{0}: other users can read it, and it may hold secrets; run `chmod 600 {0}`",
            path.display()
        )
    })
}

/// What `newcut init` writes (when there's no env file yet).
pub const STARTER: &str = "\
# newcut reads this before its config: `NAME=value` lines, for the config's
# \"$NAME\" settings (server.token, Pushover's keys). Variables already set in
# the environment win. Keep it private: chmod 600.
#
# NEWCUT_TOKEN=a-long-random-string
# PUSHOVER_APP_TOKEN=
# PUSHOVER_USER_KEY=
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_flag_wherever_it_is() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(arg(args(&["run"])), None);
        assert_eq!(
            arg(args(&["--env-file", "/a", "run", "--env-file=/b"])),
            Some(PathBuf::from("/b"))
        );
        assert_eq!(
            arg(args(&["run", "--env-file", "/a"])),
            Some(PathBuf::from("/a"))
        );
        assert_eq!(arg(args(&["run", "--", "--env-file", "/a"])), None);
    }

    #[test]
    fn a_given_file_must_be_there() {
        let err = load(Some(Path::new("/nowhere/env"))).unwrap_err();
        assert!(err.contains("/nowhere/env"), "{err}");
    }
}
