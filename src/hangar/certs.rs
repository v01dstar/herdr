//! herdr's own SSH key and short-lived hangar certificates.
//!
//! The key lives in `state_dir/client/hangar/ssh/` and is generated once with
//! `ssh-keygen` (no crypto dependency). Each machine gets a certificate from
//! `POST /v1/machines/{id}/connections`; one with more than 15 minutes left is reused.
//! The gateway checks validity only at the SSH handshake, so live connections and
//! multiplexed sessions are unaffected when a certificate expires.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::api::{Client, HangarError, HostTrust};

pub(crate) const CERT_TTL_SECONDS: u64 = 4 * 60 * 60;
pub(crate) const CERT_MIN_REMAINING: Duration = Duration::from_secs(15 * 60);
const KEY_NAME: &str = "id_ed25519";

#[derive(Clone, Debug)]
pub(crate) struct SshPaths {
    dir: PathBuf,
}

impl SshPaths {
    pub(crate) fn herdr() -> Self {
        Self::at(
            crate::config::state_dir()
                .join("client")
                .join("hangar")
                .join("ssh"),
        )
    }

    pub(crate) fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn key(&self) -> PathBuf {
        self.dir.join(KEY_NAME)
    }

    fn public_key(&self) -> PathBuf {
        self.dir.join(format!("{KEY_NAME}.pub"))
    }

    pub(crate) fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    pub(crate) fn cert(&self, machine_id: &str) -> PathBuf {
        self.dir.join(format!("{machine_id}-cert.pub"))
    }

    fn meta(&self, machine_id: &str) -> PathBuf {
        self.dir.join(format!("{machine_id}-cert.json"))
    }

    fn lock(&self) -> PathBuf {
        self.dir.join(".lock")
    }
}

/// What a certificate was issued for, so it can be reused without asking the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CertMeta {
    pub connection_id: String,
    pub machine_id: String,
    pub server: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub expires_at: String,
    pub public_key: String,
    pub host_trust: HostTrust,
}

fn invalid(context: &str, error: impl std::fmt::Display) -> HangarError {
    HangarError::Invalid(format!("{context}: {error}"))
}

/// `type base64` of an authorized_keys line, without the comment.
fn key_body(line: &str) -> String {
    let mut fields = line.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(kind), Some(key)) => format!("{kind} {key}"),
        _ => line.trim().to_owned(),
    }
}

fn store(path: &Path, content: &str, description: &str) -> Result<(), HangarError> {
    crate::client::endpoint::store_private_json(path, content.as_bytes(), description)
        .map_err(HangarError::Invalid)
}

/// Creates herdr's hangar key on first use and returns its public half.
pub(crate) fn ensure_key(paths: &SshPaths) -> Result<String, HangarError> {
    let key = paths.key();
    let public = paths.public_key();
    if key.is_file() {
        if let Ok(line) = std::fs::read_to_string(&public) {
            if !line.trim().is_empty() {
                return Ok(line.trim().to_owned());
            }
        }
    }
    super::auth::ensure_private_dir(&paths.dir)
        .map_err(|error| invalid("hangar SSH directory", error))?;
    // A half-written pair from an interrupted run would make ssh-keygen prompt.
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&public);
    let output = crate::noninteractive_process::command("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "herdr-hangar", "-f"])
        .arg(&key)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| invalid("cannot run ssh-keygen (install OpenSSH)", error))?;
    if !output.status.success() {
        return Err(invalid(
            "ssh-keygen failed",
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let line = std::fs::read_to_string(&public).map_err(|error| invalid("read SSH key", error))?;
    Ok(line.trim().to_owned())
}

fn parse_time(text: &str) -> Option<SystemTime> {
    let at =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    let nanos = u64::try_from(at.unix_timestamp_nanos()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos))
}

fn still_valid(meta: &CertMeta, now: SystemTime) -> bool {
    parse_time(&meta.expires_at)
        .and_then(|at| at.duration_since(now).ok())
        .is_some_and(|left| left > CERT_MIN_REMAINING)
}

/// A stored certificate for `machine_id` issued to the current key by `server` with
/// more than 15 minutes left. Reads files only.
pub(crate) fn cached(
    paths: &SshPaths,
    server: &str,
    machine_id: &str,
    now: SystemTime,
) -> Option<CertMeta> {
    let public = std::fs::read_to_string(paths.public_key()).ok()?;
    let meta: CertMeta =
        serde_json::from_slice(&std::fs::read(paths.meta(machine_id)).ok()?).ok()?;
    (paths.cert(machine_id).is_file()
        && paths.key().is_file()
        && meta.machine_id == machine_id
        && meta.server == server
        && key_body(&meta.public_key) == key_body(&public)
        && still_valid(&meta, now))
    .then_some(meta)
}

fn safe_token(value: &str, extra: &[char]) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || extra.contains(&character))
}

