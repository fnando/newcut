//! Reading and checking a config: TOML syntax and shape first (unknown keys
//! are errors), then what the values mean. Problems name `file:line:col`
//! where the TOML parser can tell.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::{Config, PRIMARY_TYPES, SECONDARY_TYPES, VERSION};

/// A checked config.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub config: Config,
    /// `libraries`, expanded.
    pub libraries: Vec<PathBuf>,
    /// `notification.command.bin`, expanded.
    pub notify_bin: Option<PathBuf>,
    /// Where slskd's downloads are and where tagged ones go, when both are
    /// set (and slskd's on).
    pub import: Option<ImportPaths>,
    /// `jellyfin.paths`, expanded: a folder of newcut's, and the same
    /// folder in Jellyfin.
    pub jellyfin_paths: Vec<(PathBuf, String)>,
    /// Fine to run with, but worth a look (a library that isn't there).
    pub warnings: Vec<String>,
}

/// `slskd.downloads_dir` and `slskd.output_dir`, expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPaths {
    pub downloads: PathBuf,
    /// A path with `%{…}` placeholders.
    pub output: String,
    /// `slskd.after_filing`, its program expanded.
    pub after_filing: Option<AfterFiling>,
    /// Where the files upgrades replace go (`upgrade.replaced_dir`,
    /// expanded).
    pub replaced: PathBuf,
}

/// A program run on each album folder filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterFiling {
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub timeout: std::time::Duration,
}

impl Loaded {
    /// The listen address (checked when loaded).
    #[must_use]
    pub fn listen(&self) -> SocketAddr {
        self.config
            .server
            .listen
            .parse()
            .expect("checked when loaded")
    }
}

/// Check config `text`; `origin` names it in problems (a path, or
/// "revision 3").
///
/// # Errors
///
/// Every problem found, as `origin:line:col: message` (or `origin: message`).
pub fn check_text(text: &str, origin: &str, home: &Path) -> Result<Loaded, Vec<String>> {
    let config: Config = toml::from_str(text).map_err(|e| {
        let at = e.span().map_or_default(|span| {
            let (line, col) = line_col(text, span.start);
            format!(":{line}:{col}")
        });
        vec![format!("{origin}{at}: {}", e.message().trim())]
    })?;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut error = |message: String| errors.push(format!("{origin}: {message}"));

    if config.version != VERSION {
        error(format!(
            "version {} isn't one this newcut reads (it reads version {VERSION})",
            config.version
        ));
    }
    match config.server.listen.parse::<SocketAddr>() {
        Err(_) => error(format!(
            "server.listen: {:?} isn't an address and port (e.g. \"127.0.0.1:55460\")",
            config.server.listen
        )),
        Ok(addr)
            if config.server.enabled
                && !addr.ip().is_loopback()
                && config
                    .server
                    .token
                    .as_deref()
                    .and_then(super::secret::resolve)
                    .is_none() =>
        {
            error(format!(
                "server.listen: {} is reachable from other machines, so server.token is \
                 required (or listen on 127.0.0.1)",
                config.server.listen
            ));
        }
        Ok(_) => {}
    }
    if let Some(token) = config.server.token.as_deref() {
        check_token(token, config.server.enabled, origin, &mut warnings);
    }
    for (key, url) in [
        ("musicbrainz.server", &config.musicbrainz.server),
        ("listenbrainz.server", &config.listenbrainz.server),
    ] {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            error(format!("{key}: {url:?} isn't an http(s) URL"));
        }
    }
    if config.discography.primary_types.is_empty() {
        error("discography.primary_types: name at least one (e.g. [\"Album\"])".into());
    }
    for kind in &config.discography.primary_types {
        if !PRIMARY_TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind)) {
            error(format!(
                "discography.primary_types: {kind:?} isn't one of {}",
                PRIMARY_TYPES.join(", ")
            ));
        }
    }
    for kind in &config.discography.secondary_types {
        if !SECONDARY_TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind)) {
            error(format!(
                "discography.secondary_types: {kind:?} isn't one of {}",
                SECONDARY_TYPES.join(", ")
            ));
        }
    }
    check_covers(&config.covers, &mut error);
    let replaced = check_replaced_dir(&config.upgrade, home, &mut error);
    let import = check_slskd(
        &config.slskd,
        replaced,
        home,
        origin,
        &mut error,
        &mut warnings,
    );
    check_upgrade(&config.upgrade, import.is_some(), &mut error);
    let jellyfin_paths = check_jellyfin(&config.jellyfin, home, origin, &mut error, &mut warnings);
    check_schedule(&config, &mut error);

    let libraries = check_libraries(&config.libraries, home, origin, &mut error, &mut warnings);
    let notify_bin = check_notification(&config, home, origin, &mut error, &mut warnings);
    if errors.is_empty() {
        Ok(Loaded {
            config,
            libraries,
            notify_bin,
            import,
            jellyfin_paths,
            warnings,
        })
    } else {
        Err(errors)
    }
}

