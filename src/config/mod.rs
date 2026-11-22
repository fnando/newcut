//! The config file (`config.toml`): what it holds, how it's read and
//! checked, and the revisions the daemon runs with.

pub mod diff;
pub mod duration;
pub mod load;
pub mod secret;
pub mod store;

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use load::{Loaded, check_text, load_file};

/// The config format version this build reads.
pub const VERSION: u32 = 1;

/// Where the config's JSON Schema is published (the `#:schema` line).
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/fnando/newcut/main/schema/newcut.v1.schema.json";

/// Release group primary types MusicBrainz uses.
pub const PRIMARY_TYPES: [&str; 5] = ["Album", "Single", "EP", "Broadcast", "Other"];

/// Release group secondary types MusicBrainz uses.
pub const SECONDARY_TYPES: [&str; 12] = [
    "Compilation",
    "Soundtrack",
    "Spokenword",
    "Interview",
    "Audiobook",
    "Audio drama",
    "Live",
    "Remix",
    "DJ-mix",
    "Mixtape/Street",
    "Demo",
    "Field recording",
];

/// The whole config file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "newcut config")]
pub struct Config {
    /// The config format version: `1`.
    pub version: u32,
    /// Music folders to scan (`~` and `$VAR` are expanded).
    #[serde(default)]
    pub libraries: Vec<String>,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub musicbrainz: MusicBrainzConfig,
    #[serde(default)]
    pub listenbrainz: ListenBrainzConfig,
    #[serde(default)]
    pub discography: DiscographyConfig,
    #[serde(default)]
    pub covers: CoversConfig,
    #[serde(default)]
    pub slskd: SlskdConfig,
    #[serde(default)]
    pub jellyfin: JellyfinConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub notification: NotificationConfig,
    #[serde(default)]
    pub upgrade: UpgradeConfig,
}

/// The web UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Whether the daemon serves the web UI.
    pub enabled: bool,
    /// The address to listen on.
    pub listen: String,
    /// When set, every page but the feeds needs it (sign in, or a bearer
    /// header). `"$NAME"` or `"${NAME}"` reads it from that environment
    /// variable instead.
    pub token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:55460".into(),
            token: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct MusicBrainzConfig {
    /// The MusicBrainz server (or a mirror; only musicbrainz.org is
    /// throttled to one request a second).
    pub server: String,
    /// How MusicBrainz can reach you (an email or URL); it goes into the
    /// User-Agent, as MusicBrainz asks.
    pub contact: Option<String>,
}

impl Default for MusicBrainzConfig {
    fn default() -> Self {
        Self {
            server: crate::musicbrainz::DEFAULT_SERVER.into(),
            contact: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ListenBrainzConfig {
    /// Look for fresh releases on ListenBrainz.
    pub enabled: bool,
    pub server: String,
}

impl Default for ListenBrainzConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            server: crate::listenbrainz::DEFAULT_SERVER.into(),
        }
    }
}

/// Which release groups count toward an artist's discography.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct DiscographyConfig {
    /// Album, Single, EP, Broadcast, Other.
    pub primary_types: Vec<String>,
    /// Release groups with any other secondary type (Compilation, Live,
    /// Remix, …) are left out. Empty: only those with none.
    pub secondary_types: Vec<String>,
    /// Releases whose titles match any of these regular expressions are
    /// left out too, e.g. `"(?i)\\bao vivo\\b"`.
    pub exclude: TitlePatterns,
}

impl Default for DiscographyConfig {
    fn default() -> Self {
        Self {
            primary_types: vec!["Album".into(), "EP".into()],
            secondary_types: Vec::new(),
            exclude: TitlePatterns::default(),
        }
    }
}

/// Regular expressions for titles, compiled once.
#[derive(Debug, Clone, Default)]
pub struct TitlePatterns {
    sources: Vec<String>,
    set: regex::RegexSet,
}

