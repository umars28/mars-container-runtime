use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

pub const ENDPOINT_ENV: &str = "MARS_OTLP_ENDPOINT";
const SERVICE: &str = "mars";
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub start_us: u64,
    pub end_us: u64,
}

#[derive(Debug)]
pub struct Recorder {
    origin: Instant,
    wall_origin: u128,
    phases: Vec<Phase>,
    open: Option<(String, u64)>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            wall_origin: wall_nanos(),
            phases: Vec::new(),
            open: None,
        }
    }

    pub fn begin(&mut self, name: &str) {
        self.end();
        self.open = Some((name.to_string(), self.elapsed_us()));
    }

    pub fn end(&mut self) {
        if let Some((name, start_us)) = self.open.take() {
            let end_us = self.elapsed_us();
            self.phases.push(Phase {
                name,
                start_us,
                end_us,
            });
        }
    }

    pub fn absorb(&mut self, prefix: &str, offset_us: u64, phases: Vec<Phase>) {
        for phase in phases {
            self.phases.push(Phase {
                name: format!("{prefix}.{}", phase.name),
                start_us: offset_us + phase.start_us,
                end_us: offset_us + phase.end_us,
            });
        }
    }

    pub fn elapsed_us(&self) -> u64 {
        self.origin.elapsed().as_micros() as u64
    }

    pub fn take(&mut self) -> Vec<Phase> {
        self.end();
        std::mem::take(&mut self.phases)
    }

    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    pub fn wall_origin(&self) -> u128 {
        self.wall_origin
    }
}

pub fn endpoint(explicit: Option<&str>) -> Option<String> {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var(ENDPOINT_ENV).ok())
        .filter(|value| !value.trim().is_empty())
}

pub fn export(
    endpoint: &str,
    root: &str,
    container_id: &str,
    recorder: &mut Recorder,
) -> Result<()> {
    let phases = recorder.take();

    if phases.is_empty() {
        return Ok(());
    }

    let origin = recorder.wall_origin();
    let total = phases.iter().map(|phase| phase.end_us).max().unwrap_or(0);

    let trace_id = random_hex(16)?;
    let root_span = random_hex(8)?;

    let mut spans = String::new();
    write_span(
        &mut spans,
        &trace_id,
        &root_span,
        None,
        root,
        origin,
        0,
        total,
        container_id,
    );

    for phase in &phases {
        let id = random_hex(8)?;
        spans.push(',');
        write_span(
            &mut spans,
            &trace_id,
            &id,
            Some(&root_span),
            &phase.name,
            origin,
            phase.start_us,
            phase.end_us,
            container_id,
        );
    }

    let body = format!(
        r#"{{"resourceSpans":[{{"resource":{{"attributes":[{}]}},"scopeSpans":[{{"scope":{{"name":"{SERVICE}","version":"{}"}},"spans":[{spans}]}}]}}]}}"#,
        resource_attributes(),
        env!("CARGO_PKG_VERSION"),
    );

    post(endpoint, &body)?;

    tracing::debug!(
        endpoint,
        trace_id,
        spans = phases.len() + 1,
        total_us = total,
        "startup trace exported"
    );

    Ok(())
}

fn resource_attributes() -> String {
    let host = nix::unistd::gethostname()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    format!(
        r#"{},{},{}"#,
        attribute("service.name", SERVICE),
        attribute("service.version", env!("CARGO_PKG_VERSION")),
        attribute("host.name", &host),
    )
}

fn attribute(key: &str, value: &str) -> String {
    format!(
        r#"{{"key":"{key}","value":{{"stringValue":"{}"}}}}"#,
        escape(value)
    )
}

