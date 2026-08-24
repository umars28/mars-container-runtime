use std::fs;

use crate::cli::{ListArgs, ListFormat};
use crate::error::{IoContext, Result};
use crate::paths::Layout;

pub fn run(layout: &Layout, args: &ListArgs) -> Result<()> {
    let ids = collect(layout)?;

    if args.quiet {
        for id in &ids {
            println!("{id}");
        }
        return Ok(());
    }

    match args.format {
        ListFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&ids)?);
        }
        ListFormat::Table => {
            let (h_id, h_pid, h_status, h_bundle) = ("ID", "PID", "STATUS", "BUNDLE");
            println!("{h_id:<24}{h_pid:<8}{h_status:<12}{h_bundle}");

            let unknown = "-";
            for id in &ids {
                println!("{id:<24}{unknown:<8}{unknown:<12}{unknown}");
            }
        }
    }

    Ok(())
}

fn collect(layout: &Layout) -> Result<Vec<String>> {
    if !layout.root().is_dir() {
        return Ok(Vec::new());
    }

    let mut ids = Vec::new();
    let entries =
        fs::read_dir(layout.root()).ctx(format!("read state root {}", layout.root().display()))?;

    for entry in entries {
        let entry = entry.ctx("read state root entry")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if layout.exists(&name) {
            ids.push(name);
        }
    }

    ids.sort();
    Ok(ids)
}
