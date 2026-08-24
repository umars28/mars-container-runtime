use std::time::Duration;

use serde::Serialize;

use crate::cgroup::events as cgroup_events;
use crate::cli::EventsArgs;
use crate::error::{Error, Result};
use crate::paths::Layout;
use crate::state::{Container, Status};

#[derive(Serialize)]
struct Event {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Stats>,
}

#[derive(Serialize)]
struct Stats {
    memory: Memory,
    cpu: Cpu,
    pids: Pids,
}

#[derive(Serialize)]
struct Memory {
    usage: u64,
    peak: u64,
    limit_events: u64,
    oom_kill: u64,
}

#[derive(Serialize)]
struct Cpu {
    usage_usec: u64,
    nr_periods: u64,
    nr_throttled: u64,
    throttled_usec: u64,
}

#[derive(Serialize)]
struct Pids {
    current: u64,
}

pub fn run(layout: &Layout, args: &EventsArgs) -> Result<()> {
    let container = Container::load(layout, &args.id)?;
    container.require(&[Status::Created, Status::Running, Status::Paused])?;

    if args.stats {
        println!("{}", serde_json::to_string(&sample(&container)?)?);
        return Ok(());
    }

    let interval = parse_interval(&args.interval)?;

    loop {
        if container.status() == Status::Stopped {
            println!(
                "{}",
                serde_json::to_string(&Event {
                    kind: "exit",
                    id: container.id().to_string(),
                    data: None,
                })?
            );
            return Ok(());
        }

        println!("{}", serde_json::to_string(&sample(&container)?)?);
        std::thread::sleep(interval);
    }
}

fn sample(container: &Container) -> Result<Event> {
    let cgroup = container.cgroup();

    let memory = cgroup_events::memory_events(&cgroup).unwrap_or_default();
    let cpu = cgroup_events::cpu_stat(&cgroup).unwrap_or_default();

    Ok(Event {
        kind: "stats",
        id: container.id().to_string(),
        data: Some(Stats {
            memory: Memory {
                usage: single_value(&cgroup, "memory.current"),
                peak: single_value(&cgroup, "memory.peak"),
                limit_events: memory.max,
                oom_kill: memory.oom_kill,
            },
            cpu: Cpu {
                usage_usec: cpu.usage_usec,
                nr_periods: cpu.nr_periods,
                nr_throttled: cpu.nr_throttled,
                throttled_usec: cpu.throttled_usec,
            },
            pids: Pids {
                current: single_value(&cgroup, "pids.current"),
            },
        }),
    })
}

fn single_value(cgroup: &crate::cgroup::v2::Cgroup, file: &str) -> u64 {
    cgroup
        .read(file)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn parse_interval(text: &str) -> Result<Duration> {
    let trimmed = text.trim();

    let (value, multiplier) = if let Some(rest) = trimmed.strip_suffix("ms") {
        (rest, 1)
    } else if let Some(rest) = trimmed.strip_suffix('s') {
        (rest, 1_000)
    } else if let Some(rest) = trimmed.strip_suffix('m') {
        (rest, 60_000)
    } else {
        (trimmed, 1_000)
    };

    let millis: u64 = value
        .parse()
        .map_err(|_| Error::Invalid(format!("cannot parse the interval {text:?}")))?;

    if millis == 0 {
        return Err(Error::Invalid("the interval must be positive".to_string()));
    }

    Ok(Duration::from_millis(millis * multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_accept_the_units_runc_accepts() {
        assert_eq!(parse_interval("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_interval("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_interval("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_interval("3").unwrap(), Duration::from_secs(3));
    }

    #[test]
    fn a_zero_or_unparseable_interval_is_rejected() {
        assert!(parse_interval("0s").is_err());
        assert!(parse_interval("soon").is_err());
    }
}