#[allow(clippy::too_many_arguments)]
fn write_span(
    out: &mut String,
    trace_id: &str,
    span_id: &str,
    parent: Option<&str>,
    name: &str,
    origin_nanos: u128,
    start_us: u64,
    end_us: u64,
    container_id: &str,
) {
    let start = origin_nanos + u128::from(start_us) * 1_000;
    let end = origin_nanos + u128::from(end_us) * 1_000;

    let parent = match parent {
        Some(id) => format!(r#""parentSpanId":"{id}","#),
        None => String::new(),
    };

    let _ = write!(
        out,
        r#"{{"traceId":"{trace_id}","spanId":"{span_id}",{parent}"name":"{}","kind":1,"startTimeUnixNano":"{start}","endTimeUnixNano":"{end}","attributes":[{}]}}"#,
        escape(name),
        attribute("container.id", container_id),
    );
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn post(endpoint: &str, body: &str) -> Result<()> {
    let (host_port, path) = split_endpoint(endpoint)?;

    let mut stream = TcpStream::connect(&host_port).ctx(format!(
        "connect to the OTLP collector at {host_port}; set {ENDPOINT_ENV} to host:port or a URL"
    ))?;

    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    stream
        .write_all(request.as_bytes())
        .ctx("send the OTLP payload")?;

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let head = String::from_utf8_lossy(&response);
    let status = head.lines().next().unwrap_or_default();

    if status.contains(" 200") || status.contains(" 202") || status.contains(" 204") {
        return Ok(());
    }

    Err(Error::Invalid(format!(
        "the OTLP collector answered {status:?}"
    )))
}

pub fn split_endpoint(endpoint: &str) -> Result<(String, String)> {
    let trimmed = endpoint
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    let (authority, path) = match trimmed.find('/') {
        Some(index) => (&trimmed[..index], &trimmed[index..]),
        None => (trimmed, "/v1/traces"),
    };

    if authority.is_empty() {
        return Err(Error::Invalid(format!(
            "{endpoint:?} has no host; expected something like localhost:4318"
        )));
    }

    let authority = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:4318")
    };

    Ok((authority, path.to_string()))
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];

    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buffer))
        .ctx("read /dev/urandom for a trace id")?;

    let mut out = String::with_capacity(bytes * 2);

    for byte in buffer {
        let _ = write!(out, "{byte:02x}");
    }

    Ok(out)
}

fn wall_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_default_to_the_otlp_http_port_and_path() {
        assert_eq!(
            split_endpoint("localhost").unwrap(),
            ("localhost:4318".into(), "/v1/traces".into())
        );
        assert_eq!(
            split_endpoint("tempo:4318").unwrap(),
            ("tempo:4318".into(), "/v1/traces".into())
        );
        assert_eq!(
            split_endpoint("http://tempo:4318/v1/traces").unwrap(),
            ("tempo:4318".into(), "/v1/traces".into())
        );
        assert_eq!(
            split_endpoint("https://otel.example.com/custom").unwrap(),
            ("otel.example.com:4318".into(), "/custom".into())
        );
    }

    #[test]
    fn an_endpoint_without_a_host_is_rejected() {
        assert!(split_endpoint("http://").is_err());
        assert!(split_endpoint("/v1/traces").is_err());
    }

    #[test]
    fn phases_are_recorded_in_order_and_do_not_overlap() {
        let mut recorder = Recorder::new();

        recorder.begin("first");
        std::thread::sleep(Duration::from_millis(2));
        recorder.begin("second");
        std::thread::sleep(Duration::from_millis(2));
        recorder.end();

        let phases = recorder.take();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].name, "first");
        assert_eq!(phases[1].name, "second");
        assert!(phases[0].end_us <= phases[1].start_us);
        assert!(phases[0].end_us > phases[0].start_us);
    }

    #[test]
    fn absorbed_phases_are_prefixed_and_shifted_onto_the_parents_clock() {
        let mut recorder = Recorder::new();

        recorder.absorb(
            "init",
            1_000,
            vec![Phase {
                name: "pivot_root".into(),
                start_us: 5,
                end_us: 25,
            }],
        );

        let phases = recorder.take();
        assert_eq!(phases[0].name, "init.pivot_root");
        assert_eq!(phases[0].start_us, 1_005);
        assert_eq!(phases[0].end_us, 1_025);
    }

    #[test]
    fn a_trace_id_is_sixteen_bytes_of_hex_and_a_span_id_is_eight() {
        let trace = random_hex(16).unwrap();
        let span = random_hex(8).unwrap();

        assert_eq!(trace.len(), 32);
        assert_eq!(span.len(), 16);
        assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(trace, random_hex(16).unwrap());
    }

    #[test]
    fn attribute_values_are_escaped() {
        let rendered = attribute("k", "a\"b\\c");
        assert!(rendered.contains(r#"a\"b\\c"#), "{rendered}");
    }

    #[test]
    fn the_endpoint_comes_from_the_flag_first_then_the_environment() {
        assert_eq!(endpoint(Some("a:1")).as_deref(), Some("a:1"));
        assert_eq!(endpoint(Some("   ")), None);
        assert_eq!(endpoint(None), std::env::var(ENDPOINT_ENV).ok());
    }
}
