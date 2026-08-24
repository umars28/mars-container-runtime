use std::io::Write;

use oci_spec::runtime::{
    LinuxCpuBuilder, LinuxMemoryBuilder, LinuxPidsBuilder, LinuxResources, LinuxResourcesBuilder,
};

use crate::bundle::Bundle;
use crate::cli::{CreateArgs, ExecArgs, KillArgs, RunArgs, UpdateArgs};
use crate::container::{self, CreateOptions, exec::ExecOptions, signal};
use crate::error::{Error, IoContext, Result};
use crate::paths::Layout;
use crate::state::Container;
use crate::telemetry;

pub fn create(
    layout: &Layout,
    args: &CreateArgs,
    rootless: bool,
    otlp: Option<&str>,
) -> Result<()> {
    reject_unsupported(args.preserve_fds, args.no_pivot)?;

    let mut clock = telemetry::Recorder::new();
    clock.begin("bundle");
    let bundle = Bundle::load(&args.bundle)?;

    let outcome = container::create(
        layout,
        &bundle,
        &CreateOptions {
            id: args.id.clone(),
            pid_file: args.pid_file.clone(),
            console_socket: args.console_socket.clone(),
            rootless,
        },
        &mut clock,
    );

    trace(otlp, "container.create", &args.id, &mut clock);

    outcome
}

fn trace(otlp: Option<&str>, root: &str, id: &str, clock: &mut telemetry::Recorder) {
    let Some(endpoint) = telemetry::endpoint(otlp) else {
        clock.end();
        return;
    };

    if let Err(error) = telemetry::export(&endpoint, root, id, clock) {
        tracing::warn!("could not export the startup trace: {error}");
    }
}

pub fn run(layout: &Layout, args: &RunArgs, rootless: bool, otlp: Option<&str>) -> Result<u8> {
    reject_unsupported(args.preserve_fds, args.no_pivot)?;

    let mut clock = telemetry::Recorder::new();
    clock.begin("bundle");
    let bundle = Bundle::load(&args.bundle)?;

    container::run(
        layout,
        &bundle,
        &CreateOptions {
            id: args.id.clone(),
            pid_file: args.pid_file.clone(),
            console_socket: args.console_socket.clone(),
            rootless,
        },
        args.detach,
        args.keep,
        &mut clock,
        &|clock| trace(otlp, "container.run", &args.id, clock),
    )
}

pub fn exec(layout: &Layout, args: &ExecArgs) -> Result<u8> {
    if args.preserve_fds != 0 {
        return Err(Error::Unimplemented("exec --preserve-fds"));
    }

    container::exec::exec(
        layout,
        &args.id,
        &ExecOptions {
            argv: args.argv.clone(),
            env: args.env.clone(),
            cwd: args.cwd.clone(),
            process: args.process.clone(),
            user: args.user.clone(),
            additional_gids: args.additional_gids.clone(),
            console_socket: args.console_socket.clone(),
            tty: args.tty,
            detach: args.detach,
            pid_file: args.pid_file.clone(),
            ignore_paused: args.ignore_paused,
        },
    )
}

pub fn kill(layout: &Layout, args: &KillArgs) -> Result<()> {
    let signal = signal::parse(&args.signal)?;

    container::kill(layout, &args.id, signal, args.all)
}

pub fn state(layout: &Layout, id: &str) -> Result<()> {
    let container = Container::load(layout, id)?;
    let json = serde_json::to_string_pretty(&container.oci_state())?;

    let mut out = std::io::stdout().lock();
    writeln!(out, "{json}").ctx("write the container state to stdout")
}

pub fn update(layout: &Layout, args: &UpdateArgs) -> Result<()> {
    let resources = match &args.resources {
        Some(path) if path.as_os_str() == "-" => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .ctx("read resources from stdin")?;
            serde_json::from_str::<LinuxResources>(&text)?
        }
        Some(path) => {
            let text = std::fs::read_to_string(path).ctx(format!("read {}", path.display()))?;
            serde_json::from_str::<LinuxResources>(&text)?
        }
        None => from_flags(args)?,
    };

    container::update(layout, &args.id, &resources)
}

fn from_flags(args: &UpdateArgs) -> Result<LinuxResources> {
    let mut builder = LinuxResourcesBuilder::default();
    let mut touched = false;

    if args.memory.is_some() || args.memory_swap.is_some() {
        let mut memory = LinuxMemoryBuilder::default();
        if let Some(limit) = args.memory {
            memory = memory.limit(limit);
        }
        if let Some(swap) = args.memory_swap {
            memory = memory.swap(swap);
        }
        builder = builder.memory(memory.build()?);
        touched = true;
    }

    if args.cpu_quota.is_some()
        || args.cpu_period.is_some()
        || args.cpu_share.is_some()
        || args.cpuset_cpus.is_some()
        || args.cpuset_mems.is_some()
    {
        let mut cpu = LinuxCpuBuilder::default();
        if let Some(quota) = args.cpu_quota {
            cpu = cpu.quota(quota);
        }
        if let Some(period) = args.cpu_period {
            cpu = cpu.period(period);
        }
        if let Some(shares) = args.cpu_share {
            cpu = cpu.shares(shares);
        }
        if let Some(cpus) = &args.cpuset_cpus {
            cpu = cpu.cpus(cpus.clone());
        }
        if let Some(mems) = &args.cpuset_mems {
            cpu = cpu.mems(mems.clone());
        }
        builder = builder.cpu(cpu.build()?);
        touched = true;
    }

    if let Some(limit) = args.pids_limit {
        builder = builder.pids(LinuxPidsBuilder::default().limit(limit).build()?);
        touched = true;
    }

    if !touched {
        return Err(Error::Invalid(
            "update needs at least one limit, or --resources with a JSON document".to_string(),
        ));
    }

    Ok(builder.build()?)
}

fn reject_unsupported(preserve_fds: i32, no_pivot: bool) -> Result<()> {
    if preserve_fds != 0 {
        return Err(Error::Unimplemented("--preserve-fds"));
    }
    if no_pivot {
        return Err(Error::Unimplemented("--no-pivot"));
    }

    Ok(())
}
