use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oci_spec::runtime::{Hook, Hooks};

use crate::error::{Error, IoContext, Result};
use crate::state::OciState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prestart,
    CreateRuntime,
    CreateContainer,
    StartContainer,
    Poststart,
    Poststop,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Prestart => "prestart",
            Phase::CreateRuntime => "createRuntime",
            Phase::CreateContainer => "createContainer",
            Phase::StartContainer => "startContainer",
            Phase::Poststart => "poststart",
            Phase::Poststop => "poststop",
        }
    }

    pub fn is_fatal(self) -> bool {
        !matches!(self, Phase::Poststart | Phase::Poststop)
    }

    fn select(self, hooks: &Hooks) -> Option<&Vec<Hook>> {
        #[allow(deprecated)]
        match self {
            Phase::Prestart => hooks.prestart().as_ref(),
            Phase::CreateRuntime => hooks.create_runtime().as_ref(),
            Phase::CreateContainer => hooks.create_container().as_ref(),
            Phase::StartContainer => hooks.start_container().as_ref(),
            Phase::Poststart => hooks.poststart().as_ref(),
            Phase::Poststop => hooks.poststop().as_ref(),
        }
    }
}

pub fn run(hooks: Option<&Hooks>, phase: Phase, state: &OciState) -> Result<()> {
    let Some(list) = hooks.and_then(|hooks| phase.select(hooks)) else {
        return Ok(());
    };

    if list.is_empty() {
        return Ok(());
    }

    let payload = serde_json::to_vec(state)?;

    for (index, hook) in list.iter().enumerate() {
        let outcome = one(hook, &payload).map_err(|error| {
            Error::Invalid(format!(
                "{} hook {index} ({}) failed: {error}",
                phase.name(),
                hook.path().display()
            ))
        });

        match outcome {
            Ok(()) => {}
            Err(error) if phase.is_fatal() => return Err(error),
            Err(error) => tracing::warn!(
                phase = phase.name(),
                "{error}; the spec says {} hook failures must not abort the operation",
                phase.name()
            ),
        }
    }

    Ok(())
}

fn one(hook: &Hook, payload: &[u8]) -> Result<()> {
    let args = hook.args().clone().unwrap_or_default();
    let argv0 = args
        .first()
        .cloned()
        .unwrap_or_else(|| hook.path().display().to_string());

    let mut command = Command::new(hook.path());
    command.arg0(argv0);
    command.args(args.iter().skip(1));
    command.env_clear();

    for entry in hook.env().iter().flatten() {
        if let Some((key, value)) = entry.split_once('=') {
            command.env(key, value);
        }
    }

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ctx(format!("spawn hook {}", hook.path().display()))?;

    child
        .stdin
        .take()
        .ok_or_else(|| Error::Invalid("hook stdin was not captured".to_string()))?
        .write_all(payload)
        .ctx("write the container state to the hook's stdin")?;

    let deadline = hook
        .timeout()
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Instant::now() + Duration::from_secs(seconds as u64));

    let status = match deadline {
        None => child.wait().ctx("wait for the hook")?,
        Some(deadline) => loop {
            match child.try_wait().ctx("poll the hook")? {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Invalid(format!(
                        "timed out after {}s",
                        hook.timeout().unwrap_or_default()
                    )));
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        },
    };

    if status.success() {
        return Ok(());
    }

    Err(Error::Invalid(format!("exited with {status}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_spec::runtime::{HookBuilder, HooksBuilder};
    use std::path::PathBuf;

    fn state() -> OciState {
        OciState {
            version: "1.0.2".into(),
            id: "hooktest".into(),
            status: crate::state::Status::Created,
            pid: Some(4242),
            bundle: PathBuf::from("/bundle"),
            annotations: None,
        }
    }

    #[test]
    fn a_hook_that_succeeds_is_not_an_error() {
        let hooks = HooksBuilder::default()
            .create_runtime(vec![
                HookBuilder::default()
                    .path(PathBuf::from("/bin/true"))
                    .build()
                    .unwrap(),
            ])
            .build()
            .unwrap();

        run(Some(&hooks), Phase::CreateRuntime, &state()).unwrap();
    }

    #[test]
    fn a_failing_create_runtime_hook_aborts_the_operation() {
        let hooks = HooksBuilder::default()
            .create_runtime(vec![
                HookBuilder::default()
                    .path(PathBuf::from("/bin/false"))
                    .build()
                    .unwrap(),
            ])
            .build()
            .unwrap();

        let error = run(Some(&hooks), Phase::CreateRuntime, &state()).unwrap_err();
        assert!(
            error.to_string().contains("createRuntime hook 0"),
            "{error}"
        );
    }

    #[test]
    fn a_failing_poststop_hook_is_logged_and_tolerated() {
        let hooks = HooksBuilder::default()
            .poststop(vec![
                HookBuilder::default()
                    .path(PathBuf::from("/bin/false"))
                    .build()
                    .unwrap(),
            ])
            .build()
            .unwrap();

        run(Some(&hooks), Phase::Poststop, &state()).unwrap();
    }

    #[test]
    fn the_container_state_reaches_the_hook_on_stdin() {
        let hooks = HooksBuilder::default()
            .create_runtime(vec![
                HookBuilder::default()
                    .path(PathBuf::from("/bin/sh"))
                    .args(vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "grep -q '\"id\":\"hooktest\"'".to_string(),
                    ])
                    .build()
                    .unwrap(),
            ])
            .build()
            .unwrap();

        run(Some(&hooks), Phase::CreateRuntime, &state()).unwrap();
    }

    #[test]
    fn a_hook_that_hangs_is_killed_at_its_timeout() {
        let hooks = HooksBuilder::default()
            .create_runtime(vec![
                HookBuilder::default()
                    .path(PathBuf::from("/bin/sleep"))
                    .args(vec!["sleep".to_string(), "30".to_string()])
                    .timeout(1)
                    .build()
                    .unwrap(),
            ])
            .build()
            .unwrap();

        let started = Instant::now();
        let error = run(Some(&hooks), Phase::CreateRuntime, &state()).unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn no_hooks_configured_is_not_an_error() {
        run(None, Phase::Poststart, &state()).unwrap();
    }
}
