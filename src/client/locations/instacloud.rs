use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub(crate) mod inventory;
pub(crate) mod provisioning;

pub(super) fn cli(args: &[&str], project: Option<&str>) -> Command {
    let mut command = Command::new("insta");
    command
        .args(args)
        .env("INSTA_API_URL", "https://api.instacloud.com")
        .env("INSTA_ENV", "prod")
        .env("INSTA_NO_AUTOUPDATE", "1")
        .env_remove("INSTA_ORG_ID")
        .env_remove("INSTA_BRANCH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(project) = project {
        command.env("INSTA_PROJECT_ID", project);
    } else {
        command.env_remove("INSTA_PROJECT_ID");
    }
    command
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloudTarget {
    pub project: String,
    pub branch: String,
    pub service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
}

impl CloudTarget {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("project ID", &self.project),
            ("branch", &self.branch),
            ("service name", &self.service),
        ] {
            if value.is_empty()
                || value.len() > 256
                || value.starts_with('-')
                || value.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(format!("Instacloud {name} must be nonempty, at most 256 bytes, and contain no whitespace"));
            }
        }
        Ok(())
    }

    fn command(&self, verb: &str) -> Command {
        let mut command = cli(
            &[
                "compute",
                verb,
                &self.service,
                "--branch",
                &self.branch,
                "--json",
            ],
            Some(&self.project),
        );
        command.env("INSTA_BRANCH", &self.branch);
        command
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CloudOperation {
    Status,
    Start,
    Stop,
}

fn read_bounded(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Result<Vec<u8>, String>> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stored = Vec::new();
        let mut chunk = [0; 4096];
        let result = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break Ok(stored),
                Ok(count) => {
                    let keep = count.min(65536usize.saturating_sub(stored.len()));
                    stored.extend_from_slice(&chunk[..keep]);
                }
                Err(error) => break Err(error.to_string()),
            }
        };
        let _ = send.send(result);
    });
    receive
}

fn run(mut command: Command) -> Result<serde_json::Value, String> {
    run_with_timeout(&mut command, Duration::from_secs(30))
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<serde_json::Value, String> {
    let mut child = command.spawn().map_err(|error| {
        format!("Cannot run insta: {error}. Install the Instacloud CLI and run insta login.")
    })?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Missing insta stdout".into());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Missing insta stderr".into());
    };
    let stdout = read_bounded(stdout);
    let stderr = read_bounded(stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Instacloud command timed out or failed ({result:?}); outcome may be unknown. Refresh status before retrying."));
            }
        }
    };
    let stdout = stdout
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "Instacloud stdout did not close")??;
    let stderr = stderr
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "Instacloud stderr did not close")??;
    let json = serde_json::from_slice::<serde_json::Value>(&stdout);
    if let Ok(value) = &json {
        if value.get("status").and_then(|v| v.as_str()) == Some("approval_required") {
            return Err(format!("Instacloud approval required: {value}. Approve in Instacloud, then resume this remote."));
        }
    }
    if !status.success() {
        let message: String = String::from_utf8_lossy(&stderr)
            .chars()
            .filter(|c| !c.is_control() || *c == ' ')
            .take(1024)
            .collect();
        return Err(format!("Instacloud failed: {message}"));
    }
    json.map_err(|_| "Instacloud returned invalid or oversized JSON".into())
}

fn live_state(value: &serde_json::Value) -> Result<&str, String> {
    value
        .get("state")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            "Instacloud did not return a live state; check CLI login or a pending approval".into()
        })
}

pub(super) fn operate(target: &CloudTarget, operation: CloudOperation) -> Result<String, String> {
    inventory::verify_identity(target)?;
    operate_with(
        target,
        operation,
        |verb| run(target.command(verb)),
        || std::thread::sleep(Duration::from_secs(2)),
    )
}

