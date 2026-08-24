use crate::cli::{ListArgs, ListFormat};
use crate::error::Result;
use crate::paths::Layout;
use crate::state;

pub fn run(layout: &Layout, args: &ListArgs) -> Result<()> {
    let containers = state::list(layout)?;

    if args.quiet {
        for container in &containers {
            println!("{}", container.id());
        }
        return Ok(());
    }

    match args.format {
        ListFormat::Json => {
            let rows: Vec<_> = containers
                .iter()
                .map(|container| container.oci_state())
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        ListFormat::Table => {
            let (id, pid, status, bundle) = ("ID", "PID", "STATUS", "BUNDLE");
            println!("{id:<24}{pid:<8}{status:<10}{bundle:<32}CREATED");

            for container in &containers {
                let status = container.status();
                let pid = match status {
                    state::Status::Stopped => "0".to_string(),
                    _ => container.state.pid.to_string(),
                };

                println!(
                    "{:<24}{:<8}{:<10}{:<32}{}",
                    container.id(),
                    pid,
                    status.to_string(),
                    container.state.bundle.display(),
                    container.state.created,
                );
            }
        }
    }

    Ok(())
}