impl TitlePatterns {
    /// # Errors
    ///
    /// When a pattern isn't a valid regular expression.
    pub fn new(sources: Vec<String>) -> Result<Self, regex::Error> {
        let set = regex::RegexSet::new(&sources)?;
        Ok(Self { sources, set })
    }

    /// Whether any pattern matches `title`.
    #[must_use]
    pub fn matches(&self, title: &str) -> bool {
        self.set.is_match(title)
    }

    #[must_use]
    pub fn sources(&self) -> &[String] {
        &self.sources
    }
}

impl PartialEq for TitlePatterns {
    fn eq(&self, other: &Self) -> bool {
        self.sources == other.sources
    }
}

impl Eq for TitlePatterns {}

impl Serialize for TitlePatterns {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.sources.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TitlePatterns {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let sources = Vec::<String>::deserialize(deserializer)?;
        Self::new(sources).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for TitlePatterns {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TitlePatterns".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "array",
            "items": { "type": "string", "format": "regex" },
            "description": "Regular expressions (Rust syntax; `(?i)` ignores case)",
        })
    }
}

impl DiscographyConfig {
    /// Whether a release group of these types counts.
    #[must_use]
    pub fn counts(&self, primary: Option<&str>, secondary: &[String]) -> bool {
        let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        primary.is_some_and(|p| self.primary_types.iter().any(|t| same(t, p)))
            && secondary
                .iter()
                .all(|s| self.secondary_types.iter().any(|t| same(t, s)))
    }

    /// Whether a release of these types, and this title, counts.
    #[must_use]
    pub fn counts_release(&self, title: &str, primary: Option<&str>, secondary: &[String]) -> bool {
        self.counts(primary, secondary) && !self.exclude.matches(title)
    }
}

/// Where album covers and artist images in the web UI come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct CoversConfig {
    /// Show the covers of albums you have from their folders (one of
    /// `files`), instead of the Cover Art Archive's. Albums without one, and
    /// those you don't have, still use the Cover Art Archive.
    pub local: bool,
    /// The cover file names to look for in an album's folder, in order
    /// (case doesn't matter).
    pub files: Vec<String>,
    /// The image file names to look for in an artist's folder (the one
    /// their album folders are in), in order (case doesn't matter). Artists
    /// without one get their initial.
    pub artist_files: Vec<String>,
}

impl Default for CoversConfig {
    fn default() -> Self {
        Self {
            local: false,
            files: vec!["cover.jpg".into(), "folder.jpg".into()],
            artist_files: vec!["artist.jpg".into(), "folder.jpg".into()],
        }
    }
}

/// slskd (<https://github.com/slskd/slskd>): searching Soulseek for the
/// albums you're missing, and downloading them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct SlskdConfig {
    /// Missing albums link to a page that searches slskd for them.
    pub enabled: bool,
    /// slskd's web address.
    pub server: String,
    /// An API key from slskd's config (`web.authentication.api_keys`), or
    /// `"$NAME"` to read it from that environment variable.
    pub api_key: String,
    /// slskd's downloads folder (`directories.downloads`), as this machine
    /// sees it (`~` and `$VAR` are expanded). With `output_dir`, finished
    /// downloads that match a release exactly are tagged and filed.
    pub downloads_dir: Option<String>,
    /// Where tagged albums go: a path with placeholders, e.g.
    /// `"~/Music/%{artist}/%{album} (%{year})"`. Placeholders: `artist`,
    /// `artistid` (MusicBrainz IDs, joined with `; ` as Picard does),
    /// `artist_sort`, `album`, `year` (the first release's), `date` (this
    /// release's), `release_year`, `country`, `media`, `label`,
    /// `catalog_number`, `type`, `releaseid`, `releasegroupid`.
    pub output_dir: Option<String>,
    /// Your own program, run on each album folder newcut files: after it's
    /// tagged and moved under `output_dir`, before Jellyfin hears of it.
    pub after_filing: Option<AfterFilingConfig>,
}