/// `[jellyfin]`: its address, an API key, and full paths to map.
fn check_jellyfin(
    jellyfin: &super::JellyfinConfig,
    home: &Path,
    origin: &str,
    error: &mut impl FnMut(String),
    warnings: &mut Vec<String>,
) -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    for (local, theirs) in &jellyfin.paths {
        match crate::paths::expand(local, home) {
            Ok(path) if path.is_absolute() => {}
            Ok(_) => {
                error(format!(
                    "jellyfin.paths: {local:?} must be a full path: start with /, ~ or $VAR"
                ));
                continue;
            }
            Err(e) => {
                error(format!("jellyfin.paths: {e}"));
                continue;
            }
        }
        if theirs.trim().is_empty() {
            error(format!(
                "jellyfin.paths: {local:?} needs Jellyfin's path for it"
            ));
            continue;
        }
        if let Ok(path) = crate::paths::expand(local, home) {
            paths.push((path, theirs.trim_end_matches('/').to_string()));
        }
    }
    if !jellyfin.enabled {
        return paths;
    }
    if !(jellyfin.server.starts_with("http://") || jellyfin.server.starts_with("https://")) {
        error(format!(
            "jellyfin.server: {:?} isn't an http(s) URL",
            jellyfin.server
        ));
    }
    if jellyfin.api_key.trim().is_empty() {
        error("jellyfin.api_key: required to use Jellyfin".into());
    } else if let Some(name) = super::secret::env_name(&jellyfin.api_key)
        && super::secret::resolve(&jellyfin.api_key).is_none()
    {
        warnings.push(format!(
            "{origin}: jellyfin.api_key: ${name} isn't set, so Jellyfin can't be told about new albums"
        ));
    }
    paths
}

/// `[covers]`: plain file names.
fn check_covers(covers: &super::CoversConfig, error: &mut impl FnMut(String)) {
    if covers.local && covers.files.is_empty() {
        error("covers.files: name at least one (e.g. [\"cover.jpg\"])".into());
    }
    for (key, names) in [
        ("covers.files", &covers.files),
        ("covers.artist_files", &covers.artist_files),
    ] {
        for name in names {
            if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
                error(format!("{key}: {name:?} isn't a file name"));
            }
        }
    }
}

/// `[schedule]` (and `upgrade.retry`): a minute at least.
fn check_schedule(config: &Config, error: &mut impl FnMut(String)) {
    let schedule = &config.schedule;
    for (key, every) in [
        ("schedule.scan", schedule.scan),
        ("schedule.artist", schedule.artist),
        ("schedule.fresh", schedule.fresh),
        ("schedule.notification", schedule.notification),
        ("schedule.upgrade", schedule.upgrade),
        ("upgrade.retry", config.upgrade.retry),
    ] {
        if every.0.as_secs() < 60 {
            error(format!("{key}: use at least \"1m\""));
        }
    }
}

/// `[upgrade]`: on only with somewhere to file downloads.
fn check_upgrade(upgrade: &super::UpgradeConfig, filing: bool, error: &mut impl FnMut(String)) {
    if upgrade.enabled && !filing {
        error("upgrade.enabled: upgrades need slskd on, with downloads_dir and output_dir".into());
    }
    if upgrade.per_run == 0 {
        error("upgrade.per_run: use at least 1".into());
    }
}

/// `upgrade.replaced_dir`, expanded; newcut's data folder's `replaced`
/// (`$XDG_DATA_HOME/newcut/replaced`, `~/.local/share/newcut/replaced`)
/// when it isn't set.
fn check_replaced_dir(
    upgrade: &super::UpgradeConfig,
    home: &Path,
    error: &mut impl FnMut(String),
) -> PathBuf {
    let default = || {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|d| !d.is_empty())
            .map_or_else(|| home.join(".local/share"), PathBuf::from)
            .join("newcut/replaced")
    };
    let Some(dir) = upgrade.replaced_dir.as_deref() else {
        return default();
    };
    match crate::paths::expand(dir, home) {
        Ok(path) if path.is_absolute() => path,
        Ok(_) => {
            error(format!(
                "upgrade.replaced_dir: {dir:?} must be a full path: start with /, ~ or $VAR"
            ));
            default()
        }
        Err(e) => {
            error(format!("upgrade.replaced_dir: {e}"));
            default()
        }
    }
}

