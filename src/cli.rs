//! Command-line interface definition.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "newcut",
    version,
    about = "Know which albums you have, which you don't, and what's new from the artists in your music library"
)]
pub struct Cli {
    /// The config file [default: `$NEWCUT_CONFIG_DIR/config.toml`, or
    /// `$XDG_CONFIG_HOME/newcut/config.toml`]
    #[arg(
        long,
        short = 'c',
        env = "NEWCUT_CONFIG",
        value_name = "PATH",
        global = true
    )]
    pub config: Option<PathBuf>,

    /// `NAME=value` lines to load into the environment first (for the
    /// config's "$NAME" secrets); variables already set win [default:
    /// `$NEWCUT_CONFIG_DIR/env`, or `$XDG_CONFIG_HOME/newcut/env`, when it's
    /// there]
    #[arg(long, value_name = "FILE", global = true)]
    pub env_file: Option<PathBuf>,

    /// Increase log verbosity (-v, -vv)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Only log errors
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Log output format
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty, global = true)]
    pub log_format: LogFormat,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Create a commented config file
    Init(InitArgs),
    /// Run the daemon: scan, check for releases, and serve the web UI
    Run,
    /// Scan the libraries now (the daemon does it, if it's running)
    Scan,
    /// Check discographies and fresh releases now (the daemon does
    /// it, if it's running)
    Sync(SyncArgs),
    /// Send notifications of what's new now (the daemon does it, if it's
    /// running)
    Notify(NotifyArgs),
    /// List the albums you don't have
    Missing(ListArgs),
    /// List new and upcoming releases
    New(ListArgs),
    /// List artists, with what you have and don't
    Artists,
    /// Leave a release group out of Missing and the feed
    Ignore(IgnoreArgs),
    /// Count a release group in again
    Unignore(IgnoreArgs),
    /// Have newcut look for a release group on Soulseek (even one still
    /// to come) until it's there, and download it
    Request(IgnoreArgs),
    /// Take a request back
    Unrequest(IgnoreArgs),
    /// Check, show or import the config
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// See and control the running daemon
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// A music folder to scan; repeatable
    #[arg(long = "library", short = 'l', value_name = "DIR")]
    pub libraries: Vec<String>,
    /// Replace an existing config file
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    /// Only this artist (MusicBrainz id or name)
    #[arg(long, value_name = "MBID|NAME")]
    pub artist: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct NotifyArgs {
    /// Send a test notification through each channel instead
    #[arg(long)]
    pub test: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// Only this artist (MusicBrainz id or name)
    #[arg(long, value_name = "MBID|NAME")]
    pub artist: Option<String>,
    /// Print JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct IgnoreArgs {
    /// Release group MusicBrainz ids
    #[arg(required = true, value_name = "MBID")]
    pub release_groups: Vec<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCmd {
    /// Check the config file
    Check,
    /// Print where the config file is
    Path,
    /// Print the config's JSON Schema
    Schema,
    /// Print the config the daemon runs with (the latest revision), its
    /// token masked
    Show {
        /// Print the token as it is
        #[arg(long)]
        show_secrets: bool,
    },
    /// What the config file changes from the latest revision
    Diff {
        /// Print the token as it is
        #[arg(long)]
        show_secrets: bool,
    },
    /// Store the config file as the latest revision
    Import,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DaemonCmd {
    /// How each service is doing
    Status,
    /// Pause a service
    Pause { service: String },
    /// Resume a paused service
    Resume { service: String },
    /// Restart a service, or every one
    Restart { service: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}