/// A program run on each album folder newcut files (in that folder). It
/// must leave the folder where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AfterFilingConfig {
    /// The program to run (`~` and `$VAR` are expanded).
    pub bin: String,
    /// Its arguments. Placeholders: `album_dir` (the album's folder), and
    /// those of `output_dir`, e.g. `["--dir", "%{album_dir}"]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Stopped after this long.
    #[serde(default = "after_filing_timeout")]
    pub timeout: Every,
}

/// Jellyfin: told to look at the albums newcut files, so they show up
/// there right away.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct JellyfinConfig {
    pub enabled: bool,
    /// Jellyfin's web address.
    pub server: String,
    /// An API key (Dashboard → API Keys), or `"$NAME"` to read it from
    /// that environment variable.
    pub api_key: String,
    /// How Jellyfin sees your folders, when it isn't as newcut does: each
    /// folder of newcut's (`~` and `$VAR` are expanded) to the same folder
    /// in Jellyfin, e.g. `{ "/Volumes/storage/media/music" = "/data/music" }`.
    pub paths: std::collections::BTreeMap<String, String>,
}

impl Default for SlskdConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: crate::slskd::DEFAULT_SERVER.into(),
            api_key: String::new(),
            downloads_dir: None,
            output_dir: None,
            after_filing: None,
        }
    }
}

/// Notifications of new releases (each channel is off until its section
/// is there).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct NotificationConfig {
    /// What to tell you about: `missing` (new releases you don't have) or
    /// `all` (owned ones too).
    pub strategy: Strategy,
    pub pushover: Option<PushoverConfig>,
    pub command: Option<CommandConfig>,
}

impl NotificationConfig {
    /// The channels that are on.
    #[must_use]
    pub fn channels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.pushover.as_ref().is_some_and(|p| p.enabled) {
            out.push("pushover");
        }
        if self.command.as_ref().is_some_and(|c| c.enabled) {
            out.push("command");
        }
        out
    }
}

/// What notifications tell you about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// New releases you don't have.
    #[default]
    Missing,
    /// New releases, owned or not.
    All,
}

/// Notifications through Pushover (<https://pushover.net>).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushoverConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Your Pushover application's API token, or `"$NAME"` to read it from
    /// that environment variable.
    #[serde(default)]
    pub app_api_token: String,
    /// Your Pushover user (or group) key, or `"$NAME"`.
    #[serde(default)]
    pub user_key: String,
    /// The Pushover API.
    #[serde(default = "pushover_server")]
    pub server: String,
}

/// Notifications through your own program: it gets them as JSON on stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The program to run (`~` and `$VAR` are expanded), with no arguments.
    pub bin: String,
    /// Stopped after this long.
    #[serde(default = "command_timeout")]
    pub timeout: Every,
}

fn yes() -> bool {
    true
}

fn pushover_server() -> String {
    crate::notify::PUSHOVER_SERVER.into()
}

fn command_timeout() -> Every {
    Every(Duration::from_secs(60))
}

fn after_filing_timeout() -> Every {
    Every(Duration::from_secs(600))
}

/// Upgrades: albums you have below `target` are searched for on slskd
/// again, and replaced by a better folder (the one closest to `target`
/// that meets it, or else the best one that's better than yours).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct UpgradeConfig {
    /// Look for better copies of the albums you have (needs slskd's
    /// `downloads_dir` and `output_dir`).
    pub enabled: bool,
    /// The quality to aim for: `"320"` (lossy at 320 kbps), `"lossless"`
    /// (16-bit, as on CDs) or `"hi-res"` (24-bit lossless).
    pub target: crate::quality::Tier,
    /// Albums searched for each run, at most (Soulseek doesn't like many
    /// searches).
    pub per_run: u32,
    /// How long before an album that couldn't be upgraded is searched for
    /// again.
    pub retry: Every,
    /// Tell you (through the `[notification]` channels) when an album is
    /// upgraded.
    pub notify: bool,
    /// Where the files an upgrade replaces go (`~` and `$VAR` are
    /// expanded); newcut's data folder's `replaced` by default.
    pub replaced_dir: Option<String>,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target: crate::quality::Tier::Lossless,
            per_run: 5,
            retry: Every(Duration::from_hours(14 * 24)),
            notify: false,
            replaced_dir: None,
        }
    }
}

