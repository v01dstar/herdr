//! Sign-in shared with the hangar CLI.
//!
//! `credentials.json` and its `.lock` file in the hangar config directory are a contract
//! with the `hangar` CLI: same JSON fields and the same exclusive `flock` on `.lock`
//! around every refresh. Refresh tokens rotate and reuse revokes the whole family, so a
//! refresh always re-reads the file under the lock and skips the call when another
//! process already rotated the token.
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::api::{Client, DeviceStart, ErrorCode, HangarError, HangarHttp, TokenSource, Tokens};

/// Refresh access tokens this long before they expire (same as the CLI).
const REFRESH_SKEW: Duration = Duration::from_secs(30);
/// Go's zero `time.Time`; the CLI cannot decode an empty string.
const GO_ZERO_TIME: &str = "0001-01-01T00:00:00Z";
const DEFAULT_DEVICE_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_DEVICE_EXPIRY: Duration = Duration::from_secs(900);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Credentials {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub access_expires_at: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub refresh_expires_at: String,
    /// Fields a newer CLI may add survive a herdr refresh.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Credentials {
    fn with_tokens(&mut self, tokens: Tokens) {
        self.access_token = tokens.access_token;
        self.access_expires_at = tokens.access_expires_at;
        self.refresh_token = tokens.refresh_token;
        self.refresh_expires_at = tokens.refresh_expires_at;
    }
}

/// `None` for an empty or Go zero time ("does not expire"). An unparsable time is
/// treated as already expired so a refresh replaces it.
fn expiry(text: &str) -> Option<SystemTime> {
    if text.is_empty() || text == GO_ZERO_TIME {
        return None;
    }
    match time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339) {
        Ok(at) if at.year() <= 1 => None,
        Ok(at) => {
            let nanos = at.unix_timestamp_nanos();
            if nanos <= 0 {
                Some(SystemTime::UNIX_EPOCH)
            } else {
                Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos as u64))
            }
        }
        Err(_) => Some(SystemTime::UNIX_EPOCH),
    }
}

fn expires_within(text: &str, now: SystemTime, window: Duration) -> bool {
    expiry(text).is_some_and(|at| now + window >= at)
}

/// The hangar CLI's config directory: `$XDG_CONFIG_HOME/hangar`, else `~/.config/hangar`.
pub(crate) fn hangar_config_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(dir) = var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("hangar"));
    }
    var("HOME")
        .or_else(|| var("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".config").join("hangar"))
}

pub(crate) fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match crate::platform::create_remote_private_dir(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

/// Opens (creating if needed) `path` and takes an exclusive lock on it. On Unix this is
/// `flock(LOCK_EX)`, the same lock the hangar CLI takes. Dropping the file unlocks it.
pub(crate) fn lock_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let file = match crate::platform::create_private_state_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::OpenOptions::new().write(true).open(path)?
        }
        Err(error) => return Err(error),
    };
    file.lock()?;
    Ok(file)
}

fn io_error(context: &str, error: impl std::fmt::Display) -> HangarError {
    HangarError::Invalid(format!("{context}: {error}"))
}

#[derive(Clone, Debug)]
pub(crate) struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    pub(crate) fn shared() -> Option<Self> {
        hangar_config_dir().map(Self::at)
    }

    pub(crate) fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("credentials.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join(".lock")
    }

    pub(crate) fn lock(&self) -> Result<File, HangarError> {
        lock_file(&self.lock_path()).map_err(|error| io_error("lock hangar credentials", error))
    }

    pub(crate) fn load(&self) -> Result<Option<Credentials>, HangarError> {
        let bytes = match std::fs::read(self.path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read hangar credentials", error)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io_error(&self.path().display().to_string(), error))
    }

    /// Removes the stored sign-in, as `hangar logout` does. Callers hold `lock()`.
    pub(crate) fn delete(&self) -> Result<(), HangarError> {
        match std::fs::remove_file(self.path()) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(io_error("remove hangar credentials", error))
            }
            _ => Ok(()),
        }
    }

    /// Atomic replace with mode 0600, like the CLI. Callers hold `lock()`.
    pub(crate) fn save(&self, credentials: &Credentials) -> Result<(), HangarError> {
        ensure_private_dir(&self.dir)
            .map_err(|error| io_error("hangar config directory", error))?;
        let mut stored = credentials.clone();
        for time in [
            &mut stored.access_expires_at,
            &mut stored.refresh_expires_at,
        ] {
            if time.is_empty() {
                *time = GO_ZERO_TIME.to_owned();
            }
        }
        let mut bytes = serde_json::to_vec_pretty(&stored)
            .map_err(|error| io_error("encode hangar credentials", error))?;
        bytes.push(b'\n');
        crate::client::endpoint::store_private_json(&self.path(), &bytes, "hangar credentials")
            .map_err(HangarError::Invalid)
    }
}

