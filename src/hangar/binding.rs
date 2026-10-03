//! A saved remote bound to a hangar machine, and its SSH preparation.
//!
//! The saved SSH profile's target is the alias `hangar-<machineId>`. Before herdr starts
//! `ssh` for such a target it checks that the machine is running (it never starts one),
//! makes sure a certificate is available, and writes the `Host` block into herdr's
//! temporary SSH config. Other targets are untouched.
use std::io;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::api::{Client, HangarError, MachineState};
use super::certs::{self, SshPaths};

const ALIAS_PREFIX: &str = "hangar-";

/// Stored in `client/locations.json` next to the profile it belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct HangarBinding {
    pub server: String,
    pub machine_id: String,
    pub machine_name: String,
    pub alias: String,
}

impl HangarBinding {
    pub(crate) fn new(server: &str, machine_id: &str, machine_name: &str) -> Result<Self, String> {
        let binding = Self {
            server: super::normalize_server(server),
            machine_id: machine_id.to_owned(),
            machine_name: machine_name.to_owned(),
            alias: alias_for(machine_id),
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let server_ok = (self.server.starts_with("https://") || self.server.starts_with("http://"))
            && self.server.len() <= 256
            && !self
                .server
                .chars()
                .any(|character| character.is_control() || character.is_whitespace());
        if !server_ok {
            return Err("hangar server must be an http(s) URL without spaces".into());
        }
        if !is_machine_id(&self.machine_id) || self.alias != alias_for(&self.machine_id) {
            return Err("invalid hangar machine binding".into());
        }
        if self.machine_name.len() > 128 || self.machine_name.chars().any(char::is_control) {
            return Err("hangar machine name must be at most 128 bytes".into());
        }
        Ok(())
    }
}

pub(crate) fn alias_for(machine_id: &str) -> String {
    format!("{ALIAS_PREFIX}{machine_id}")
}

/// hangar machine IDs are `m_` plus 26 lowercase base32 characters.
pub(crate) fn is_machine_id(id: &str) -> bool {
    id.strip_prefix("m_").is_some_and(|body| {
        body.len() == 26
            && body
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
    })
}

/// The machine behind a `hangar-<machineId>` alias. Aliases made by `hangar ssh-config`
/// (`hangar-<name>`) are ordinary SSH targets and stay that way.
pub(crate) fn machine_id_for_target(target: &str) -> Option<&str> {
    target
        .strip_prefix(ALIAS_PREFIX)
        .filter(|id| is_machine_id(id))
}

pub(crate) fn is_hangar_target(target: &str) -> bool {
    machine_id_for_target(target).is_some()
}

/// The saved binding for `target`, or one on the default server for an alias that
/// was typed directly (`herdr --remote hangar-m_…`).
fn binding_for_target(target: &str, machine_id: &str) -> io::Result<HangarBinding> {
    if let Some(binding) =
        crate::client::locations::hangar_binding_for_target(target).map_err(io::Error::other)?
    {
        return Ok(binding);
    }
    HangarBinding::new(&super::default_server(), machine_id, machine_id)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// Checks the machine is running, ensures a certificate, and returns the `Host` block.
pub(crate) fn prepare_with(
    binding: &HangarBinding,
    client: &Client,
    paths: &SshPaths,
    now: SystemTime,
) -> Result<String, HangarError> {
    let machine = client.machine(&binding.machine_id)?;
    if machine.state != MachineState::Running {
        return Err(HangarError::MachineNotRunning {
            machine: if binding.machine_name.is_empty() {
                machine.name
            } else {
                binding.machine_name.clone()
            },
            state: machine.state.as_str().to_owned(),
        });
    }
    let meta = certs::ensure_cert(paths, client, &binding.machine_id, now)?;
    certs::host_block(paths, &binding.alias, &meta)
}

/// `Some(Host block)` for a hangar target, `None` for any other target.
pub(crate) fn prepare_ssh_target(target: &str) -> io::Result<Option<String>> {
    let Some(machine_id) = machine_id_for_target(target) else {
        return Ok(None);
    };
    let binding = binding_for_target(target, machine_id)?;
    let client = super::auth::shared_client(&binding.server).map_err(HangarError::into_io)?;
    prepare_with(&binding, &client, &SshPaths::herdr(), SystemTime::now())
        .map(Some)
        .map_err(HangarError::into_io)
}

/// Run before every new `ssh` process for a hangar target whose `Host` block was
/// already written: renews the certificate file in place when it is about to expire.
/// A still-valid certificate costs two small file reads and no request.
pub(crate) fn refresh_certificate(target: &str) -> io::Result<()> {
    let Some(machine_id) = machine_id_for_target(target) else {
        return Ok(());
    };
    let binding = binding_for_target(target, machine_id)?;
    if certs::cached(
        &SshPaths::herdr(),
        &binding.server,
        machine_id,
        SystemTime::now(),
    )
    .is_some()
    {
        return Ok(());
    }
    prepare_ssh_target(target).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::super::api::fake::*;
    use super::super::certs::tests::{connection, paths_with_key};
    use super::*;

    const ID: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";

    fn binding() -> HangarBinding {
        HangarBinding::new("https://hangar.test", ID, "box").unwrap()
    }

    #[test]
    fn only_machine_id_aliases_are_hangar_targets() {
        assert_eq!(machine_id_for_target(&alias_for(ID)), Some(ID));
        for target in [
            "hangar-e2e1",
            "hangar-m_short",
            "workbox",
            "hangar-m_AGQP6JAAA6KQKITOG6ZZQZDFHY",
        ] {
            assert!(!is_hangar_target(target), "{target}");
        }
        assert!(HangarBinding::new("https://h", "m_bad", "x").is_err());
        let mut edited = binding();
        edited.alias = "other".into();
        assert!(edited.validate().is_err());
        assert!(HangarBinding::new("https://h x", ID, "x").is_err());
    }

    #[test]
    fn stopped_machine_fails_without_starting_it_or_issuing_a_certificate() {
        let paths = paths_with_key("stopped");
        let http = FakeHttp::new();
        http.reply(200, machine(ID, "stopped", false));
        let error =
            prepare_with(&binding(), &client(&http), &paths, SystemTime::now()).unwrap_err();
        assert!(error
            .to_string()
            .contains("box is stopped; use Start remote"));
        assert_eq!(error.into_io().kind(), io::ErrorKind::NotConnected);
        assert_eq!(http.paths(), [format!("GET /v1/machines/{ID}")]);
    }

    #[test]
    fn suspended_machine_is_never_resumed_by_a_connection() {
        let paths = paths_with_key("suspended");
        let http = FakeHttp::new();
        http.reply(200, machine(ID, "suspended", false));
        let error =
            prepare_with(&binding(), &client(&http), &paths, SystemTime::now()).unwrap_err();
        assert!(error.to_string().contains("use Resume remote"), "{error}");
        assert_eq!(error.into_io().kind(), io::ErrorKind::NotConnected);
        assert_eq!(http.paths(), [format!("GET /v1/machines/{ID}")]);
    }

    #[test]
    fn running_machine_yields_a_host_block_for_its_alias() {
        let paths = paths_with_key("running");
        let http = FakeHttp::new();
        http.reply(200, machine(ID, "running", true))
            .reply(201, connection("2099-01-01T00:00:00Z"));
        let block = prepare_with(&binding(), &client(&http), &paths, SystemTime::now()).unwrap();
        assert!(block.starts_with(&format!("Host hangar-{ID}\n")));
        assert_eq!(
            http.paths(),
            [
                format!("GET /v1/machines/{ID}"),
                format!("POST /v1/machines/{ID}/connections")
            ]
        );
    }

    #[test]
    fn deleted_machine_needs_attention() {
        let paths = paths_with_key("deleted");
        let http = FakeHttp::new();
        http.error(404, "not_found");
        let error =
            prepare_with(&binding(), &client(&http), &paths, SystemTime::now()).unwrap_err();
        assert_eq!(error.into_io().kind(), io::ErrorKind::NotFound);
    }
}