/// `[slskd]`, when it's on: its address, and an API key.
fn check_slskd(
    slskd: &super::SlskdConfig,
    replaced: PathBuf,
    home: &Path,
    origin: &str,
    error: &mut impl FnMut(String),
    warnings: &mut Vec<String>,
) -> Option<ImportPaths> {
    let mut expand = |key: &str, value: &str| match crate::paths::expand(value, home) {
        Ok(path) if path.is_absolute() => Some(path),
        Ok(_) => {
            error(format!(
                "slskd.{key}: {value:?} must be a full path: start with /, ~ or $VAR"
            ));
            None
        }
        Err(e) => {
            error(format!("slskd.{key}: {e}"));
            None
        }
    };
    let downloads = slskd
        .downloads_dir
        .as_deref()
        .and_then(|d| expand("downloads_dir", d));
    let output = slskd
        .output_dir
        .as_deref()
        .and_then(|o| expand("output_dir", o));
    if let Some(output) = &output
        && let Err(e) = crate::import::check_template(&output.to_string_lossy())
    {
        error(format!("slskd.output_dir: {e}"));
    }
    if !slskd.enabled {
        return None;
    }
    if !(slskd.server.starts_with("http://") || slskd.server.starts_with("https://")) {
        error(format!(
            "slskd.server: {:?} isn't an http(s) URL",
            slskd.server
        ));
    }
    if slskd.api_key.trim().is_empty() {
        error("slskd.api_key: required to use slskd".into());
    } else if let Some(name) = super::secret::env_name(&slskd.api_key)
        && super::secret::resolve(&slskd.api_key).is_none()
    {
        warnings.push(format!(
            "{origin}: slskd.api_key: ${name} isn't set, so slskd can't be searched"
        ));
    }
    match (downloads, output) {
        (Some(downloads), Some(output)) => {
            if !downloads.is_dir() {
                warnings.push(format!(
                    "{origin}: slskd.downloads_dir: {} isn't there (yet?)",
                    downloads.display()
                ));
            }
            let after_filing = slskd.after_filing.as_ref().and_then(|hook| {
                let known = crate::import::arg_placeholders();
                for arg in &hook.args {
                    if let Err(e) = crate::import::check_placeholders(arg, &known) {
                        error(format!("slskd.after_filing.args: {e}"));
                    }
                }
                let bin = check_bin(
                    "slskd.after_filing.bin",
                    &hook.bin,
                    home,
                    origin,
                    error,
                    warnings,
                )?;
                Some(AfterFiling {
                    bin,
                    args: hook.args.clone(),
                    timeout: hook.timeout.0,
                })
            });
            Some(ImportPaths {
                downloads,
                output: output.to_string_lossy().into_owned(),
                after_filing,
                replaced,
            })
        }
        (None, None) if slskd.after_filing.is_none() => None,
        (None, None) => {
            warnings.push(format!(
                "{origin}: slskd.after_filing: newcut files nothing without downloads_dir and output_dir"
            ));
            None
        }
        _ => {
            warnings.push(format!(
                "{origin}: slskd: set both downloads_dir and output_dir to tag and file downloads"
            ));
            None
        }
    }
}

/// `[notification]`: Pushover's keys, and the command's program (expanded).
fn check_notification(
    config: &Config,
    home: &Path,
    origin: &str,
    error: &mut impl FnMut(String),
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    if let Some(pushover) = config.notification.pushover.as_ref().filter(|p| p.enabled) {
        for (key, value) in [
            ("app_api_token", &pushover.app_api_token),
            ("user_key", &pushover.user_key),
        ] {
            if value.trim().is_empty() {
                error(format!(
                    "notification.pushover.{key}: required to use Pushover"
                ));
            } else if let Some(name) = super::secret::env_name(value)
                && super::secret::resolve(value).is_none()
            {
                warnings.push(format!(
                    "{origin}: notification.pushover.{key}: ${name} isn't set, so Pushover can't send"
                ));
            }
        }
        if !(pushover.server.starts_with("http://") || pushover.server.starts_with("https://")) {
            error(format!(
                "notification.pushover.server: {:?} isn't an http(s) URL",
                pushover.server
            ));
        }
    }
    let command = config.notification.command.as_ref()?;
    if command.enabled {
        check_bin(
            "notification.command.bin",
            &command.bin,
            home,
            origin,
            error,
            warnings,
        )
    } else {
        expand_bin("notification.command.bin", &command.bin, home, error)
    }
}

