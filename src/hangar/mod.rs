//! hangar control-plane client for herdr remotes.
//!
//! hangar machines are reached through its SSH gateway with short-lived certificates.
//! This module owns the HTTP API types, the sign-in shared with the `hangar` CLI
//! (`~/.config/hangar/credentials.json`), herdr's own SSH key and certificates, and the
//! translation of a saved hangar binding into an OpenSSH `Host` block. Everything here
//! blocks on network or subprocess I/O and must run on worker threads, never in render.

pub(crate) mod api;
pub(crate) mod auth;
pub(crate) mod binding;
pub(crate) mod certs;

/// Used when neither `HANGAR_SERVER` nor stored credentials name a server.
pub(crate) const DEFAULT_SERVER: &str = "https://152.236.1.51";
pub(crate) const SERVER_ENV: &str = "HANGAR_SERVER";

/// Same normalization as the hangar CLI: trim, drop trailing slashes, default to https.
pub(crate) fn normalize_server(server: &str) -> String {
    let server = server.trim().trim_end_matches('/');
    if server.contains("://") {
        server.to_owned()
    } else {
        format!("https://{server}")
    }
}

/// Server for a new binding: `HANGAR_SERVER`, then the signed-in server, then the default.
/// Existing bindings keep the server they were created with.
pub(crate) fn default_server() -> String {
    if let Some(server) = std::env::var(SERVER_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return normalize_server(&server);
    }
    auth::CredentialStore::shared()
        .and_then(|store| store.load().ok().flatten())
        .map(|credentials| normalize_server(&credentials.server))
        .filter(|server| !server.is_empty() && server != "https://")
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned())
}

/// A unique key for one logical mutation. Retries of that mutation reuse it.
pub(crate) fn new_idempotency_key() -> String {
    format!("herdr-{}", crate::client::endpoint::ProfileId::generate())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_normalization_matches_the_hangar_cli() {
        assert_eq!(normalize_server(" 152.236.1.51/ "), "https://152.236.1.51");
        assert_eq!(
            normalize_server("http://localhost:8080//"),
            "http://localhost:8080"
        );
        assert_ne!(new_idempotency_key(), new_idempotency_key());
    }
}
