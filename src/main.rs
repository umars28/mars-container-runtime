use std::process::ExitCode;

use clap::Parser;
use mars::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = mars::logging::init(&cli) {
        eprintln!("mars: {err}");
        return ExitCode::FAILURE;
    }

    match mars::commands::dispatch(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            tracing::error!("{err}");
            eprintln!("mars: {err}");
            ExitCode::FAILURE
        }
    }
}