/// How often each service does its work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ScheduleConfig {
    /// Scan the libraries.
    pub scan: Every,
    /// Check each artist's discography on MusicBrainz.
    pub artist: Every,
    /// Check ListenBrainz's fresh releases.
    pub fresh: Every,
    /// Release groups out within this long (or coming) count as new.
    pub new_window: Every,
    /// Send notifications of new releases.
    pub notification: Every,
    /// Look for upgrades of albums below `upgrade.target`.
    pub upgrade: Every,
    /// Look on Soulseek again for the albums you requested and it didn't have.
    pub request: Every,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        let hours = |h: u64| Every(Duration::from_secs(h * 3600));
        Self {
            scan: hours(6),
            artist: hours(7 * 24),
            fresh: hours(24),
            new_window: hours(60 * 24),
            notification: hours(24),
            upgrade: hours(24),
            request: hours(6),
        }
    }
}

/// A duration written as `"6h"`, `"7d"`, `"1h30m"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Every(pub Duration);

impl Serialize for Every {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&duration::format(self.0))
    }
}

impl<'de> Deserialize<'de> for Every {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        duration::parse(&text)
            .map(Every)
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Every {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^([0-9]+[smhdw])+$",
            "description": "A duration such as \"6h\", \"7d\" or \"1h30m\"",
        })
    }
}

/// The config's JSON Schema, pretty-printed.
#[must_use]
pub fn schema_json() -> String {
    let schema = schemars::schema_for!(Config);
    let mut text = serde_json::to_string_pretty(&schema).expect("the schema serializes");
    text.push('\n');
    text
}

/// What `newcut init` writes.
#[must_use]
pub fn starter(libraries: &[String]) -> String {
    let list = if libraries.is_empty() {
        "\"~/Music\"".to_string()
    } else {
        libraries
            .iter()
            .map(|l| toml::Value::String(l.clone()).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    include_str!("../../examples/config.toml").replace(
        "libraries = [\"~/Music\"]",
        &format!("libraries = [{list}]"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discography_types_filter_release_groups() {
        let d = DiscographyConfig::default();
        assert!(d.counts(Some("Album"), &[]));
        assert!(d.counts(Some("ep"), &[]));
        assert!(!d.counts(Some("Single"), &[]));
        assert!(!d.counts(None, &[]));
        assert!(!d.counts(Some("Album"), &["Live".into()]));
        let live = DiscographyConfig {
            secondary_types: vec!["Live".into()],
            ..DiscographyConfig::default()
        };
        assert!(live.counts(Some("Album"), &["Live".into()]));
        assert!(!live.counts(Some("Album"), &["Live".into(), "Compilation".into()]));
        let toml = r#"exclude = ['(?i)\bao vivo\b', '(?i)[(\[]live\b']"#;
        let d: DiscographyConfig = toml::from_str(toml).unwrap();
        assert!(!d.counts_release("Refestança (Ao Vivo)", Some("Album"), &[]));
        assert!(!d.counts_release("Tested (Live)", Some("Album"), &[]));
        assert!(d.counts_release("Liverbirds", Some("Album"), &[]));
        assert!(d.counts_release("Nobody Can Live Forever", Some("Album"), &[]));
        let bad = toml::from_str::<DiscographyConfig>("exclude = ['(']").unwrap_err();
        assert!(bad.to_string().contains("regex parse error"), "{bad}");
    }

    #[test]
    fn the_starter_config_is_valid() {
        let text = starter(&["/music/a".into(), "~/b".into()]);
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.libraries, ["/music/a", "~/b"]);
        assert_eq!(config.schedule, ScheduleConfig::default());
        assert_eq!(config.discography, DiscographyConfig::default());
        assert_eq!(config.covers, CoversConfig::default());
        assert_eq!(config.upgrade, UpgradeConfig::default());
    }
}