/// A program's full path (`key` names its setting).
fn expand_bin(
    key: &str,
    bin: &str,
    home: &Path,
    error: &mut impl FnMut(String),
) -> Option<PathBuf> {
    match crate::paths::expand(bin, home) {
        Ok(path) if path.is_absolute() => Some(path),
        Ok(_) => {
            error(format!(
                "{key}: {bin:?} must be a full path: start with /, ~ or $VAR"
            ));
            None
        }
        Err(e) => {
            error(format!("{key}: {e}"));
            None
        }
    }
}

/// [`expand_bin`], with a warning when it isn't (yet) a program.
fn check_bin(
    key: &str,
    bin: &str,
    home: &Path,
    origin: &str,
    error: &mut impl FnMut(String),
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let bin = expand_bin(key, bin, home, error)?;
    match std::fs::metadata(&bin) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {}
        Ok(_) => warnings.push(format!(
            "{origin}: {key}: {} isn't an executable file",
            bin.display()
        )),
        Err(_) => warnings.push(format!(
            "{origin}: {key}: {} isn't there (yet?)",
            bin.display()
        )),
    }
    Some(bin)
}

/// The library folders, expanded, each once.
fn check_libraries(
    names: &[String],
    home: &Path,
    origin: &str,
    error: &mut impl FnMut(String),
    warnings: &mut Vec<String>,
) -> Vec<std::path::PathBuf> {
    let mut libraries = Vec::new();
    for library in names {
        match crate::paths::expand(library, home) {
            Ok(path) => {
                if !path.is_dir() {
                    warnings.push(format!(
                        "{origin}: library {} isn't a folder (yet?)",
                        path.display()
                    ));
                }
                if !libraries.contains(&path) {
                    libraries.push(path);
                }
            }
            Err(e) => error(format!("libraries: {e}")),
        }
    }
    if names.is_empty() {
        warnings.push(format!(
            "{origin}: no libraries; add your music folders to `libraries`"
        ));
    }
    libraries
}

/// `server.token`: when it names a variable, that it's set.
fn check_token(token: &str, serving: bool, origin: &str, warnings: &mut Vec<String>) {
    if serving
        && let Some(name) = super::secret::env_name(token)
        && super::secret::resolve(token).is_none()
    {
        warnings.push(format!(
            "{origin}: server.token: ${name} isn't set, so the web UI has no sign-in"
        ));
    }
}

/// Read and check the config at `path`; returns its text too.
///
/// # Errors
///
/// When the file can't be read, or isn't a valid config (every problem,
/// one per line).
pub fn load_file(path: &Path, home: &Path) -> anyhow::Result<(String, Loaded)> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "there's no config at {}; create one with `newcut init`",
                path.display()
            )
        } else {
            anyhow::anyhow!("cannot read {}: {e}", path.display())
        }
    })?;
    let mut loaded = check_text(&text, &path.display().to_string(), home)
        .map_err(|errors| anyhow::anyhow!(errors.join("\n")))?;
    if let Some(warning) = readable_secret(path, &loaded) {
        loaded.warnings.push(warning);
    }
    Ok((text, loaded))
}

/// A secret written in the file (not a `$NAME`) that other users can read.
fn readable_secret(path: &Path, loaded: &Loaded) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let literal = |t: &str| !t.is_empty() && super::secret::env_name(t).is_none();
    let config = &loaded.config;
    let pushover = config.notification.pushover.as_ref();
    let secrets = config.server.token.as_deref().is_some_and(literal)
        || pushover.is_some_and(|p| literal(&p.app_api_token) || literal(&p.user_key))
        || literal(&config.slskd.api_key)
        || literal(&config.jellyfin.api_key);
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (secrets && mode & 0o077 != 0).then(|| {
        format!(
            "{0}: it has secrets (server.token, Pushover's keys, slskd.api_key or jellyfin.api_key) and other users can read it; \
             run `chmod 600 {0}`, or write them as \"$NAME\" and set those variables",
            path.display()
        )
    })
}