pub(crate) type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

pub(crate) fn system_clock() -> Clock {
    Arc::new(SystemTime::now)
}

/// Hands out the stored access token for one server and rotates it when needed.
pub(crate) struct StoredTokens {
    store: CredentialStore,
    server: String,
    refresher: Client,
    now: Clock,
}

impl StoredTokens {
    pub(crate) fn new(
        store: CredentialStore,
        server: &str,
        http: Arc<dyn HangarHttp>,
        now: Clock,
    ) -> Self {
        let refresher = Client::new(server, http, None);
        Self {
            store,
            server: refresher.server().to_owned(),
            refresher,
            now,
        }
    }

    fn load(&self) -> Result<Credentials, HangarError> {
        let credentials = self.store.load()?.ok_or(HangarError::NotSignedIn)?;
        if credentials.access_token.is_empty() {
            return Err(HangarError::NotSignedIn);
        }
        let signed_in = super::normalize_server(&credentials.server);
        if signed_in != self.server {
            return Err(HangarError::WrongServer {
                signed_in,
                wanted: self.server.clone(),
            });
        }
        Ok(credentials)
    }

    fn refresh_locked(&self, stale: &str) -> Result<String, HangarError> {
        let _lock = self.store.lock()?;
        let mut credentials = self.load()?;
        let now = (self.now)();
        if credentials.access_token != stale
            && !expires_within(&credentials.access_expires_at, now, REFRESH_SKEW)
        {
            return Ok(credentials.access_token); // another process already rotated it
        }
        if credentials.refresh_token.is_empty()
            || expires_within(&credentials.refresh_expires_at, now, Duration::ZERO)
        {
            return Err(HangarError::SessionExpired);
        }
        let tokens = match self.refresher.refresh(&credentials.refresh_token) {
            Ok(tokens) => tokens,
            Err(HangarError::Api(error)) if matches!(error.status, 400 | 401) => {
                return Err(HangarError::SessionExpired)
            }
            Err(error) => return Err(error),
        };
        if tokens.access_token.is_empty() {
            return Err(HangarError::Invalid(
                "refresh returned no access token".into(),
            ));
        }
        credentials.with_tokens(tokens);
        self.store.save(&credentials)?;
        Ok(credentials.access_token)
    }
}

impl TokenSource for StoredTokens {
    fn token(&self) -> Result<String, HangarError> {
        let credentials = self.load()?;
        if expires_within(&credentials.access_expires_at, (self.now)(), REFRESH_SKEW) {
            return self.refresh_locked(&credentials.access_token);
        }
        Ok(credentials.access_token)
    }

    fn refresh_rejected(&self, rejected: &str) -> Result<String, HangarError> {
        self.refresh_locked(rejected)
    }
}

/// An authenticated client for `server` using the shared credentials and curl.
pub(crate) fn shared_client(server: &str) -> Result<Client, HangarError> {
    let store = CredentialStore::shared().ok_or(HangarError::NotSignedIn)?;
    let http: Arc<dyn HangarHttp> = Arc::new(super::api::CurlHttp);
    let tokens = StoredTokens::new(store, server, http.clone(), system_clock());
    Ok(Client::new(server, http, Some(Arc::new(tokens))))
}

/// What Settings shows about the shared hangar sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountStatus {
    SignedOut {
        server: String,
    },
    SignedIn {
        server: String,
        login: String,
    },
    /// Stored credentials that expired or that hangar rejected.
    Expired {
        server: String,
    },
    /// Stored credentials whose user could not be read (offline, server error).
    Unverified {
        server: String,
        error: String,
    },
}

impl AccountStatus {
    pub(crate) fn signed_in(&self) -> bool {
        !matches!(self, Self::SignedOut { .. })
    }

    pub(crate) fn summary(&self) -> String {
        match self {
            Self::SignedOut { server } => format!("Not signed in to hangar ({server})."),
            Self::SignedIn { server, login } => format!("Signed in as @{login} on {server}."),
            Self::Expired { server } => {
                format!("The hangar sign-in on {server} expired. Sign in again.")
            }
            Self::Unverified { server, error } => {
                format!("Signed in on {server}, but the account could not be checked: {error}")
            }
        }
    }
}

