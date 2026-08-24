use std::fs::OpenOptions;
use std::io;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use crate::cli::{Cli, LogFormat};
use crate::error::{IoContext, Result};

pub fn init(cli: &Cli) -> Result<()> {
    let default = if cli.debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_env("MARS_LOG").unwrap_or_else(|_| EnvFilter::new(default));

    let writer = match &cli.log {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ctx(format!("open log file {}", path.display()))?;
            BoxMakeWriter::new(Arc::new(file))
        }
        None => BoxMakeWriter::new(io::stderr),
    };

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_target(false)
        .with_ansi(false);

    match cli.log_format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }

    Ok(())
}
