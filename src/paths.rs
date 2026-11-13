//! XDG directories and path expansion.
//!
//! Config lives in `$NEWCUT_CONFIG_DIR`, or `$XDG_CONFIG_HOME/newcut`
//! (`~/.config/newcut`); state in `$XDG_DATA_HOME/newcut`
//! (`~/.local/share/newcut`), on macOS too.

use std::path::{Path, PathBuf};

use etcetera::BaseStrategy;
use etcetera::base_strategy::Xdg;

/// Overrides the config directory.
pub const CONFIG_DIR_VAR: &str = "NEWCUT_CONFIG_DIR";

/// newcut's base directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

/// Where newcut keeps its state, under the data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub dir: PathBuf,
    pub state_db: PathBuf,
    pub scheduler_lock: PathBuf,
    pub cache: PathBuf,
}

impl Dirs {
    /// Resolve from the environment (`$HOME`, `$NEWCUT_CONFIG_DIR`,
    /// `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`).
    ///
    /// # Errors
    ///
    /// Fails if the home directory can't be found, or `$NEWCUT_CONFIG_DIR`
    /// names an unset variable.
    pub fn from_env() -> Result<Self, String> {
        let xdg = Xdg::new().map_err(|e| format!("cannot find the home directory: {e}"))?;
        let home = xdg.home_dir().to_path_buf();
        let config_dir = match std::env::var(CONFIG_DIR_VAR) {
            Ok(dir) if !dir.is_empty() => {
                expand(&dir, &home).map_err(|e| format!("{CONFIG_DIR_VAR}: {e}"))?
            }
            _ => xdg.config_dir().join("newcut"),
        };
        Ok(Self {
            home,
            config_dir,
            data_dir: xdg.data_dir().join("newcut"),
        })
    }

    /// The conventional config file location.
    #[must_use]
    pub fn default_config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    #[must_use]
    pub fn state(&self) -> StatePaths {
        let dir = self.data_dir.clone();
        StatePaths {
            state_db: dir.join("state.db"),
            scheduler_lock: dir.join("scheduler.lock"),
            cache: dir.join("cache"),
            dir,
        }
    }
}

/// Expand a leading `~` and `$VAR` / `${VAR}` references.
///
/// # Errors
///
/// Fails if a `${` isn't closed or a referenced variable is not set.
pub fn expand(input: &str, home: &Path) -> Result<PathBuf, String> {
    expand_with(input, home, |name| std::env::var(name).ok())
}

/// [`expand`] with an injectable variable lookup.
///
/// # Errors
///
/// Fails if a `${` isn't closed or `var` returns `None` for a referenced name.
pub fn expand_with(
    input: &str,
    home: &Path,
    var: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    let mut out = String::new();
    let rest = if input == "~" {
        out.push_str(&home.to_string_lossy());
        ""
    } else if let Some(rest) = input.strip_prefix("~/") {
        out.push_str(&home.to_string_lossy());
        out.push('/');
        rest
    } else {
        input
    };

    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.next() != Some('}') {
            return Err(format!("{input:?}: unclosed ${{"));
        }
        if name.is_empty() {
            out.push('$');
            continue;
        }
        let value = var(&name)
            .ok_or_else(|| format!("{input:?}: environment variable ${name} is not set"))?;
        out.push_str(&value);
    }
    Ok(PathBuf::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(s: &str) -> Result<PathBuf, String> {
        expand_with(s, Path::new("/home/me"), |name| {
            (name == "MUSIC").then(|| "/mnt/m".into())
        })
    }

    #[test]
    fn expands_home_and_variables() {
        assert_eq!(exp("~"), Ok("/home/me".into()));
        assert_eq!(exp("~/Music"), Ok("/home/me/Music".into()));
        assert_eq!(exp("$MUSIC/flac"), Ok("/mnt/m/flac".into()));
        assert_eq!(exp("${MUSIC}flac"), Ok("/mnt/mflac".into()));
        assert_eq!(exp("a~b$"), Ok("a~b$".into()));
        assert!(exp("$MISSING/x").is_err());
        assert!(exp("${MUSIC").is_err());
    }

    #[test]
    fn state_paths() {
        let dirs = Dirs {
            home: "/h".into(),
            config_dir: "/c/newcut".into(),
            data_dir: "/d/newcut".into(),
        };
        let state = dirs.state();
        assert_eq!(state.state_db, PathBuf::from("/d/newcut/state.db"));
        assert_eq!(
            state.scheduler_lock,
            PathBuf::from("/d/newcut/scheduler.lock")
        );
        assert_eq!(
            dirs.default_config_file(),
            PathBuf::from("/c/newcut/config.toml")
        );
    }
}