/// Reads the shared sign-in and asks hangar who it belongs to (refreshing the access
/// token when needed). `fallback_server` is shown when nobody is signed in.
pub(crate) fn account_status(
    store: &CredentialStore,
    http: Arc<dyn HangarHttp>,
    now: Clock,
    fallback_server: &str,
) -> AccountStatus {
    let credentials = match store.load() {
        Ok(Some(credentials)) if !credentials.access_token.is_empty() => credentials,
        Ok(_) => {
            return AccountStatus::SignedOut {
                server: fallback_server.to_owned(),
            }
        }
        Err(error) => {
            return AccountStatus::Unverified {
                server: fallback_server.to_owned(),
                error: error.to_string(),
            }
        }
    };
    let server = super::normalize_server(&credentials.server);
    let tokens = StoredTokens::new(store.clone(), &server, http.clone(), now);
    match Client::new(&server, http, Some(Arc::new(tokens))).me() {
        Ok(me) => AccountStatus::SignedIn {
            server,
            login: me.login,
        },
        Err(error) if error.needs_sign_in() => AccountStatus::Expired { server },
        Err(error) => AccountStatus::Unverified {
            server,
            error: error.to_string(),
        },
    }
}

/// The stored access token as is; logout never refreshes it first.
struct FixedToken(String);

impl TokenSource for FixedToken {
    fn token(&self) -> Result<String, HangarError> {
        Ok(self.0.clone())
    }