/// Everything from the server that lands in an SSH config or known_hosts line.
fn validate_connection(meta: &CertMeta, certificate: &str) -> Result<(), HangarError> {
    let single_line = |value: &str| {
        !value.trim().is_empty() && !value.trim().chars().any(|character| character.is_control())
    };
    if !safe_token(&meta.host, &['.', '-', ':', '[', ']'])
        || !safe_token(&meta.username, &['_', '-', '.'])
        || !single_line(certificate)
        || !single_line(&meta.host_trust.public_key)
        || key_body(&meta.host_trust.public_key).split(' ').count() != 2
    {
        return Err(HangarError::Invalid(
            "server returned an incomplete or malformed SSH connection".into(),
        ));
    }
    Ok(())
}

fn known_hosts_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Writes the gateway's host CA for `host:port`, replacing an earlier line for it.
fn update_known_hosts(paths: &SshPaths, meta: &CertMeta) -> Result<(), HangarError> {
    let pattern = known_hosts_pattern(&meta.host, meta.port);
    let wanted = format!(
        "@cert-authority {pattern} {}",
        key_body(&meta.host_trust.public_key)
    );
    let current = std::fs::read_to_string(paths.known_hosts()).unwrap_or_default();
    if current.lines().any(|line| line.trim() == wanted) {
        return Ok(());
    }
    let mut lines: Vec<&str> = current
        .lines()
        .map(str::trim)
        .filter(|line| {
            let mut fields = line.split_whitespace();
            let replaced =
                fields.next() == Some("@cert-authority") && fields.next() == Some(pattern.as_str());
            !line.is_empty() && !replaced
        })
        .collect();
    lines.push(&wanted);
    store(
        &paths.known_hosts(),
        &(lines.join("\n") + "\n"),
        "hangar known hosts",
    )
}

/// Returns a usable certificate for `machine_id`, requesting a new one unless the
/// stored one has more than 15 minutes left.
pub(crate) fn ensure_cert(
    paths: &SshPaths,
    client: &Client,
    machine_id: &str,
    now: SystemTime,
) -> Result<CertMeta, HangarError> {
    let _lock = super::auth::lock_file(&paths.lock())
        .map_err(|error| invalid("lock hangar SSH directory", error))?;
    let public = ensure_key(paths)?;
    if let Some(meta) = cached(paths, client.server(), machine_id, now) {
        update_known_hosts(paths, &meta)?;
        return Ok(meta);
    }
    let connection = client.create_connection(
        &super::new_idempotency_key(),
        machine_id,
        &key_body(&public),
        CERT_TTL_SECONDS,
    )?;
    let meta = CertMeta {
        connection_id: connection.id,
        machine_id: machine_id.to_owned(),
        server: client.server().to_owned(),
        host: connection.host,
        port: if connection.port == 0 {
            22
        } else {
            connection.port
        },
        username: if connection.username.is_empty() {
            machine_id.to_owned()
        } else {
            connection.username
        },
        expires_at: connection.expires_at,
        public_key: key_body(&public),
        host_trust: connection.host_trust,
    };
    validate_connection(&meta, &connection.certificate)?;
    store(
        &paths.cert(machine_id),
        &format!("{}\n", connection.certificate.trim()),
        "hangar SSH certificate",
    )?;
    let encoded = serde_json::to_string_pretty(&meta).map_err(|error| invalid("encode", error))?;
    store(
        &paths.meta(machine_id),
        &encoded,
        "hangar certificate record",
    )?;
    update_known_hosts(paths, &meta)?;
    Ok(meta)
}

