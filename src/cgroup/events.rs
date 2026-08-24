use std::collections::HashMap;

use crate::error::Result;

use super::v2::Cgroup;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MemoryEvents {
    pub low: u64,
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CpuStat {
    pub usage_usec: u64,
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_usec: u64,
}

pub fn parse_flat_keyed(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let key = parts.next()?;
            let value = parts.next()?.parse().ok()?;
            Some((key.to_string(), value))
        })
        .collect()
}

pub fn memory_events(cgroup: &Cgroup) -> Result<MemoryEvents> {
    let parsed = parse_flat_keyed(&cgroup.read("memory.events")?);

    Ok(MemoryEvents {
        low: parsed.get("low").copied().unwrap_or(0),
        high: parsed.get("high").copied().unwrap_or(0),
        max: parsed.get("max").copied().unwrap_or(0),
        oom: parsed.get("oom").copied().unwrap_or(0),
        oom_kill: parsed.get("oom_kill").copied().unwrap_or(0),
    })
}

pub fn cpu_stat(cgroup: &Cgroup) -> Result<CpuStat> {
    let parsed = parse_flat_keyed(&cgroup.read("cpu.stat")?);

    Ok(CpuStat {
        usage_usec: parsed.get("usage_usec").copied().unwrap_or(0),
        nr_periods: parsed.get("nr_periods").copied().unwrap_or(0),
        nr_throttled: parsed.get("nr_throttled").copied().unwrap_or(0),
        throttled_usec: parsed.get("throttled_usec").copied().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_memory_events_format() {
        let parsed =
            parse_flat_keyed("low 0\nhigh 0\nmax 14\noom 1\noom_kill 1\noom_group_kill 0\n");

        assert_eq!(parsed.get("max"), Some(&14));
        assert_eq!(parsed.get("oom_kill"), Some(&1));
        assert_eq!(parsed.get("low"), Some(&0));
    }

    #[test]
    fn ignores_lines_it_cannot_parse() {
        let parsed = parse_flat_keyed("good 7\nmalformed\nalso bad-value\n");

        assert_eq!(parsed.get("good"), Some(&7));
        assert_eq!(parsed.len(), 1);
    }
}
