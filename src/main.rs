use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use tidepool::app::{self, UsageError, exit};
use tidepool::cli::{Cli, LogFormat};

fn init_tracing(cli: &Cli) {
    let level = match (cli.quiet, cli.verbose) {
        (true, _) => "error",
        (false, 0) => "info",
        (false, 1) => "debug",
        (false, _) => "trace",
    };
    let filter = EnvFilter::try_from_env("TIDEPOOL_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},rusqlite_migration=warn")));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match cli.log_format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.without_time().with_target(false).init(),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse(); // usage errors exit 2
    init_tracing(&cli);
    let code = match app::run(&cli, &mut std::io::stdout().lock()).await {
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
    ExitCode::from(code as u8)
}