    fn refresh_rejected(&self, _rejected: &str) -> Result<String, HangarError> {
        Err(HangarError::SessionExpired)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignOut {
    NotSignedIn,
    /// `warning` says why the server-side revocation failed; the local sign-in is
    /// removed regardless.
    SignedOut {
        server: String,
        warning: Option<String>,
    },
}

/// Like `hangar logout`: revokes the tokens on the server, then removes the shared
/// credentials, which signs out the hangar CLI too. A rejected token is already
/// unusable and is not reported.
pub(crate) fn sign_out(
    store: &CredentialStore,
    http: Arc<dyn HangarHttp>,
) -> Result<SignOut, HangarError> {
    let _lock = store.lock()?;
    let Some(credentials) = store.load()? else {
        return Ok(SignOut::NotSignedIn);
    };
    let server = super::normalize_server(&credentials.server);
    let warning = if credentials.access_token.is_empty() {
        None
    } else {
        let token = Arc::new(FixedToken(credentials.access_token.clone()));
        match Client::new(&server, http, Some(token)).logout() {
            Ok(()) => None,
            Err(error) if error.needs_sign_in() => None,
            Err(error) => Some(error.to_string()),
        }
    };
    store.delete()?;
    Ok(SignOut::SignedOut { server, warning })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DevicePoll {
    Pending,
    SlowDown,
    Approved(Tokens),
}

pub(crate) fn poll_device_once(
    client: &Client,
    device_code: &str,
) -> Result<DevicePoll, HangarError> {
    match client.poll_device(device_code) {
        Ok(tokens) => Ok(DevicePoll::Approved(tokens)),
        Err(error) => match error.code() {
            Some(ErrorCode::AuthorizationPending) => Ok(DevicePoll::Pending),
            Some(ErrorCode::SlowDown) => Ok(DevicePoll::SlowDown),
            Some(ErrorCode::ExpiredToken) => Err(HangarError::Invalid(
                "the sign-in code expired; start sign-in again".into(),
            )),
            _ => Err(refused(error)),
        },
    }
}

/// A refused sign-in or sign-up (`access_denied`, `invite_required`, `invite_invalid`)
/// keeps its code so callers can tell the cases apart; other errors pass unchanged.
pub(crate) fn refused(error: HangarError) -> HangarError {
    match error {
        HangarError::Api(api)
            if matches!(
                api.code,
                ErrorCode::AccessDenied | ErrorCode::InviteRequired | ErrorCode::InviteInvalid
            ) =>
        {
            HangarError::SignInRefused {
                code: api.code,
                detail: String::new(),
            }
        }
        error => error,
    }
}

/// Stores a new sign-in, replacing any previous one (as `hangar login` does).
pub(crate) fn save_sign_in(
    store: &CredentialStore,
    server: &str,
    tokens: Tokens,
) -> Result<(), HangarError> {
    let _lock = store.lock()?;
    let mut credentials = Credentials {
        server: super::normalize_server(server),
        ..Credentials::default()
    };
    credentials.with_tokens(tokens);
    store.save(&credentials)
}

/// GitHub device flow against `/v1/auth/device`. `show` receives the code to display;
/// polling honours the interval and `slow_down` and stops when `cancelled` turns true.
/// `invite` signs up a new account with an invite code; it is only sent, never kept.
pub(crate) fn sign_in(
    client: &Client,
    store: &CredentialStore,
    invite: Option<&str>,
    mut show: impl FnMut(&DeviceStart),
    cancelled: impl Fn() -> bool,
    now: &Clock,
) -> Result<(), HangarError> {
    let device = client.start_device(invite).map_err(refused)?;
    if device.device_code.is_empty() || device.user_code.is_empty() {
        return Err(HangarError::Invalid("sign-in returned no code".into()));
    }
    show(&device);
    let mut interval = if device.interval == 0 {
        DEFAULT_DEVICE_INTERVAL
    } else {
        Duration::from_secs(device.interval)
    };
    let lifetime = if device.expires_in == 0 {
        DEFAULT_DEVICE_EXPIRY
    } else {
        Duration::from_secs(device.expires_in)
    };
    let deadline = now() + lifetime;
    loop {
        let mut waited = Duration::ZERO;
        while waited < interval {
            if cancelled() {
                return Err(HangarError::Invalid("sign-in cancelled".into()));
            }
            let step = (interval - waited).min(Duration::from_millis(250));
            client.sleep(step);
            waited += step;
        }
        if cancelled() {
            return Err(HangarError::Invalid("sign-in cancelled".into()));
        }
        match poll_device_once(client, &device.device_code)? {
            DevicePoll::Approved(tokens) => return save_sign_in(store, client.server(), tokens),
            DevicePoll::Pending => {}
            DevicePoll::SlowDown => interval += Duration::from_secs(5),
        }
        if now() > deadline {
            return Err(HangarError::Invalid(
                "the sign-in code expired; start sign-in again".into(),
            ));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::api::fake::*;
    use super::*;

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-hangar-{name}-{}-{}",
            std::process::id(),
            crate::client::endpoint::ProfileId::generate()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixed(at: &'static str) -> Clock {
        Arc::new(move || expiry(at).unwrap())
    }

    fn tokens(access: &str, refresh: &str, access_exp: &str) -> serde_json::Value {
        serde_json::json!({
            "accessToken": access, "accessExpiresAt": access_exp,
            "refreshToken": refresh, "refreshExpiresAt": "2026-11-01T00:00:00.123456789Z"
        })
    }

    pub(crate) fn signed_in_store(name: &str, server: &str) -> CredentialStore {
        let store = CredentialStore::at(temp_dir(name));
        let credentials: Credentials = serde_json::from_value(serde_json::json!({
            "server": server, "accessToken": "a1",
            "accessExpiresAt": "2026-10-02T12:00:00.197604092Z",
            "refreshToken": "r1", "refreshExpiresAt": "2026-11-02T04:46:47.197604092Z",
            "futureField": {"kept": true}
        }))
        .unwrap();
        store.save(&credentials).unwrap();
        store
    }

    #[test]
    fn credentials_round_trip_in_the_cli_format() {
        let store = signed_in_store("format", "https://hangar.test");
        let text = std::fs::read_to_string(store.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        for field in [
            "server",
            "accessToken",
            "accessExpiresAt",
            "refreshToken",
            "refreshExpiresAt",
        ] {
            assert!(value.get(field).is_some(), "{field}");
        }
        assert_eq!(value["futureField"]["kept"], true);
        assert_eq!(value["accessExpiresAt"], "2026-10-02T12:00:00.197604092Z");
        assert!(text.ends_with("}\n"));
        assert!(store.lock_path().ends_with(".lock"));
        let empty = Credentials {
            access_token: "x".into(),
            ..Credentials::default()
        };
        store.save(&empty).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(value["accessExpiresAt"], GO_ZERO_TIME);
    }

    #[test]
    fn token_refreshes_thirty_seconds_before_expiry_and_saves_rotation() {
        let store = signed_in_store("skew", "https://hangar.test");
        let http = FakeHttp::new();
        let early = StoredTokens::new(
            store.clone(),
            "https://hangar.test",
            http.clone(),
            fixed("2026-10-02T11:59:00Z"),
        );
        assert_eq!(early.token().unwrap(), "a1");
        assert!(http.sent().is_empty());
        http.reply(200, tokens("a2", "r2", "2026-10-02T13:00:00Z"));
        let late = StoredTokens::new(
            store.clone(),
            "https://hangar.test",
            http.clone(),
            fixed("2026-10-02T11:59:45Z"),
        );
        assert_eq!(late.token().unwrap(), "a2");
        assert_eq!(http.paths(), ["POST /v1/auth/refresh"]);
        let stored = store.load().unwrap().unwrap();
        assert_eq!(stored.refresh_token, "r2");
        assert_eq!(stored.extra["futureField"]["kept"], true);
    }

    #[test]
    fn two_refreshers_rotate_the_token_once() {
        let store = signed_in_store("race", "https://hangar.test");
        let http = FakeHttp::new();
        http.reply(200, tokens("a2", "r2", "2026-10-02T13:00:00Z"));
        let clock = fixed("2026-10-02T12:30:00Z");
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let refresher = StoredTokens::new(
                    store.clone(),
                    "https://hangar.test",
                    http.clone(),
                    clock.clone(),
                );
                std::thread::spawn(move || refresher.token())
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap().unwrap(), "a2");
        }
        assert_eq!(
            http.sent().len(),
            1,
            "a rotated refresh token must not be reused"
        );
    }

    #[test]
    fn rejected_token_refreshes_once_then_requires_sign_in() {
        let store = signed_in_store("401", "https://hangar.test");
        let http = FakeHttp::new();
        let clock = fixed("2026-10-02T11:00:00Z");
        let tokens_source = Arc::new(StoredTokens::new(
            store.clone(),
            "https://hangar.test",
            http.clone(),
            clock,
        ));
        let client =
            Client::new("https://hangar.test", http.clone(), Some(tokens_source)).without_sleep();
        http.error(401, "unauthenticated")
            .reply(200, tokens("a2", "r2", "2026-10-02T13:00:00Z"))
            .reply(200, machine("m_a", "running", true));
        assert_eq!(client.machine("m_a").unwrap().name, "name-m_a");
        let sent = http.sent();
        assert_eq!(sent[0].bearer.as_deref(), Some("a1"));
        assert_eq!(sent[2].bearer.as_deref(), Some("a2"));
        // A second rejection after refreshing is final, and a rejected refresh
        // token means signing in again.
        http.error(401, "unauthenticated")
            .error(401, "unauthenticated");
        let error = client.machine("m_a").unwrap_err();
        assert!(error.needs_sign_in());
        http.error(401, "unauthenticated");
        let store_error = StoredTokens::new(
            store,
            "https://hangar.test",
            http.clone(),
            fixed("2026-10-02T12:59:50Z"),
        )
        .token()
        .unwrap_err();
        assert!(matches!(store_error, HangarError::SessionExpired));
    }

    #[test]
    fn a_different_server_or_missing_file_is_not_signed_in() {
        let store = signed_in_store("server", "https://other.test");
        let http = FakeHttp::new();
        let tokens_source =
            StoredTokens::new(store, "https://hangar.test", http.clone(), system_clock());
        assert!(matches!(
            tokens_source.token(),
            Err(HangarError::WrongServer { .. })
        ));
        let empty = StoredTokens::new(
            CredentialStore::at(temp_dir("missing")),
            "https://hangar.test",
            http,
            system_clock(),
        );
        assert!(matches!(empty.token(), Err(HangarError::NotSignedIn)));
    }

    #[test]
    fn account_status_names_the_user_or_says_why_not() {
        let fallback = "https://default.test";
        let empty = CredentialStore::at(temp_dir("account-empty"));
        let http = FakeHttp::new();
        assert_eq!(
            account_status(&empty, http.clone(), system_clock(), fallback),
            AccountStatus::SignedOut {
                server: fallback.into()
            }
        );
        assert!(http.sent().is_empty(), "no sign-in, no request");
        let store = signed_in_store("account", "https://hangar.test/");
        let clock = fixed("2026-10-02T11:00:00Z");
        http.reply(
            200,
            serde_json::json!({"userId": 7, "login": "octo", "admin": false}),
        );
        let status = account_status(&store, http.clone(), clock.clone(), fallback);
        assert_eq!(
            status,
            AccountStatus::SignedIn {
                server: "https://hangar.test".into(),
                login: "octo".into()
            }
        );
        assert_eq!(
            status.summary(),
            "Signed in as @octo on https://hangar.test."
        );
        assert_eq!(http.paths(), ["GET /v1/me"]);
        assert_eq!(http.sent()[0].bearer.as_deref(), Some("a1"));
        http.error(401, "unauthenticated")
            .error(401, "unauthenticated");
        assert_eq!(
            account_status(&store, http.clone(), clock.clone(), fallback),
            AccountStatus::Expired {
                server: "https://hangar.test".into()
            }
        );
        http.push(Err(super::super::api::TransportError::Failed(
            "offline".into(),
        )));
        assert!(matches!(
            account_status(&store, http, clock, fallback),
            AccountStatus::Unverified { .. }
        ));
    }

    #[test]
    fn sign_out_revokes_then_removes_the_shared_credentials() {
        let store = signed_in_store("logout", "https://hangar.test");
        let http = FakeHttp::new();
        http.push(Ok(super::super::api::HttpResponse {
            status: 204,
            body: Vec::new(),
        }));
        assert_eq!(
            sign_out(&store, http.clone()).unwrap(),
            SignOut::SignedOut {
                server: "https://hangar.test".into(),
                warning: None
            }
        );
        assert_eq!(http.paths(), ["POST /v1/auth/logout"]);
        let sent = http.sent();
        assert_eq!(sent[0].bearer.as_deref(), Some("a1"));
        assert!(sent[0].idempotency_key.is_none());
        assert!(store.load().unwrap().is_none());
        assert!(store.lock_path().exists(), "the CLI's lock file stays");
        assert_eq!(
            sign_out(&store, http.clone()).unwrap(),
            SignOut::NotSignedIn
        );
        // A rejected token is already unusable; other failures are reported but the
        // local sign-in is still removed.
        let store = signed_in_store("logout-401", "https://hangar.test");
        let http = FakeHttp::new();
        http.error(401, "unauthenticated");
        assert!(matches!(
            sign_out(&store, http.clone()).unwrap(),
            SignOut::SignedOut { warning: None, .. }
        ));
        assert_eq!(http.sent().len(), 1, "logout never refreshes first");
        let store = signed_in_store("logout-500", "https://hangar.test");
        let http = FakeHttp::new();
        http.error(500, "internal");
        assert!(matches!(
            sign_out(&store, http).unwrap(),
            SignOut::SignedOut {
                warning: Some(_),
                ..
            }
        ));
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn device_flow_waits_for_approval_and_honours_slow_down() {
        let store = CredentialStore::at(temp_dir("device"));
        let http = FakeHttp::new();
        http.reply(
            200,
            serde_json::json!({"deviceCode": "dc", "userCode": "ABCD-EFGH", "verificationUri": "https://github.com/login/device", "interval": 1, "expiresIn": 900}),
        )
        .error(400, "authorization_pending")
        .error(400, "slow_down")
        .reply(200, tokens("a1", "r1", "2026-10-02T13:00:00Z"));
        let client = Client::new("https://hangar.test", http.clone(), None).without_sleep();
        let mut shown = Vec::new();
        sign_in(
            &client,
            &store,
            None,
            |device| shown.push(device.user_code.clone()),
            || false,
            &system_clock(),
        )
        .unwrap();
        assert_eq!(shown, ["ABCD-EFGH"]);
        let stored = store.load().unwrap().unwrap();
        assert_eq!(stored.server, "https://hangar.test");
        assert_eq!(stored.access_token, "a1");
        assert_eq!(http.sent().len(), 4);
        assert!(http.sent()[1..]
            .iter()
            .all(|request| request.body.as_deref() == Some(r#"{"deviceCode":"dc"}"#)));

        let denied = FakeHttp::new();
        denied
            .reply(
                200,
                serde_json::json!({"deviceCode": "dc", "userCode": "X", "verificationUri": "u", "interval": 1}),
            )
            .error(400, "access_denied");
        let client = Client::new("https://hangar.test", denied, None).without_sleep();
        assert!(sign_in(&client, &store, None, |_| {}, || false, &system_clock()).is_err());
        let cancelled = FakeHttp::new();
        cancelled.reply(
            200,
            serde_json::json!({"deviceCode": "dc", "userCode": "X", "verificationUri": "u"}),
        );
        let client = Client::new("https://hangar.test", cancelled.clone(), None).without_sleep();
        assert!(sign_in(&client, &store, None, |_| {}, || true, &system_clock()).is_err());
        assert_eq!(cancelled.sent().len(), 1);
    }
}
