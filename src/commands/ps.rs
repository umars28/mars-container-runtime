use std::process::Command;

use crate::cli::PsArgs;
use crate::error::{Error, IoContext, Result};
use crate::paths::Layout;
use crate::state::{Container, Status};

pub fn run(layout: &Layout, args: &PsArgs) -> Result<()> {
    let container = Container::load(layout, &args.id)?;
    container.require(&[Status::Created, Status::Running, Status::Paused])?;

    let pids = container.cgroup().procs()?;

    match args.format.as_str() {
        "json" => {
            let raw: Vec<i32> = pids.iter().map(|pid| pid.as_raw()).collect();
            println!("{}", serde_json::to_string(&raw)?);
            Ok(())
        }
        "table" => table(&pids, &args.ps_options),
        other => Err(Error::Invalid(format!(
            "unknown ps format {other:?}, expected table or json"
        ))),
    }
}

fn table(pids: &[nix::unistd::Pid], ps_options: &[String]) -> Result<()> {
    if pids.is_empty() {
        println!("no processes in the container cgroup");
        return Ok(());
    }

    let mut command = Command::new("ps");

    if ps_options.is_empty() {
        command.arg("-f");
    } else {
        command.args(ps_options);
    }

    let list = pids
        .iter()
        .map(|pid| pid.as_raw().to_string())
        .collect::<Vec<_>>()
        .join(",");

    command.arg("-p").arg(&list);

    let status = command
        .status()
        .ctx("run ps to describe the container's processes")?;

    if status.success() {
        return Ok(());
    }

    Err(Error::Invalid(format!("ps exited with {status}")))
}
