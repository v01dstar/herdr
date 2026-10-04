//! hangar HTTP API: types mirroring `pkg/hangarapi` and a small blocking client.
//!
//! Herdr already shells out to `curl` for HTTPS (updates, the agent-detection catalog),
//! so the transport reuses that instead of linking an HTTP and TLS stack. Requests are
//! passed to curl through a config on stdin so tokens never appear in process arguments.
use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT_SECONDS: u64 = 10;
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const ATTEMPTS: usize = 3;
const MAX_MACHINE_PAGES: usize = 20;

// ---------------------------------------------------------------------------
// Types. Unknown enum values from a newer server decode to `Unknown`.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    Unauthenticated,
    PermissionDenied,
    NotFound,
    BadRequest,
    OperationConflict,
    IdempotencyConflict,
    MachineNotRunning,
    QuotaExceeded,
    NoCapacity,
    HostUnavailable,
    Internal,
    AuthorizationPending,
    SlowDown,
    ExpiredToken,
    AccessDenied,
    /// The loopback sign-in code expired, was reused or did not match.
    InvalidGrant,
    Unknown(String),
}

impl ErrorCode {
    pub(crate) fn parse(code: &str) -> Self {
        match code {
            "unauthenticated" => Self::Unauthenticated,
            "permission_denied" => Self::PermissionDenied,
            "not_found" => Self::NotFound,
            "bad_request" => Self::BadRequest,
            "operation_conflict" => Self::OperationConflict,
            "idempotency_conflict" => Self::IdempotencyConflict,
            "machine_not_running" => Self::MachineNotRunning,
            "quota_exceeded" => Self::QuotaExceeded,
            "no_capacity" => Self::NoCapacity,
            "host_unavailable" => Self::HostUnavailable,
            "internal" => Self::Internal,
            "authorization_pending" => Self::AuthorizationPending,
            "slow_down" => Self::SlowDown,
            "expired_token" => Self::ExpiredToken,
            "access_denied" => Self::AccessDenied,
            "invalid_grant" => Self::InvalidGrant,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorDetail {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub operation_id: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

/// A structured non-2xx API response.
#[derive(Clone, Debug)]
pub(crate) struct ApiError {
    pub status: u16,
    pub code: ErrorCode,
    pub message: String,
    pub operation_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MachineState {
    Creating,
    Starting,
    Running,
    Stopping,
    Stopped,
    Suspending,
    Suspended,
    Resuming,
    Deleting,
    Deleted,
    Error,
    #[serde(other)]
    Unknown,
}

impl MachineState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Suspending => "suspending",
            Self::Suspended => "suspended",
            Self::Resuming => "resuming",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OperationState {
    Queued,
    Running,
    Succeeded,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Runtime {
    #[serde(default)]
    pub ready: bool,
}

/// The template version a machine or image was made from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateRef {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version: String,
}

impl TemplateRef {
    pub(crate) fn label(&self) -> String {
        match (self.id.is_empty(), self.version.is_empty()) {
            (true, _) => "unknown template".into(),
            (false, true) => self.id.clone(),
            (false, false) => format!("{}@{}", self.id, self.version),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageRef {
    #[serde(default)]
    pub id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkRef {
    #[serde(default)]
    pub machine_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Storage {
    /// False while the newest state of a stopped or suspended machine exists only on
    /// its host; images need it uploaded.
    #[serde(default)]
    pub synced: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Machine {
    pub id: String,
    pub name: String,
    pub state: MachineState,
    #[serde(default)]
    pub runtime: Runtime,
    #[serde(default)]
    pub last_error: Option<ErrorDetail>,
    /// Absent on servers that predate it.
    #[serde(default)]
    pub template: Option<TemplateRef>,
    #[serde(default)]
    pub storage: Option<Storage>,
    /// The image the machine was created from.
    #[serde(default)]
    pub image: Option<ImageRef>,
    /// The machine a fork was copied from.
    #[serde(default)]
    pub forked_from: Option<ForkRef>,
    /// Absent on servers that predate it.
    #[serde(default)]
    pub spec: Option<MachineSpec>,
}

/// A machine's size.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MachineSpec {
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default, rename = "memMiB")]
    pub mem_mib: u64,
    #[serde(default, rename = "persistentDiskGiB")]
    pub persistent_disk_gib: u64,
    /// Absent: the template's root disk size.
    #[serde(default, rename = "rootDiskGiB")]
    pub root_disk_gib: Option<u64>,
}

/// Guest capabilities of a template version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TemplateCapability {
    /// Images and forks of its machines are allowed.
    IdentityReset,
    RootGrow,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Template {
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<TemplateCapability>,
}

impl Template {
    pub(crate) fn has(&self, capability: TemplateCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

#[derive(Deserialize)]
struct TemplateList {
    #[serde(default)]
    templates: Vec<Template>,
}

/// A private image: the root disk of a stopped machine.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Image {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source_machine_id: String,
    #[serde(default)]
    pub template: TemplateRef,
    #[serde(default)]
    pub root_size_bytes: u64,
    /// Stored bytes only this image references; absent before a usage run counted it.
    #[serde(default)]
    pub exclusive_bytes: Option<u64>,
    /// RFC 3339.
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize)]
struct ImageList {
    #[serde(default)]
    images: Vec<Image>,
}

/// Storage usage and limits of the caller (`GET /v1/usage`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Usage {
    /// RFC 3339 time of the usage run the byte figures come from; `None` before one
    /// included the caller.
    #[serde(default)]
    pub computed_at: Option<String>,
    #[serde(default)]
    pub logical_bytes: u64,
    #[serde(default)]
    pub stored_bytes: u64,
    #[serde(default)]
    pub exclusive_bytes: u64,
    #[serde(default)]
    pub machines: u64,
    #[serde(default)]
    pub images: u64,
    #[serde(default)]
    pub limits: UsageLimits,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageLimits {
    #[serde(default)]
    pub max_machines: u64,
    #[serde(default)]
    pub max_images: u64,
    #[serde(default, rename = "maxStoredGiB")]
    pub max_stored_gib: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateImageRequest<'a> {
    pub name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub description: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MachineList {
    #[serde(default)]
    machines: Vec<Machine>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Operation {
    pub id: String,
    #[serde(default)]
    pub machine_id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub state: OperationState,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub error: Option<ErrorDetail>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostTrust {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub principals: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Connection {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub certificate: String,
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub host_trust: HostTrust,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: u64,
}

/// hangar-issued tokens. Times stay in the server's RFC 3339 text so the shared
/// credentials file keeps the exact format the hangar CLI writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub access_expires_at: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub refresh_expires_at: String,
}

/// The signed-in user (`GET /v1/me`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Me {
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub login: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateMachineRequest<'a> {
    pub name: &'a str,
    /// Exactly one of `template_id` and `image_id` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_id: Option<&'a str>,
}

/// A new machine with a copy of a stopped machine's root disk and /data. The sizes are
/// the source's.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkMachineRequest<'a> {
    pub name: &'a str,
    /// `running` (the server's default) or `stopped`.
    pub desired_state: &'a str,
}

// ---------------------------------------------------------------------------
// Errors.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransportError {
    /// The request may have reached the server.
    Timeout,
    /// The request was not sent (DNS or TCP connect failed).
    Unreachable(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub(crate) enum HangarError {
    Api(ApiError),
    Transport(TransportError),
    NotSignedIn,
    SessionExpired,
    WrongServer { signed_in: String, wanted: String },
    MachineNotRunning { machine: String, state: String },
    Invalid(String),
}

impl HangarError {
    pub(crate) fn code(&self) -> Option<&ErrorCode> {
        match self {
            Self::Api(error) => Some(&error.code),
            _ => None,
        }
    }

    /// Only a missing, expired or rejected sign-in can be fixed by signing in.
    /// `permission_denied` is an authorization decision, not a stale login.
    pub(crate) fn needs_sign_in(&self) -> bool {
        matches!(
            self,
            Self::NotSignedIn | Self::SessionExpired | Self::WrongServer { .. }
        ) || self.code() == Some(&ErrorCode::Unauthenticated)
    }

    /// Maps onto the saved-SSH failure policy: sign-in, permission, deleted machines and
    /// stopped machines need attention; capacity, host and transport errors use the
    /// normal reconnect backoff.
    pub(crate) fn into_io(self) -> std::io::Error {
        use std::io::ErrorKind;
        let kind = match &self {
            _ if self.needs_sign_in() => ErrorKind::PermissionDenied,
            Self::Api(error) => match error.code {
                ErrorCode::PermissionDenied => ErrorKind::PermissionDenied,
                ErrorCode::NotFound => ErrorKind::NotFound,
                ErrorCode::MachineNotRunning => ErrorKind::NotConnected,
                ErrorCode::BadRequest | ErrorCode::IdempotencyConflict => ErrorKind::InvalidInput,
                _ => ErrorKind::Other,
            },
            Self::MachineNotRunning { .. } => ErrorKind::NotConnected,
            Self::Transport(TransportError::Timeout) => ErrorKind::TimedOut,
            Self::Transport(_) => ErrorKind::Other,
            Self::Invalid(_) => ErrorKind::InvalidData,
            _ => ErrorKind::Other,
        };
        std::io::Error::new(kind, self.to_string())
    }
}

pub(crate) const SIGN_IN_HINT: &str =
    "Sign in to hangar from Settings → Remotes → Account, or run `hangar login`.";

impl std::fmt::Display for HangarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSignedIn => write!(f, "Not signed in to hangar. {SIGN_IN_HINT}"),
            Self::SessionExpired => write!(f, "The hangar sign-in expired. {SIGN_IN_HINT}"),
            Self::WrongServer { signed_in, wanted } => write!(
                f,
                "Signed in to hangar at {signed_in}, not {wanted}. Sign in again for {wanted}."
            ),
            Self::MachineNotRunning { machine, state } if state == "suspended" => write!(
                f,
                "hangar machine {machine} is suspended; use Resume remote to resume it."
            ),
            Self::MachineNotRunning { machine, state } => write!(
                f,
                "hangar machine {machine} is {state}; use Start remote to start it."
            ),
            Self::Invalid(message) => write!(f, "hangar: {message}"),
            Self::Transport(TransportError::Timeout) => {
                write!(f, "hangar did not respond in time; the outcome is unknown")
            }
            Self::Transport(TransportError::Unreachable(message)) => {
                write!(f, "Cannot reach hangar: {message}")
            }
            Self::Transport(TransportError::Failed(message)) => {
                write!(f, "hangar request failed: {message}")
            }
            Self::Api(error) => match &error.code {
                ErrorCode::Unauthenticated => {
                    write!(f, "The hangar sign-in was rejected. {SIGN_IN_HINT}")
                }
                ErrorCode::PermissionDenied => {
                    write!(f, "Not allowed on this hangar server: {}", error.message)
                }
                ErrorCode::NotFound => write!(
                    f,
                    "The hangar machine was deleted or is not visible to this account"
                ),
                ErrorCode::MachineNotRunning => write!(
                    f,
                    "The hangar machine is not running; use Start remote to start it."
                ),
                ErrorCode::OperationConflict => write!(
                    f,
                    "Another operation is running on this hangar machine: {}",
                    error.message
                ),
                ErrorCode::QuotaExceeded => write!(f, "hangar quota exceeded: {}", error.message),
                ErrorCode::NoCapacity | ErrorCode::HostUnavailable => write!(
                    f,
                    "hangar cannot place the machine right now: {}",
                    error.message
                ),
                code => write!(
                    f,
                    "hangar error ({}): {}",
                    match code {
                        ErrorCode::Unknown(code) => code.as_str(),
                        _ => "request failed",
                    },
                    error.message
                ),
            },
        }
    }
}

impl std::error::Error for HangarError {}

// ---------------------------------------------------------------------------
// Transport.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub bearer: Option<String>,
    pub idempotency_key: Option<String>,
    pub body: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(crate) trait HangarHttp: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// `curl` with the request in a config read from stdin: `-q` ignores `~/.curlrc`, and the
/// bearer token and body never appear in the process table.
pub(crate) struct CurlHttp;

fn curl_quote(value: &str) -> Result<String, TransportError> {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                return Err(TransportError::Failed(
                    "request contains a control character".into(),
                ))
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

pub(crate) fn curl_config(request: &HttpRequest) -> Result<String, TransportError> {
    let mut lines = vec![
        "silent".to_owned(),
        "show-error".to_owned(),
        format!("connect-timeout = {CONNECT_TIMEOUT_SECONDS}"),
        format!("max-time = {REQUEST_TIMEOUT_SECONDS}"),
        format!("max-filesize = {MAX_RESPONSE_BYTES}"),
        format!("request = {}", curl_quote(request.method)?),
        format!("url = {}", curl_quote(&request.url)?),
        format!("header = {}", curl_quote("Accept: application/json")?),
        format!(
            "header = {}",
            curl_quote(&format!("User-Agent: herdr/{}", env!("CARGO_PKG_VERSION")))?
        ),
        format!("write-out = {}", curl_quote("\n%{http_code}")?),
    ];
    if let Some(token) = &request.bearer {
        lines.push(format!(
            "header = {}",
            curl_quote(&format!("Authorization: Bearer {token}"))?
        ));
    }
    if let Some(key) = &request.idempotency_key {
        lines.push(format!(
            "header = {}",
            curl_quote(&format!("Idempotency-Key: {key}"))?
        ));
    }
    if let Some(body) = &request.body {
        lines.push(format!(
            "header = {}",
            curl_quote("Content-Type: application/json")?
        ));
        lines.push(format!("data-binary = {}", curl_quote(body)?));
    }
    Ok(lines.join("\n") + "\n")
}

fn parse_curl_output(stdout: &[u8]) -> Result<HttpResponse, TransportError> {
    let split = stdout
        .iter()
        .rposition(|byte| *byte == b'\n')
        .ok_or_else(|| TransportError::Failed("curl returned no status".into()))?;
    let status = std::str::from_utf8(&stdout[split + 1..])
        .ok()
        .and_then(|code| code.trim().parse::<u16>().ok())
        .filter(|code| *code >= 100)
        .ok_or_else(|| TransportError::Failed("curl returned no HTTP status".into()))?;
    Ok(HttpResponse {
        status,
        body: stdout[..split].to_vec(),
    })
}

impl HangarHttp for CurlHttp {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let config = curl_config(request)?;
        let mut child = crate::noninteractive_process::curl_command()
            .args(["-q", "-K", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                TransportError::Failed(format!("cannot run curl ({error}); install curl"))
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(error) = stdin.write_all(config.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TransportError::Failed(format!("curl input: {error}")));
            }
        }
        let mut stdout = Vec::new();
        if let Some(pipe) = child.stdout.take() {
            if let Err(error) = pipe
                .take(MAX_RESPONSE_BYTES as u64 + 64)
                .read_to_end(&mut stdout)
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TransportError::Failed(format!("curl output: {error}")));
            }
        }
        let mut stderr = String::new();
        if let Some(pipe) = child.stderr.take() {
            let _ = pipe.take(4096).read_to_string(&mut stderr);
        }
        let status = child
            .wait()
            .map_err(|error| TransportError::Failed(format!("curl: {error}")))?;
        let detail = || {
            let message = stderr.trim().trim_start_matches("curl: ").to_owned();
            if message.is_empty() {
                format!("curl exited with {status}")
            } else {
                message
            }
        };
        match status.code() {
            Some(0) => parse_curl_output(&stdout),
            Some(28) => Err(TransportError::Timeout),
            Some(5..=7) => Err(TransportError::Unreachable(detail())),
            _ => Err(TransportError::Failed(detail())),
        }
    }
}

// ---------------------------------------------------------------------------
// Client.

/// Supplies bearer tokens and replaces one the server rejected.
pub(crate) trait TokenSource: Send + Sync {
    fn token(&self) -> Result<String, HangarError>;
    fn refresh_rejected(&self, rejected: &str) -> Result<String, HangarError>;
}

#[derive(Clone)]
pub(crate) struct Client {
    server: String,
    http: Arc<dyn HangarHttp>,
    tokens: Option<Arc<dyn TokenSource>>,
    sleep: fn(Duration),
}

fn real_sleep(duration: Duration) {
    std::thread::sleep(duration);
}

impl Client {
    pub(crate) fn new(
        server: &str,
        http: Arc<dyn HangarHttp>,
        tokens: Option<Arc<dyn TokenSource>>,
    ) -> Self {
        Self {
            server: super::normalize_server(server),
            http,
            tokens,
            sleep: real_sleep,
        }
    }

    #[cfg(test)]
    pub(crate) fn without_sleep(mut self) -> Self {
        self.sleep = |_| {};
        self
    }

    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    pub(crate) fn sleep(&self, duration: Duration) {
        (self.sleep)(duration);
    }

    /// GETs and mutations carrying an idempotency key are retried after a timeout or a
    /// failed connect, with the same key. A 401 refreshes the token once.
    fn request(
        &self,
        method: &'static str,
        path: &str,
        body: Option<String>,
        idempotency_key: Option<&str>,
        auth: bool,
    ) -> Result<Vec<u8>, HangarError> {
        let retry_safe = method == "GET" || idempotency_key.is_some();
        let mut bearer = None;
        if auth {
            let tokens = self.tokens.as_ref().ok_or(HangarError::NotSignedIn)?;
            bearer = Some(tokens.token()?);
        }
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let request = HttpRequest {
                method,
                url: format!("{}{path}", self.server),
                bearer: bearer.clone(),
                idempotency_key: idempotency_key.map(str::to_owned),
                body: body.clone(),
            };
            match self.http.send(&request) {
                Err(error @ (TransportError::Timeout | TransportError::Unreachable(_)))
                    if retry_safe && attempt < ATTEMPTS =>
                {
                    tracing::debug!(?error, method, path, "retrying hangar request");
                    self.sleep(Duration::from_secs(attempt as u64));
                }
                Err(error) => return Err(HangarError::Transport(error)),
                Ok(response) if (200..300).contains(&response.status) => return Ok(response.body),
                Ok(response) if response.status == 401 && auth && !refreshed => {
                    refreshed = true;
                    let (Some(tokens), Some(rejected)) = (self.tokens.as_ref(), bearer.as_ref())
                    else {
                        return Err(HangarError::NotSignedIn);
                    };
                    bearer = Some(tokens.refresh_rejected(rejected)?);
                }
                Ok(response) => return Err(decode_error(response.status, &response.body)),
            }
        }
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        body: Option<String>,
        idempotency_key: Option<&str>,
        auth: bool,
    ) -> Result<T, HangarError> {
        let bytes = self.request(method, path, body, idempotency_key, auth)?;
        serde_json::from_slice(&bytes).map_err(|error| {
            HangarError::Invalid(format!("unexpected response to {method} {path}: {error}"))
        })
    }

    pub(crate) fn machines(&self) -> Result<Vec<Machine>, HangarError> {
        let mut machines = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_MACHINE_PAGES {
            let path = match &cursor {
                Some(cursor) => format!("/v1/machines?limit=100&cursor={}", query_escape(cursor)),
                None => "/v1/machines?limit=100".to_owned(),
            };
            let page: MachineList = self.call("GET", &path, None, None, true)?;
            machines.extend(page.machines);
            match page.next_cursor.filter(|cursor| !cursor.is_empty()) {
                Some(next) => cursor = Some(next),
                None => return Ok(machines),
            }
        }
        // A cut-off list would look like deleted machines to a sync: it is a failure.
        Err(HangarError::Invalid(format!(
            "more than {} machines are listed; the list is incomplete",
            MAX_MACHINE_PAGES * 100
        )))
    }

    pub(crate) fn machine(&self, id: &str) -> Result<Machine, HangarError> {
        self.call(
            "GET",
            &format!("/v1/machines/{}", path_segment(id)?),
            None,
            None,
            true,
        )
    }

    pub(crate) fn create_machine(
        &self,
        key: &str,
        request: &CreateMachineRequest<'_>,
    ) -> Result<Operation, HangarError> {
        let body = serde_json::to_string(request)
            .map_err(|error| HangarError::Invalid(error.to_string()))?;
        self.call("POST", "/v1/machines", Some(body), Some(key), true)
    }

    /// Returns the `create` operation of the new machine.
    pub(crate) fn fork_machine(
        &self,
        key: &str,
        source_id: &str,
        request: &ForkMachineRequest<'_>,
    ) -> Result<Operation, HangarError> {
        let body = serde_json::to_string(request)
            .map_err(|error| HangarError::Invalid(error.to_string()))?;
        self.call(
            "POST",
            &format!("/v1/machines/{}/fork", path_segment(source_id)?),
            Some(body),
            Some(key),
            true,
        )
    }

    pub(crate) fn start_machine(&self, key: &str, id: &str) -> Result<Operation, HangarError> {
        self.call(
            "POST",
            &format!("/v1/machines/{}/start", path_segment(id)?),
            None,
            Some(key),
            true,
        )
    }

    pub(crate) fn stop_machine(&self, key: &str, id: &str) -> Result<Operation, HangarError> {
        self.call(
            "POST",
            &format!("/v1/machines/{}/stop", path_segment(id)?),
            None,
            Some(key),
            true,
        )
    }

    /// Snapshots a running machine's memory and stops it; start resumes it.
    pub(crate) fn suspend_machine(&self, key: &str, id: &str) -> Result<Operation, HangarError> {
        self.call(
            "POST",
            &format!("/v1/machines/{}/suspend", path_segment(id)?),
            None,
            Some(key),
            true,
        )
    }

    /// Deletes the machine with its persistent disk and snapshots.
    pub(crate) fn delete_machine(&self, key: &str, id: &str) -> Result<Operation, HangarError> {
        self.call(
            "DELETE",
            &format!("/v1/machines/{}", path_segment(id)?),
            None,
            Some(key),
            true,
        )
    }

    pub(crate) fn templates(&self) -> Result<Vec<Template>, HangarError> {
        let list: TemplateList = self.call("GET", "/v1/templates", None, None, true)?;
        Ok(list.templates)
    }

    /// The caller's images, oldest first.
    pub(crate) fn images(&self) -> Result<Vec<Image>, HangarError> {
        let list: ImageList = self.call("GET", "/v1/images", None, None, true)?;
        Ok(list.images)
    }

    /// The caller's storage usage and limits.
    pub(crate) fn usage(&self) -> Result<Usage, HangarError> {
        self.call("GET", "/v1/usage", None, None, true)
    }

    /// Saves a stopped, synced machine's root disk as an image. Synchronous.
    pub(crate) fn create_image(
        &self,
        key: &str,
        machine_id: &str,
        request: &CreateImageRequest<'_>,
    ) -> Result<Image, HangarError> {
        let body = serde_json::to_string(request)
            .map_err(|error| HangarError::Invalid(error.to_string()))?;
        self.call(
            "POST",
            &format!("/v1/machines/{}/images", path_segment(machine_id)?),
            Some(body),
            Some(key),
            true,
        )
    }

    /// Deletes an image; machines created from it are not affected.
    pub(crate) fn delete_image(&self, key: &str, id: &str) -> Result<(), HangarError> {
        self.request(
            "DELETE",
            &format!("/v1/images/{}", path_segment(id)?),
            None,
            Some(key),
            true,
        )
        .map(|_| ())
    }

    pub(crate) fn operation(&self, id: &str) -> Result<Operation, HangarError> {
        self.call(
            "GET",
            &format!("/v1/operations/{}", path_segment(id)?),
            None,
            None,
            true,
        )
    }

    pub(crate) fn create_connection(
        &self,
        key: &str,
        machine_id: &str,
        public_key: &str,
        ttl_seconds: u64,
    ) -> Result<Connection, HangarError> {
        let body = serde_json::json!({ "publicKey": public_key, "ttlSeconds": ttl_seconds });
        self.call(
            "POST",
            &format!("/v1/machines/{}/connections", path_segment(machine_id)?),
            Some(body.to_string()),
            Some(key),
            true,
        )
    }

    pub(crate) fn start_device(&self) -> Result<DeviceStart, HangarError> {
        self.call("POST", "/v1/auth/device", None, None, false)
    }

    pub(crate) fn poll_device(&self, device_code: &str) -> Result<Tokens, HangarError> {
        let body = serde_json::json!({ "deviceCode": device_code });
        self.call(
            "POST",
            "/v1/auth/device/token",
            Some(body.to_string()),
            None,
            false,
        )
    }

    /// Whether the server offers the browser (loopback) sign-in. Like the hangar CLI,
    /// a request without parameters gets 400 when it does; 404 (older servers) and 503
    /// (browser sign-in not configured) mean it does not. `Err` says why not.
    pub(crate) fn probe_cli_login(&self) -> Result<(), String> {
        let request = HttpRequest {
            method: "GET",
            url: format!("{}/auth/cli/start", self.server),
            bearer: None,
            idempotency_key: None,
            body: None,
        };
        match self.http.send(&request) {
            Ok(response) if response.status == 400 => Ok(()),
            Ok(response) => Err(format!(
                "the server does not offer browser sign-in (HTTP {})",
                response.status
            )),
            Err(error) => Err(HangarError::Transport(error).to_string()),
        }
    }

    /// The page that starts the loopback sign-in in the user's browser.
    pub(crate) fn cli_start_url(&self, redirect_uri: &str, state: &str, challenge: &str) -> String {
        format!(
            "{}/auth/cli/start?redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
            self.server,
            query_escape(redirect_uri),
            query_escape(state),
            query_escape(challenge)
        )
    }

    /// Exchanges a one-time loopback code. Never retried: codes are single use.
    pub(crate) fn exchange_cli_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<Tokens, HangarError> {
        let body = serde_json::json!({
            "code": code, "codeVerifier": verifier, "redirectUri": redirect_uri
        });
        self.call(
            "POST",
            "/v1/auth/cli/token",
            Some(body.to_string()),
            None,
            false,
        )
    }

    pub(crate) fn me(&self) -> Result<Me, HangarError> {
        self.call("GET", "/v1/me", None, None, true)
    }

    /// Revokes the presented access token and its refresh token.
    pub(crate) fn logout(&self) -> Result<(), HangarError> {
        self.request("POST", "/v1/auth/logout", None, None, true)
            .map(|_| ())
    }

    pub(crate) fn refresh(&self, refresh_token: &str) -> Result<Tokens, HangarError> {
        let body = serde_json::json!({ "refreshToken": refresh_token });
        // Never retried: a refresh that reached the server rotated the token family.
        self.call(
            "POST",
            "/v1/auth/refresh",
            Some(body.to_string()),
            None,
            false,
        )
    }
}

fn decode_error(status: u16, body: &[u8]) -> HangarError {
    if let Ok(parsed) = serde_json::from_slice::<ErrorBody>(body) {
        if !parsed.error.code.is_empty() {
            return HangarError::Api(ApiError {
                status,
                code: ErrorCode::parse(&parsed.error.code),
                message: parsed.error.message,
                operation_id: parsed.error.operation_id,
            });
        }
    }
    let message: String = String::from_utf8_lossy(body)
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    let code = match status {
        401 => ErrorCode::Unauthenticated,
        403 => ErrorCode::PermissionDenied,
        404 => ErrorCode::NotFound,
        _ => ErrorCode::Unknown(format!("http {status}")),
    };
    HangarError::Api(ApiError {
        status,
        code,
        message,
        operation_id: None,
    })
}

/// IDs are server-generated; refuse anything that could change the request path.
fn path_segment(id: &str) -> Result<&str, HangarError> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(id)
    } else {
        Err(HangarError::Invalid(format!("invalid identifier {id:?}")))
    }
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            byte => format!("%{byte:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Test doubles.

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted HTTP: answers requests in order and records them.
    #[derive(Default)]
    pub(crate) struct FakeHttp {
        responses: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
        pub requests: Mutex<Vec<HttpRequest>>,
    }

    impl FakeHttp {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub(crate) fn reply(&self, status: u16, body: serde_json::Value) -> &Self {
            self.push(Ok(HttpResponse {
                status,
                body: body.to_string().into_bytes(),
            }))
        }

        pub(crate) fn error(&self, status: u16, code: &str) -> &Self {
            self.reply(
                status,
                serde_json::json!({"error": {"code": code, "message": code, "requestId": "r", "retryable": false, "operationId": null}}),
            )
        }

        pub(crate) fn push(&self, response: Result<HttpResponse, TransportError>) -> &Self {
            self.responses
                .lock()
                .expect("fake http lock")
                .push_back(response);
            self
        }

        pub(crate) fn sent(&self) -> Vec<HttpRequest> {
            self.requests.lock().expect("fake http lock").clone()
        }

        pub(crate) fn paths(&self) -> Vec<String> {
            self.sent()
                .into_iter()
                .map(|request| {
                    format!(
                        "{} {}",
                        request.method,
                        request
                            .url
                            .split_once("://")
                            .and_then(|(_, rest)| rest.split_once('/'))
                            .map(|(_, path)| format!("/{path}"))
                            .unwrap_or_default()
                    )
                })
                .collect()
        }
    }

    impl HangarHttp for FakeHttp {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.requests
                .lock()
                .expect("fake http lock")
                .push(request.clone());
            self.responses
                .lock()
                .expect("fake http lock")
                .pop_front()
                .unwrap_or_else(|| Err(TransportError::Failed("no scripted response".into())))
        }
    }

    pub(crate) struct StaticToken(pub &'static str);

    impl TokenSource for StaticToken {
        fn token(&self) -> Result<String, HangarError> {
            Ok(self.0.to_owned())
        }
        fn refresh_rejected(&self, _rejected: &str) -> Result<String, HangarError> {
            Err(HangarError::SessionExpired)
        }
    }

    pub(crate) fn client(http: &Arc<FakeHttp>) -> Client {
        Client::new(
            "https://hangar.test",
            http.clone(),
            Some(Arc::new(StaticToken("token"))),
        )
        .without_sleep()
    }

    pub(crate) fn machine(id: &str, state: &str, ready: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": format!("name-{id}"), "desiredState": "running", "state": state,
            "revision": 1, "operationId": null, "runtime": {"ready": ready},
            "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z"
        })
    }

    pub(crate) fn image(id: &str, name: &str, source: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": name, "sourceMachineId": source, "sourceSnapshotSeq": 3,
            "template": {"id": "herdr", "version": "2026-10-03.2", "digest": "sha256:x"},
            "rootSizeBytes": 4294967296u64, "createdAt": "2026-10-03T08:00:00Z"
        })
    }

    pub(crate) fn template(version: &str, capabilities: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "id": "herdr", "version": version, "digest": "sha256:x", "arch": "x86_64",
            "defaultSpec": {"vcpus": 2, "memMiB": 2048, "persistentDiskGiB": 5},
            "capabilities": capabilities
        })
    }

    pub(crate) fn operation(id: &str, kind: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "machineId": "m_x", "type": kind, "state": state, "error": null,
            "createdAt": "2026-10-02T00:00:00Z", "updatedAt": "2026-10-02T00:00:00Z"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn an_incomplete_machine_listing_is_an_error_not_a_shorter_list() {
        let page = |id: &str, next: Option<&str>| serde_json::json!({"machines": [machine(id, "running", true)], "nextCursor": next});
        // Every page leads to another: the cap is reached with a cursor remaining.
        let http = FakeHttp::new();
        for index in 0..MAX_MACHINE_PAGES {
            http.reply(200, page(&format!("m_{index}"), Some("more")));
        }
        assert!(client(&http).machines().is_err());
        assert_eq!(http.sent().len(), MAX_MACHINE_PAGES);
        // A failing later page fails the whole listing.
        let http = FakeHttp::new();
        http.reply(200, page("m_a", Some("c2")))
            .error(500, "internal");
        assert!(client(&http).machines().is_err());
        // A complete listing follows the cursor to the end.
        let http = FakeHttp::new();
        http.reply(200, page("m_a", Some("c2")))
            .reply(200, page("m_b", None));
        assert_eq!(client(&http).machines().unwrap().len(), 2);
    }

    #[test]
    fn unknown_states_and_codes_decode_to_fallbacks() {
        let machine: Machine = serde_json::from_value(serde_json::json!({
            "id": "m_a", "name": "a", "state": "hibernating", "futureField": 1
        }))
        .unwrap();
        assert_eq!(machine.state, MachineState::Unknown);
        assert!(!machine.runtime.ready);
        let operation: Operation = serde_json::from_value(serde_json::json!({
            "id": "op_a", "state": "paused"
        }))
        .unwrap();
        assert_eq!(operation.state, OperationState::Unknown);
        assert_eq!(
            ErrorCode::parse("teleport_failed"),
            ErrorCode::Unknown("teleport_failed".into())
        );
    }

    #[test]
    fn image_fields_and_unknown_capabilities_decode() {
        let machine: Machine = serde_json::from_value(serde_json::json!({
            "id": "m_a", "name": "a", "state": "stopped",
            "template": {"id": "herdr", "version": "v1", "digest": "d"},
            "storage": {"sizeGiB": 5, "mountPath": "/data", "persistent": true, "synced": true},
            "image": {"id": "im_a"}, "forkedFrom": {"machineId": "m_b", "snapshotSeq": 2}
        }))
        .unwrap();
        assert_eq!(machine.template.unwrap().label(), "herdr@v1");
        assert!(machine.storage.unwrap().synced);
        assert_eq!(machine.image.unwrap().id, "im_a");
        assert_eq!(machine.forked_from.unwrap().machine_id, "m_b");
        let template: Template =
            serde_json::from_value(template("v2", &["identity-reset", "teleport"])).unwrap();
        assert!(template.has(TemplateCapability::IdentityReset));
        assert_eq!(template.capabilities[1], TemplateCapability::Unknown);
        let image: Image = serde_json::from_value(image("im_a", "base", "m_a")).unwrap();
        assert_eq!(image.template.label(), "herdr@2026-10-03.2");
    }

    #[test]
    fn usage_spec_and_image_sizes_decode() {
        let http = FakeHttp::new();
        http.reply(
            200,
            serde_json::json!({
                "computedAt": "2026-10-03T08:00:00Z", "logicalBytes": 10, "storedBytes": 2147483648u64,
                "exclusiveBytes": 1, "machines": 3, "images": 2,
                "limits": {"maxMachines": 5, "maxImages": 10, "maxStoredGiB": 20}
            }),
        );
        let usage = client(&http).usage().unwrap();
        assert_eq!(http.paths(), ["GET /v1/usage"]);
        assert_eq!(usage.stored_bytes, 2 << 30);
        assert_eq!(usage.limits.max_stored_gib, 20);
        assert_eq!(usage.limits.max_images, 10);
        let never: Usage = serde_json::from_value(serde_json::json!({"computedAt": null})).unwrap();
        assert_eq!(never.computed_at, None);
        let machine: Machine = serde_json::from_value(serde_json::json!({
            "id": "m_a", "name": "a", "state": "running",
            "spec": {"vcpus": 2, "memMiB": 4096, "persistentDiskGiB": 20, "rootDiskGiB": 8}
        }))
        .unwrap();
        let spec = machine.spec.unwrap();
        assert_eq!((spec.vcpus, spec.mem_mib), (2, 4096));
        assert_eq!(
            (spec.persistent_disk_gib, spec.root_disk_gib),
            (20, Some(8))
        );
        let mut value = image("im_a", "base", "m_a");
        value["exclusiveBytes"] = 512.into();
        let image: Image = serde_json::from_value(value).unwrap();
        assert_eq!(image.exclusive_bytes, Some(512));
    }

    #[test]
    fn image_mutations_carry_keys_and_create_sends_only_the_image() {
        let http = FakeHttp::new();
        http.reply(201, image("im_a", "base", "m_a"))
            .push(Ok(HttpResponse {
                status: 204,
                body: Vec::new(),
            }))
            .reply(202, operation("op_1", "create", "queued"));
        let client = client(&http);
        let saved = client
            .create_image(
                "k1",
                "m_a",
                &CreateImageRequest {
                    name: "base",
                    description: "",
                },
            )
            .unwrap();
        assert_eq!(saved.id, "im_a");
        client.delete_image("k2", "im_a").unwrap();
        client
            .create_machine(
                "k3",
                &CreateMachineRequest {
                    name: "box",
                    template_id: None,
                    image_id: Some("im_a"),
                },
            )
            .unwrap();
        assert_eq!(
            http.paths(),
            [
                "POST /v1/machines/m_a/images",
                "DELETE /v1/images/im_a",
                "POST /v1/machines"
            ]
        );
        let sent = http.sent();
        assert!(sent.iter().all(|request| request.idempotency_key.is_some()));
        let body = |index: usize| -> serde_json::Value {
            serde_json::from_str(sent[index].body.as_deref().unwrap()).unwrap()
        };
        assert_eq!(body(0), serde_json::json!({"name": "base"}));
        assert_eq!(
            body(2),
            serde_json::json!({"name": "box", "imageId": "im_a"})
        );
        assert!(client.delete_image("k", "../x").is_err());
    }

    #[test]
    fn fork_posts_to_the_source_with_a_key_and_only_name_and_state() {
        let http = FakeHttp::new();
        let mut accepted = operation("op_1", "create", "queued");
        accepted["machineId"] = "m_new".into();
        http.reply(202, accepted);
        let client = client(&http);
        let operation = client
            .fork_machine(
                "k1",
                "m_a",
                &ForkMachineRequest {
                    name: "a-fork",
                    desired_state: "running",
                },
            )
            .unwrap();
        assert_eq!(operation.machine_id, "m_new");
        assert_eq!(http.paths(), ["POST /v1/machines/m_a/fork"]);
        let sent = &http.sent()[0];
        assert_eq!(sent.idempotency_key.as_deref(), Some("k1"));
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name": "a-fork", "desiredState": "running"})
        );
        let request = ForkMachineRequest {
            name: "x",
            desired_state: "running",
        };
        assert!(client.fork_machine("k", "../x", &request).is_err());
    }

    #[test]
    fn mutation_timeout_is_retried_with_the_same_idempotency_key() {
        let http = FakeHttp::new();
        http.push(Err(TransportError::Timeout))
            .reply(202, operation("op_1", "start", "queued"));
        let client = client(&http);
        let operation = client.start_machine("key-1", "m_a").unwrap();
        assert_eq!(operation.id, "op_1");
        let sent = http.sent();
        assert_eq!(sent.len(), 2);
        assert!(sent
            .iter()
            .all(|request| request.idempotency_key.as_deref() == Some("key-1")));
        assert_eq!(sent[0], sent[1]);
    }

    #[test]
    fn a_failed_refresh_is_never_retried() {
        let http = FakeHttp::new();
        http.push(Err(TransportError::Timeout));
        let client = Client::new("https://hangar.test", http.clone(), None).without_sleep();
        assert!(matches!(
            client.refresh("rt"),
            Err(HangarError::Transport(TransportError::Timeout))
        ));
        assert_eq!(http.sent().len(), 1);
    }

    #[test]
    fn structured_errors_keep_code_and_operation() {
        let http = FakeHttp::new();
        http.reply(
            409,
            serde_json::json!({"error": {"code": "operation_conflict", "message": "busy", "requestId": "r", "retryable": false, "operationId": "op_9"}}),
        );
        let error = client(&http).stop_machine("k", "m_a").unwrap_err();
        let HangarError::Api(error) = error else {
            panic!("api error");
        };
        assert_eq!(error.code, ErrorCode::OperationConflict);
        assert_eq!(error.operation_id.as_deref(), Some("op_9"));
    }

    #[test]
    fn error_table_maps_to_attention_or_backoff() {
        use std::io::ErrorKind;
        let api = |code: &str| {
            HangarError::Api(ApiError {
                status: 400,
                code: ErrorCode::parse(code),
                message: code.into(),
                operation_id: None,
            })
        };
        let unauthenticated = api("unauthenticated");
        assert!(unauthenticated.needs_sign_in());
        assert!(unauthenticated.to_string().contains("Sign in to hangar"));
        assert_eq!(
            unauthenticated.into_io().kind(),
            ErrorKind::PermissionDenied
        );
        let denied = api("permission_denied");
        assert!(!denied.needs_sign_in());
        assert!(denied
            .to_string()
            .contains("Not allowed on this hangar server"));
        assert_eq!(api("not_found").into_io().kind(), ErrorKind::NotFound);
        let stopped = HangarError::MachineNotRunning {
            machine: "box".into(),
            state: "stopped".into(),
        };
        assert!(stopped.to_string().contains("use Start remote"));
        assert_eq!(stopped.into_io().kind(), ErrorKind::NotConnected);
        for code in ["no_capacity", "host_unavailable", "internal"] {
            assert_eq!(api(code).into_io().kind(), ErrorKind::Other);
        }
        assert_eq!(
            HangarError::Transport(TransportError::Timeout)
                .into_io()
                .kind(),
            ErrorKind::TimedOut
        );
    }

    #[test]
    fn curl_config_quotes_values_and_keeps_secrets_off_the_command_line() {
        let config = curl_config(&HttpRequest {
            method: "POST",
            url: "https://h/v1/machines".into(),
            bearer: Some("se\"cret".into()),
            idempotency_key: Some("k1".into()),
            body: Some(r#"{"name":"a\\b"}"#.into()),
        })
        .unwrap();
        assert!(config.contains("header = \"Authorization: Bearer se\\\"cret\"\n"));
        assert!(config.contains("header = \"Idempotency-Key: k1\"\n"));
        assert!(config.contains("data-binary = \"{\\\"name\\\":\\\"a\\\\\\\\b\\\"}\"\n"));
        assert!(config.contains("write-out = \"\\n%{http_code}\"\n"));
        assert!(curl_config(&HttpRequest {
            method: "GET",
            url: "https://h/\u{7}".into(),
            bearer: None,
            idempotency_key: None,
            body: None,
        })
        .is_err());
        let response = parse_curl_output(b"{\"a\":1}\n\n201").unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"{\"a\":1}\n");
    }

    #[test]
    fn machine_listing_follows_cursors_and_rejects_path_injection() {
        let http = FakeHttp::new();
        http.reply(
            200,
            serde_json::json!({"machines": [machine("m_a", "running", true)], "nextCursor": "c/1"}),
        )
        .reply(
            200,
            serde_json::json!({"machines": [machine("m_b", "stopped", false)], "nextCursor": null}),
        );
        let client = client(&http);
        let machines = client.machines().unwrap();
        assert_eq!(machines.len(), 2);
        assert_eq!(http.paths()[1], "GET /v1/machines?limit=100&cursor=c%2F1");
        assert!(client.machine("../me").is_err());
    }
}