fn operate_with(
    target: &CloudTarget,
    operation: CloudOperation,
    mut command: impl FnMut(&str) -> Result<serde_json::Value, String>,
    mut wait: impl FnMut(),
) -> Result<String, String> {
    target.validate()?;
    let verb = match operation {
        CloudOperation::Status => "status",
        CloudOperation::Start => "start",
        CloudOperation::Stop => "stop",
    };
    let response = command(verb)?;
    let state = live_state(&response)?;
    if matches!(operation, CloudOperation::Status) {
        return Ok(format!(
            "{}: live={}, desired={}",
            target.service,
            state,
            response
                .get("desiredState")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    let expected = if matches!(operation, CloudOperation::Start) {
        "running"
    } else {
        "stopped"
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    // Mutation acceptance is not readiness. Poll GET status; never repeat a mutation.
    loop {
        let current = command("status")?;
        let state = live_state(&current)?;
        // Instacloud can suspend the VM when an explicit stop is accepted.
        // A suspended VM with desired=running is merely sleeping, not stopped.
        let suspended_stop = matches!(operation, CloudOperation::Stop)
            && state == "suspended"
            && current.get("desiredState").and_then(|v| v.as_str()) == Some("stopped");
        if state == expected || suspended_stop {
            return Ok(format!("{}: {expected}", target.service));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} was accepted, but {} is not confirmed {expected}; refresh status",
                verb, target.service
            ));
        }
        wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_start_polls_until_running_without_repeating_the_mutation() {
        let target = CloudTarget {
            project: "project".into(),
            branch: "main".into(),
            service: "worker".into(),
            service_id: None,
        };
        let mut calls = Vec::new();
        let mut states = ["starting", "starting", "running"].into_iter();
        let result = operate_with(
            &target,
            CloudOperation::Start,
            |verb| {
                calls.push(verb.to_owned());
                Ok(serde_json::json!({"state": states.next().unwrap()}))
            },
            || {},
        );
        assert_eq!(result.unwrap(), "worker: running");
        assert_eq!(calls, ["start", "status", "status"]);
    }

    #[test]
    fn status_failure_after_stop_does_not_retry_stop() {
        let target = CloudTarget {
            project: "project".into(),
            branch: "main".into(),
            service: "worker".into(),
            service_id: None,
        };
        let mut calls = Vec::new();
        let result = operate_with(
            &target,
            CloudOperation::Stop,
            |verb| {
                calls.push(verb.to_owned());
                if verb == "stop" {
                    Ok(serde_json::json!({"state":"stopping"}))
                } else {
                    Err("network timeout".into())
                }
            },
            || {},
        );
        assert!(result.is_err());
        assert_eq!(calls, ["stop", "status"]);
    }

    #[test]
    fn stop_waits_for_explicit_stopped_intent_before_accepting_suspended() {
        let target = CloudTarget {
            project: "project".into(),
            branch: "main".into(),
            service: "worker".into(),
            service_id: None,
        };
        let mut calls = Vec::new();
        let mut responses = [
            serde_json::json!({"state":"running", "desiredState":"stopped"}),
            serde_json::json!({"state":"suspended", "desiredState":"running"}),
            serde_json::json!({"state":"suspended", "desiredState":"stopped"}),
        ]
        .into_iter();
        let result = operate_with(
            &target,
            CloudOperation::Stop,
            |verb| {
                calls.push(verb.to_owned());
                Ok(responses.next().expect("bounded status polling"))
            },
            || {},
        );
        assert_eq!(result.unwrap(), "worker: stopped");
        assert_eq!(calls, ["stop", "status", "status"]);
    }

    #[test]
    fn provider_scope_does_not_depend_on_a_linked_working_directory() {
        let target = CloudTarget {
            project: "project-id".into(),
            branch: "main".into(),
            service: "worker".into(),
            service_id: None,
        };
        let command = target.command("stop");
        let args = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            ["compute", "stop", "worker", "--branch", "main", "--json"]
        );
        let env = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(env["INSTA_PROJECT_ID"].as_deref(), Some("project-id"));
        assert_eq!(
            env["INSTA_API_URL"].as_deref(),
            Some("https://api.instacloud.com")
        );
    }

    #[test]
    fn approval_or_desired_state_alone_is_not_readiness() {
        assert!(live_state(&serde_json::json!({"approvalRequired":true})).is_err());
        assert!(live_state(&serde_json::json!({"desiredState":"running"})).is_err());
        assert_eq!(
            live_state(&serde_json::json!({"state":"starting"})).unwrap(),
            "starting"
        );
    }
}