/// Drops the certificate and its record for a deleted machine. The shared key and
/// `known_hosts` (the gateway's host CA) stay for other machines.
pub(crate) fn forget_machine(paths: &SshPaths, machine_id: &str) -> std::io::Result<()> {
    if !std::fs::exists(&paths.dir)? {
        return Ok(());
    }
    let _lock = super::auth::lock_file(&paths.lock())?;
    for path in [paths.cert(machine_id), paths.meta(machine_id)] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn config_path(path: &Path) -> Result<String, HangarError> {
    let text = path.to_string_lossy();
    if text.contains('"') || text.chars().any(char::is_control) {
        return Err(HangarError::Invalid(format!(
            "unsupported path for SSH config: {}",
            path.display()
        )));
    }
    // OpenSSH on Windows (and MSYS) accepts forward slashes in config paths.
    let text = if std::path::MAIN_SEPARATOR == '\\' {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    };
    Ok(format!("\"{text}\""))
}

/// The `Host` block that makes `alias` reach the machine through the gateway. It goes
/// first in herdr's temporary SSH config, so OpenSSH's first-value-wins rule keeps it
/// ahead of anything in the user's own configuration.
pub(crate) fn host_block(
    paths: &SshPaths,
    alias: &str,
    meta: &CertMeta,
) -> Result<String, HangarError> {
    Ok(format!(
        "Host {alias}\n  HostName {host}\n  Port {port}\n  User {user}\n  IdentityFile {key}\n  CertificateFile {cert}\n  IdentitiesOnly yes\n  UserKnownHostsFile {known_hosts}\n  StrictHostKeyChecking yes\n  UpdateHostKeys no\n",
        host = meta.host.trim_start_matches('[').trim_end_matches(']'),
        port = meta.port,
        user = meta.username,
        key = config_path(&paths.key())?,
        cert = config_path(&paths.cert(&meta.machine_id))?,
        known_hosts = config_path(&paths.known_hosts())?,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::api::fake::*;
    use super::*;

    pub(crate) fn paths_with_key(name: &str) -> SshPaths {
        let paths = SshPaths::at(super::super::auth::tests::temp_dir(name));
        std::fs::write(paths.key(), "private").unwrap();
        std::fs::write(
            paths.public_key(),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample herdr-hangar\n",
        )
        .unwrap();
        paths
    }

    pub(crate) fn connection(expires_at: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "cn_1", "machineId": "m_x", "transport": "ssh", "host": "152.236.1.51",
            "port": 2222, "username": "m_x", "certificate": "ssh-ed25519-cert-v01@openssh.com AAAAcert",
            "expiresAt": expires_at,
            "hostTrust": {"type": "ca", "publicKey": "ssh-ed25519 AAAAhostca hangar-host-ca", "principals": ["152.236.1.51"]}
        })
    }

    fn at(text: &str) -> SystemTime {
        parse_time(text).unwrap()
    }

    #[test]
    fn certificate_is_reused_until_fifteen_minutes_remain() {
        let paths = paths_with_key("reuse");
        let http = FakeHttp::new();
        http.reply(201, connection("2026-10-02T16:00:00Z"));
        let client = client(&http);
        let first = ensure_cert(&paths, &client, "m_x", at("2026-10-02T12:00:00Z")).unwrap();
        assert_eq!(first.port, 2222);
        let request = &http.sent()[0];
        assert!(request.idempotency_key.is_some());
        let body: serde_json::Value =
            serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["ttlSeconds"], CERT_TTL_SECONDS);
        assert_eq!(
            body["publicKey"],
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample"
        );
        let reused = ensure_cert(&paths, &client, "m_x", at("2026-10-02T15:44:00Z")).unwrap();
        assert_eq!(reused, first);
        assert_eq!(http.sent().len(), 1);
        http.reply(201, connection("2026-10-02T19:50:00Z"));
        let renewed = ensure_cert(&paths, &client, "m_x", at("2026-10-02T15:46:00Z")).unwrap();
        assert_eq!(renewed.expires_at, "2026-10-02T19:50:00Z");
        assert_eq!(http.sent().len(), 2);
        assert!(std::fs::read_to_string(paths.cert("m_x"))
            .unwrap()
            .starts_with("ssh-ed25519-cert-v01@openssh.com"));
        let known = std::fs::read_to_string(paths.known_hosts()).unwrap();
        assert_eq!(
            known,
            "@cert-authority [152.236.1.51]:2222 ssh-ed25519 AAAAhostca\n"
        );
        // A certificate from another server is never reused.
        assert!(cached(&paths, "https://other", "m_x", at("2026-10-02T16:00:00Z")).is_none());
    }

    #[test]
    fn forgetting_a_machine_keeps_the_shared_key_and_other_certificates() {
        let paths = paths_with_key("forget");
        std::fs::write(paths.known_hosts(), "@cert-authority gateway\n").unwrap();
        for id in ["m_gone", "m_kept"] {
            std::fs::write(paths.cert(id), "cert").unwrap();
            std::fs::write(paths.meta(id), "{}").unwrap();
        }
        forget_machine(&paths, "m_gone").unwrap();
        assert!(!paths.cert("m_gone").exists());
        assert!(!paths.meta("m_gone").exists());
        assert!(paths.cert("m_kept").exists());
        assert!(paths.meta("m_kept").exists());
        assert!(paths.key().exists());
        assert!(paths.public_key().exists());
        assert!(paths.known_hosts().exists());
        forget_machine(&paths, "m_gone").unwrap();
        forget_machine(&SshPaths::at(paths.dir.join("missing")), "m_gone").unwrap();
    }

    #[test]
    fn malformed_connection_is_rejected_before_it_reaches_ssh_config() {
        let paths = paths_with_key("malformed");
        let http = FakeHttp::new();
        let mut bad = connection("2026-10-02T16:00:00Z");
        bad["host"] = serde_json::json!("evil\n  ProxyCommand sh");
        http.reply(201, bad);
        assert!(ensure_cert(&paths, &client(&http), "m_x", at("2026-10-02T12:00:00Z")).is_err());
        assert!(!paths.cert("m_x").exists());
    }

    #[test]
    fn host_block_pins_key_certificate_and_host_ca() {
        let paths = paths_with_key("block");
        let http = FakeHttp::new();
        http.reply(201, connection("2026-10-02T16:00:00Z"));
        let meta = ensure_cert(&paths, &client(&http), "m_x", at("2026-10-02T12:00:00Z")).unwrap();
        let block = host_block(&paths, "hangar-m_x", &meta).unwrap();
        assert!(block
            .starts_with("Host hangar-m_x\n  HostName 152.236.1.51\n  Port 2222\n  User m_x\n"));
        for line in [
            "IdentitiesOnly yes",
            "StrictHostKeyChecking yes",
            "UpdateHostKeys no",
            "CertificateFile \"",
            "UserKnownHostsFile \"",
        ] {
            assert!(block.contains(line), "{line}");
        }
    }
}
