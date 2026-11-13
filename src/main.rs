use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use newcut::app::{self, UsageError, exit};
use newcut::cli::{Cli, LogFormat};

fn init_tracing(cli: &Cli) {
    let level = match (cli.quiet, cli.verbose) {
        (true, _) => "error",
        (false, 0) => "info",
        (false, 1) => "debug",
        (false, _) => "trace",
    };
    let filter = EnvFilter::try_from_env("NEWCUT_LOG").unwrap_or_else(|_| {
        // Verbosity is about newcut's own output; libraries only warn.
        let directives = if cli.quiet {
            "error".to_string()
        } else {
            format!("warn,newcut={level}")
        };
        EnvFilter::new(directives)
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match cli.log_format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder
            .without_time()
            .with_target(false)
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .init(),
    }
}

fn main() -> ExitCode {
    // Before clap reads NEWCUT_* and before any thread starts: loading sets
    // environment variables.
    let given = newcut::env_file::arg(std::env::args().skip(1));
    if let Err(err) = newcut::env_file::load(given.as_deref()) {
        eprintln!("error: {err}");
        return ExitCode::from(2);
    }
    let cli = Cli::parse(); // usage errors exit 2
    init_tracing(&cli);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let result = runtime.block_on(app::run(&cli, &mut std::io::stdout().lock()));
    let code = match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            if err.is::<UsageError>() {
                exit::USAGE
            } else {
                exit::FATAL
            }
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