/// 1-based line and column of byte `offset` in `text`.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Result<Loaded, Vec<String>> {
        check_text(text, "config.toml", Path::new("/home/me"))
    }

    #[test]
    fn a_minimal_config_gets_the_defaults() {
        let loaded = check("version = 1\nlibraries = [\"~/Music\"]\n").unwrap();
        assert_eq!(loaded.libraries, [PathBuf::from("/home/me/Music")]);
        assert_eq!(loaded.listen().port(), 55460);
        assert_eq!(loaded.config.discography.primary_types, ["Album", "EP"]);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    }

    #[test]
    fn syntax_and_unknown_keys_point_at_the_line() {
        let errors = check("version = 1\n[server]\nlisten = 3\n").unwrap_err();
        assert!(errors[0].starts_with("config.toml:3:10:"), "{errors:?}");
        let errors = check("version = 1\nlibrary = []\n").unwrap_err();
        assert!(errors[0].starts_with("config.toml:2:1:"), "{errors:?}");
        assert!(errors[0].contains("unknown field"), "{errors:?}");
    }

    #[test]
    fn the_token_can_come_from_the_environment() {
        let loaded = check("version = 1\n[server]\ntoken = \"$NEWCUT_TEST_NEVER_SET\"\n").unwrap();
        assert!(
            loaded.warnings.iter().any(|w| w.contains(
                "server.token: $NEWCUT_TEST_NEVER_SET isn't set, so the web UI has no sign-in"
            )),
            "{:?}",
            loaded.warnings
        );
        // HOME is set.
        let loaded = check("version = 1\n[server]\ntoken = \"${HOME}\"\n").unwrap();
        assert!(loaded.warnings.iter().all(|w| !w.contains("server.token")));
    }

    #[test]
    fn notification_channels_are_checked() {
        let errors = check(
            "version = 1\n[notification.pushover]\nuser_key = \"u\"\n\
             [notification.command]\nbin = \"notify.sh\"\n",
        )
        .unwrap_err();
        let all = errors.join("\n");
        assert!(
            all.contains("notification.pushover.app_api_token: required"),
            "{all}"
        );
        assert!(
            all.contains("notification.command.bin: \"notify.sh\" must be a full path"),
            "{all}"
        );
        // Off: not needed.
        assert!(check("version = 1\n[notification.pushover]\nenabled = false\n").is_ok());

        let loaded = check(
            "version = 1\n[notification.pushover]\napp_api_token = \"$NEWCUT_TEST_NEVER_SET\"\n\
             user_key = \"u\"\n[notification.command]\nbin = \"/nowhere/notify\"\n\
             [schedule]\nnotification = \"1h\"\n",
        )
        .unwrap();
        let warnings = loaded.warnings.join("\n");
        assert!(
            warnings.contains("$NEWCUT_TEST_NEVER_SET isn't set, so Pushover can't send"),
            "{warnings}"
        );
        assert!(
            warnings.contains("/nowhere/notify isn't there"),
            "{warnings}"
        );
        assert_eq!(
            loaded.notify_bin.as_deref(),
            Some(Path::new("/nowhere/notify"))
        );
        assert_eq!(
            loaded.config.notification.channels(),
            ["pushover", "command"]
        );
        assert_eq!(
            loaded.config.notification.strategy,
            crate::config::Strategy::Missing
        );
        assert!(
            check("version = 1\n[notification]\nstrategy = \"all\"\n")
                .unwrap()
                .config
                .notification
                .strategy
                == crate::config::Strategy::All
        );
        assert!(check("version = 1\n[notification]\nstrategy = \"some\"\n").is_err());
    }

    #[test]
    fn after_filing_is_checked() {
        let slskd = "version = 1\n[slskd]\nenabled = true\napi_key = \"k\"\n\
                     downloads_dir = \"/nowhere/downloads\"\noutput_dir = \"~/Music/%{album}\"\n";
        let loaded = check(&format!(
            "{slskd}[slskd.after_filing]\nbin = \"~/bin/tidy\"\nargs = [\"-q\", \"%{{album_dir}}\"]\n"
        ))
        .unwrap();
        let hook = loaded.import.unwrap().after_filing.unwrap();
        assert_eq!(hook.bin, Path::new("/home/me/bin/tidy"));
        assert_eq!(hook.args, ["-q", "%{album_dir}"]);
        assert_eq!(hook.timeout.as_secs(), 600);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("slskd.after_filing.bin: /home/me/bin/tidy isn't there")),
            "{:?}",
            loaded.warnings
        );
        let errors = check(&format!(
            "{slskd}[slskd.after_filing]\nbin = \"tidy\"\nargs = [\"%{{dir}}\"]\n"
        ))
        .unwrap_err();
        assert!(
            errors
                .join("\n")
                .contains("slskd.after_filing.args: %{dir} isn't a placeholder"),
            "{errors:?}"
        );
        assert!(
            errors
                .join("\n")
                .contains("slskd.after_filing.bin: \"tidy\" must be a full path"),
            "{errors:?}"
        );
        // Nothing's filed: it'd never run.
        let loaded = check(
            "version = 1\n[slskd]\nenabled = true\napi_key = \"k\"\n\
             [slskd.after_filing]\nbin = \"/bin/true\"\n",
        )
        .unwrap();
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("newcut files nothing")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn listening_beyond_this_machine_needs_a_token() {
        let open = "version = 1\n[server]\nlisten = \"0.0.0.0:55460\"\n";
        let errors = check(open).unwrap_err();
        assert!(
            errors[0].contains("reachable from other machines, so server.token is required"),
            "{errors:?}"
        );
        assert!(check(&format!("{open}token = \"secret\"\n")).is_ok());
        assert!(check(&format!("{open}token = \"$NEWCUT_TEST_NEVER_SET\"\n")).is_err());
        assert!(check(&format!("{open}enabled = false\n")).is_ok());
        assert!(check("version = 1\n[server]\nlisten = \"[::1]:55460\"\n").is_ok());
    }

    #[test]
    fn values_are_checked() {
        let errors = check(
            "version = 2\n[server]\nlisten = \"nope\"\ntoken = \"short\"\n\
             [discography]\nprimary_types = [\"LP\"]\nsecondary_types = [\"Bootleg\"]\n\
             [schedule]\nscan = \"10s\"\nfresh = \"often\"\n",
        )
        .unwrap_err();
        assert_eq!(
            errors.len(),
            1,
            "a bad duration is a parse error: {errors:?}"
        );
        let errors = check(
            "version = 2\n[server]\nlisten = \"nope\"\ntoken = \"short\"\n\
             [discography]\nprimary_types = [\"LP\"]\nsecondary_types = [\"Bootleg\"]\n\
             [schedule]\nscan = \"10s\"\n\
             [covers]\nfiles = [\"../cover.jpg\"]\nartist_files = [\"a/b.jpg\"]\n\
             [slskd]\nenabled = true\nserver = \"localhost:5030\"\n\
             output_dir = \"/m/%{artist}/%{albm}\"\ndownloads_dir = \"rel\"\n",
        )
        .unwrap_err();
        let all = errors.join("\n");
        for part in [
            "version 2",
            "server.listen",
            "\"LP\"",
            "\"Bootleg\"",
            "schedule.scan",
            "covers.files",
            "covers.artist_files",
            "slskd.server",
            "slskd.api_key",
            "slskd.output_dir: %{albm}",
            "slskd.downloads_dir",
        ] {
            assert!(all.contains(part), "{part} in {all}");
        }
    }

    #[test]
    fn upgrades_are_checked() {
        let errors = check(
            "version = 1\n[upgrade]\nenabled = true\nper_run = 0\nretry = \"10s\"\n\
             target = \"hi-res\"\n",
        )
        .unwrap_err()
        .join("\n");
        for part in [
            "upgrade.enabled: upgrades need slskd",
            "upgrade.per_run",
            "upgrade.retry",
        ] {
            assert!(errors.contains(part), "{part} in {errors}");
        }
        assert!(check("version = 1\n[upgrade]\ntarget = \"lossy\"\n").is_err());
        let loaded = check(
            "version = 1\n[slskd]\nenabled = true\napi_key = \"k\"\n\
             downloads_dir = \"/d\"\noutput_dir = \"/m/%{album}\"\n\
             [upgrade]\nenabled = true\ntarget = \"320\"\nreplaced_dir = \"~/old\"\n",
        )
        .unwrap();
        assert_eq!(loaded.config.upgrade.target, crate::quality::Tier::R320);
        assert!(loaded.import.unwrap().replaced.ends_with("old"));
    }
}
