//! Sign-in to hangar: the browser (loopback) flow by default, the device code flow
//! as fallback.
//!
//! The browser flow follows RFC 8252 like `hangar login`: listen on an ephemeral port
//! of literal `127.0.0.1`, open `/auth/cli/start` with a random `state` and a PKCE S256
//! challenge, wait for one redirect to `/callback`, check `state`, and exchange the code
//! with the verifier. Anything that goes wrong before the browser opens (no display,
//! SSH session, a server without browser sign-in, no free port) falls back to the
//! device code. Everything here blocks and runs on worker threads.
//!
//! Sign-up is the same flow with an invite code: `&invite=` on the start URL, or
//! `invite` in the device start request. The code is never logged or stored.
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::api::{Client, DeviceStart, ErrorCode, HangarError};
use super::auth::{save_sign_in, Clock, CredentialStore};

/// How long the browser sign-in waits for the redirect (same as the CLI).
pub(crate) const BROWSER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// Bounds a connection that never sends its request (browser pre-connects).
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// What the user is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SignInStep {
    /// The browser was opened at `url`; waiting for its redirect.
    Browser { url: String },
    /// Device code sign-in; `reason` says why the browser sign-in was not used.
    Device {
        device: DeviceStart,
        reason: Option<String>,
    },
}

impl SignInStep {
    /// Dialog text for this step.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Browser { url } => format!(
                "Continue in your browser to sign in to hangar. Esc cancels. If no browser opened, visit {url}"
            ),
            Self::Device { device, reason } => {
                let reason = reason
                    .as_deref()
                    .map(|reason| format!("Using a sign-in code: {reason}. "))
                    .unwrap_or_default();
                format!(
                    "{reason}Open {} and enter the code {}. Waiting for approval…",
                    device.verification_uri, device.user_code
                )
            }
        }
    }
}

/// How this process can show a page to the user.
pub(crate) struct Browser<'a> {
    /// Why no browser can be used here (SSH session, no display), or `None`.
    pub unavailable: Option<String>,
    /// Opens a URL in the user's browser.
    pub open: &'a dyn Fn(&str) -> Result<(), String>,
}

/// Why no local browser can be used, from the environment. Mirrors `hangar login`,
/// and also treats `SSH_TTY` as an SSH session.
pub(crate) fn browser_unavailable(
    var: impl Fn(&str) -> Option<String>,
    needs_display: bool,
) -> Option<String> {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    let display = set("DISPLAY") || set("WAYLAND_DISPLAY");
    if (set("SSH_CONNECTION") || set("SSH_TTY")) && !display {
        Some("this is an SSH session without a display".into())
    } else if needs_display && !display {
        Some("no display (DISPLAY/WAYLAND_DISPLAY unset)".into())
    } else {
        None
    }
}

enum Attempt {
    /// The browser was not opened; use the device code.
    Fallback(String),
    Failed(HangarError),
}

fn cancelled_error() -> HangarError {
    HangarError::Invalid("sign-in cancelled".into())
}

/// The invite code prefix (`hgi_` and 20 base32 characters).
const INVITE_PREFIX: &str = "hgi_";
const INVITE_BODY_LEN: usize = 20;

/// Trims an entered invite code and checks its shape; the server decides whether it
/// is valid. `Err` is a message for the user.
pub(crate) fn normalize_invite(code: &str) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() {
        return Err("Enter the invite code you received.".into());
    }
    let body = code.strip_prefix(INVITE_PREFIX).unwrap_or_default();
    if body.len() != INVITE_BODY_LEN || !body.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(format!(
            "That doesn't look like an invite code: they start with {INVITE_PREFIX} followed by {INVITE_BODY_LEN} letters and digits."
        ));
    }
    Ok(code.to_owned())
}

/// Why a sign-in or sign-up failed, as far as invite codes are concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InviteProblem {
    /// The account is not on hangar and no invite code was given.
    Required,
    /// The invite code is unknown, expired, revoked or used up.
    Invalid,
    /// An invite code was sent and the sign-in was refused without saying why: a
    /// server without invite codes, or one that did not accept it.
    NotAccepted,
}

/// Classifies a failed sign-in; `invite_sent` says whether it carried an invite code.
pub(crate) fn invite_problem(error: &HangarError, invite_sent: bool) -> Option<InviteProblem> {
    match error.code()? {
        ErrorCode::InviteRequired => Some(InviteProblem::Required),
        ErrorCode::InviteInvalid => Some(InviteProblem::Invalid),
        ErrorCode::AccessDenied | ErrorCode::PermissionDenied if invite_sent => {
            Some(InviteProblem::NotAccepted)
        }
        _ => None,
    }
}

/// Signs in and stores the tokens in the shared credentials, replacing any previous
/// sign-in. `invite` signs up a new account with an invite code. `notify` receives
/// each step to show; `cancelled` is polled while waiting.
pub(crate) fn sign_in(
    client: &Client,
    store: &CredentialStore,
    browser: &Browser<'_>,
    invite: Option<&str>,
    mut notify: impl FnMut(SignInStep),
    cancelled: impl Fn() -> bool,
    now: &Clock,
) -> Result<(), HangarError> {
    sign_in_with_timeout(
        client,
        store,
        browser,
        invite,
        &mut notify,
        &cancelled,
        now,
        BROWSER_TIMEOUT,
    )
}

#[allow(clippy::too_many_arguments)] // The public entry point fixes the timeout.
fn sign_in_with_timeout(
    client: &Client,
    store: &CredentialStore,
    browser: &Browser<'_>,
    invite: Option<&str>,
    notify: &mut dyn FnMut(SignInStep),
    cancelled: &dyn Fn() -> bool,
    now: &Clock,
    timeout: Duration,
) -> Result<(), HangarError> {
    let reason = match &browser.unavailable {
        Some(reason) => reason.clone(),
        None => match browser_sign_in(
            client,
            store,
            browser.open,
            invite,
            notify,
            cancelled,
            timeout,
        ) {
            Ok(()) => return Ok(()),
            Err(Attempt::Failed(error)) => return Err(error),
            Err(Attempt::Fallback(reason)) => {
                tracing::debug!(%reason, "using the hangar device code sign-in");
                reason
            }
        },
    };
    if cancelled() {
        return Err(cancelled_error());
    }
    let can_open = browser.unavailable.is_none();
    super::auth::sign_in(
        client,
        store,
        invite,
        |device| {
            notify(SignInStep::Device {
                device: device.clone(),
                reason: Some(reason.clone()),
            });
            if can_open {
                if let Err(error) = (browser.open)(&device.verification_uri) {
                    tracing::debug!(%error, "could not open the device sign-in page");
                }
            }
        },
        cancelled,
        now,
    )
}

fn random_url_safe(bytes: usize) -> std::io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    crate::platform::fill_random(&mut buffer)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer))
}

/// base64url(sha256(verifier)) without padding.
pub(crate) fn code_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn browser_sign_in(
    client: &Client,
    store: &CredentialStore,
    open: &dyn Fn(&str) -> Result<(), String>,
    invite: Option<&str>,
    notify: &mut dyn FnMut(SignInStep),
    cancelled: &dyn Fn() -> bool,
    timeout: Duration,
) -> Result<(), Attempt> {
    client.probe_cli_login().map_err(Attempt::Fallback)?;
    if cancelled() {
        return Err(Attempt::Failed(cancelled_error()));
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
        .map_err(|error| Attempt::Fallback(format!("cannot listen on 127.0.0.1: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| Attempt::Fallback(format!("cannot listen on 127.0.0.1: {error}")))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let (verifier, state) = random_url_safe(32)
        .and_then(|verifier| Ok((verifier, random_url_safe(24)?)))
        .map_err(|error| Attempt::Fallback(format!("no secure random numbers: {error}")))?;
    let url = client.cli_start_url(&redirect, &state, &code_challenge(&verifier), invite);
    open(&url).map_err(|error| Attempt::Fallback(format!("cannot open a browser: {error}")))?;
    notify(SignInStep::Browser { url });
    let deadline = Instant::now() + timeout;
    loop {
        if cancelled() {
            return Err(Attempt::Failed(cancelled_error()));
        }
        if Instant::now() >= deadline {
            return Err(Attempt::Failed(HangarError::Invalid(
                "timed out waiting for the browser sign-in; start sign-in again".into(),
            )));
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(Attempt::Failed(HangarError::Invalid(format!(
                    "the sign-in listener failed: {error}"
                ))))
            }
        };
        let Some((mut stream, callback)) = read_callback(stream, &state) else {
            continue;
        };
        let result = match callback {
            Callback::Code(code) => client
                .exchange_cli_code(&code, &verifier, &redirect)
                .map_err(exchange_error)
                .and_then(|tokens| save_sign_in(store, client.server(), tokens)),
            Callback::Error(error) => Err(error),
        };
        let page = match &result {
            Ok(()) => page(
                200,
                "Signed in to hangar",
                "You can close this tab and return to Herdr.",
            ),
            Err(error) => page(
                200,
                "Sign-in failed",
                &format!("{error}. Return to Herdr to try again."),
            ),
        };
        let _ = stream.write_all(page.as_bytes());
        return result.map_err(Attempt::Failed);
    }
}

fn exchange_error(error: HangarError) -> HangarError {
    match error.code() {
        Some(ErrorCode::InvalidGrant) => HangarError::Invalid(
            "the sign-in code expired or was already used; start sign-in again".into(),
        ),
        _ => error,
    }
}

enum Callback {
    Code(String),
    Error(HangarError),
}

/// Reads one request. Requests that are not this sign-in's callback get an answer
/// and `None`, so the listener keeps waiting (stale tabs, favicon, other clients).
fn read_callback(mut stream: TcpStream, state: &str) -> Option<(TcpStream, Callback)> {
    // Accepted sockets inherit non-blocking mode on some platforms.
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(READ_TIMEOUT)).ok()?;
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => request.extend_from_slice(&chunk[..read]),
        }
        if request.len() > MAX_REQUEST_BYTES {
            let _ = stream.write_all(page(431, "Bad request", "Request too large.").as_bytes());
            return None;
        }
    }
    let line = String::from_utf8_lossy(&request);
    let mut parts = line.lines().next().unwrap_or_default().split(' ');
    let (method, target) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        let _ = stream.write_all(page(404, "Not found", "Nothing here.").as_bytes());
        return None;
    }
    if method != "GET" {
        let _ = stream.write_all(page(405, "Not allowed", "Use GET.").as_bytes());
        return None;
    }
    let params = parse_query(query);
    let param = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    };
    if !constant_time_eq(param("state").as_bytes(), state.as_bytes()) {
        let _ = stream.write_all(
            page(
                400,
                "Sign-in failed",
                "The sign-in state did not match. Start sign-in again from Herdr.",
            )
            .as_bytes(),
        );
        return None;
    }
    let callback = match (param("error"), param("code")) {
        (code @ ("access_denied" | "invite_required" | "invite_invalid"), _) => {
            Callback::Error(HangarError::SignInRefused {
                code: ErrorCode::parse(code),
                detail: describe("", param("error_description")),
            })
        }
        ("", "") => Callback::Error(HangarError::Invalid("sign-in returned no code".into())),
        ("", code) => Callback::Code(code.to_owned()),
        (error, _) => Callback::Error(HangarError::Invalid(describe(
            &format!("sign-in failed ({error})"),
            param("error_description"),
        ))),
    };
    Some((stream, callback))
}

/// `summary: description` with the server's description cleaned and bounded; just
/// the description when `summary` is empty.
fn describe(summary: &str, description: &str) -> String {
    let description: String = description
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    if description.is_empty() {
        summary.to_owned()
    } else if summary.is_empty() {
        description
    } else {
        format!("{summary}: {description}")
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = text
                    .get(index + 1..index + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

/// A complete HTTP response with a small page.
fn page(status: u16, title: &str, message: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Request Header Fields Too Large",
    };
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title} · herdr</title>\n<style>body{{font:16px system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1rem;color:#222}}\n@media (prefers-color-scheme:dark){{body{{background:#111;color:#ddd}}}}</style></head>\n<body><h1>{title}</h1><p>{message}</p></body></html>\n",
        title = html_escape(title),
        message = html_escape(message),
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::super::api::fake::*;
    use super::super::auth::{system_clock, tests::temp_dir};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn tokens() -> serde_json::Value {
        serde_json::json!({
            "accessToken": "a1", "accessExpiresAt": "2026-10-02T13:00:00Z",
            "refreshToken": "r1", "refreshExpiresAt": "2026-11-01T00:00:00Z"
        })
    }

    fn query_param(url: &str, name: &str) -> String {
        let (_, query) = url.split_once('?').unwrap();
        parse_query(query)
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
            .unwrap()
    }

    /// Sends one GET to the loopback listener and returns the raw response.
    fn get(url: &str) -> String {
        let rest = url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_once('/').unwrap();
        let mut stream = TcpStream::connect(host).unwrap();
        write!(stream, "GET /{path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// A "browser" that follows the start URL like hangar would: it records the URL
    /// and, on a thread, sends the given callback query to the redirect URI.
    fn browser(
        opened: Arc<Mutex<Vec<String>>>,
        callback: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
        pages: Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(&str) -> Result<(), String> {
        let callback = Arc::new(callback);
        move |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            let redirect = query_param(url, "redirect_uri");
            let state = query_param(url, "state");
            let callback = callback.clone();
            let pages = pages.clone();
            std::thread::spawn(move || {
                for query in callback(&state) {
                    let page = get(&format!("{redirect}?{query}"));
                    pages.lock().unwrap().push(page);
                }
            });
            Ok(())
        }
    }

    fn run(
        http: &Arc<FakeHttp>,
        store: &CredentialStore,
        open: &dyn Fn(&str) -> Result<(), String>,
        unavailable: Option<String>,
        steps: &mut Vec<SignInStep>,
        cancelled: &dyn Fn() -> bool,
        timeout: Duration,
    ) -> Result<(), HangarError> {
        run_with_invite(
            http,
            store,
            open,
            unavailable,
            None,
            steps,
            cancelled,
            timeout,
        )
    }

    #[allow(clippy::too_many_arguments)] // Mirrors sign_in_with_timeout.
    fn run_with_invite(
        http: &Arc<FakeHttp>,
        store: &CredentialStore,
        open: &dyn Fn(&str) -> Result<(), String>,
        unavailable: Option<String>,
        invite: Option<&str>,
        steps: &mut Vec<SignInStep>,
        cancelled: &dyn Fn() -> bool,
        timeout: Duration,
    ) -> Result<(), HangarError> {
        let client = Client::new("https://hangar.test", http.clone(), None).without_sleep();
        let browser = Browser { unavailable, open };
        sign_in_with_timeout(
            &client,
            store,
            &browser,
            invite,
            &mut |step| steps.push(step),
            cancelled,
            &system_clock(),
            timeout,
        )
    }

    #[test]
    fn browser_sign_in_checks_state_and_exchanges_the_code_with_the_verifier() {
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({})).reply(200, tokens());
        let store = CredentialStore::at(temp_dir("loopback"));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let pages = Arc::new(Mutex::new(Vec::new()));
        let open = browser(
            opened.clone(),
            |state| {
                vec![
                    "code=hgc_stale&state=wrong-state-value".into(),
                    format!("code=hgc_1&state={state}"),
                ]
            },
            pages.clone(),
        );
        let mut steps = Vec::new();
        run(
            &http,
            &store,
            &open,
            None,
            &mut steps,
            &|| false,
            Duration::from_secs(20),
        )
        .unwrap();
        let url = opened.lock().unwrap()[0].clone();
        assert!(url.starts_with(
            "https://hangar.test/auth/cli/start?redirect_uri=http%3A%2F%2F127.0.0.1%3A"
        ));
        assert_eq!(query_param(&url, "code_challenge_method"), "S256");
        let redirect = query_param(&url, "redirect_uri");
        assert!(redirect.ends_with("/callback"));
        assert!(query_param(&url, "state").len() >= 16);
        assert_eq!(steps, [SignInStep::Browser { url: url.clone() }]);
        assert_eq!(
            http.paths(),
            ["GET /auth/cli/start", "POST /v1/auth/cli/token"]
        );
        let sent = http.sent();
        assert!(sent.iter().all(|request| request.bearer.is_none()));
        let body: serde_json::Value =
            serde_json::from_str(sent[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["code"], "hgc_1");
        assert_eq!(body["redirectUri"], redirect);
        let verifier = body["codeVerifier"].as_str().unwrap();
        assert!((43..=128).contains(&verifier.len()));
        assert_eq!(
            code_challenge(verifier),
            query_param(&url, "code_challenge")
        );
        let stored = store.load().unwrap().unwrap();
        assert_eq!(stored.server, "https://hangar.test");
        assert_eq!(stored.access_token, "a1");
        // Wait for the browser thread to read both pages.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pages.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pages = pages.lock().unwrap();
        assert!(pages[0].starts_with("HTTP/1.1 400"));
        assert!(pages[1].starts_with("HTTP/1.1 200"));
        assert!(pages[1].contains("Signed in to hangar"));
    }

    #[test]
    fn denied_browser_sign_in_reports_the_reason_without_exchanging() {
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({}));
        let store = CredentialStore::at(temp_dir("denied"));
        let pages = Arc::new(Mutex::new(Vec::new()));
        let open = browser(
            Arc::new(Mutex::new(Vec::new())),
            |state| {
                vec![format!(
                    "error=access_denied&error_description=%3Cb%3Enot+allowed%3C%2Fb%3E&state={state}"
                )]
            },
            pages.clone(),
        );
        let error = run(
            &http,
            &store,
            &open,
            None,
            &mut Vec::new(),
            &|| false,
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "hangar: sign-in was denied: <b>not allowed</b>"
        );
        assert_eq!(http.sent().len(), 1, "a denied sign-in is never exchanged");
        assert!(store.load().unwrap().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while pages.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let page = pages.lock().unwrap()[0].clone();
        assert!(page.contains("&lt;b&gt;not allowed&lt;/b&gt;"), "{page}");
    }

    #[test]
    fn a_rejected_code_says_to_start_again() {
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({}))
            .error(400, "invalid_grant");
        let store = CredentialStore::at(temp_dir("grant"));
        let open = browser(
            Arc::new(Mutex::new(Vec::new())),
            |state| vec![format!("code=hgc_1&state={state}")],
            Arc::new(Mutex::new(Vec::new())),
        );
        let error = run(
            &http,
            &store,
            &open,
            None,
            &mut Vec::new(),
            &|| false,
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(error.to_string().contains("start sign-in again"));
        assert_eq!(http.sent().len(), 2, "codes are single use: never retried");
    }

    fn device_replies(http: &FakeHttp) {
        http.reply(
            200,
            serde_json::json!({"deviceCode": "dc", "userCode": "ABCD-EFGH", "verificationUri": "https://github.com/login/device", "interval": 1}),
        )
        .reply(200, tokens());
    }

    #[test]
    fn servers_without_browser_sign_in_fall_back_to_the_device_code() {
        for status in [404, 503] {
            let http = FakeHttp::new();
            http.reply(status, serde_json::json!({}));
            device_replies(&http);
            let store = CredentialStore::at(temp_dir("fallback"));
            let opened = Arc::new(Mutex::new(Vec::new()));
            let record = opened.clone();
            let open = move |url: &str| {
                record.lock().unwrap().push(url.to_owned());
                Ok(())
            };
            let mut steps = Vec::new();
            run(
                &http,
                &store,
                &open,
                None,
                &mut steps,
                &|| false,
                Duration::from_secs(5),
            )
            .unwrap();
            let [SignInStep::Device { device, reason }] = steps.as_slice() else {
                panic!("{steps:?}");
            };
            assert_eq!(device.user_code, "ABCD-EFGH");
            assert_eq!(
                reason.as_deref(),
                Some(format!("the server does not offer browser sign-in (HTTP {status})").as_str())
            );
            // The device page still opens when a browser is available.
            assert_eq!(*opened.lock().unwrap(), ["https://github.com/login/device"]);
            assert_eq!(
                http.paths(),
                [
                    "GET /auth/cli/start",
                    "POST /v1/auth/device",
                    "POST /v1/auth/device/token"
                ]
            );
            assert_eq!(store.load().unwrap().unwrap().access_token, "a1");
        }
    }

    #[test]
    fn no_browser_uses_the_device_code_without_probing_or_opening() {
        let http = FakeHttp::new();
        device_replies(&http);
        let store = CredentialStore::at(temp_dir("ssh"));
        let open = |_: &str| -> Result<(), String> { panic!("must not open a browser") };
        let mut steps = Vec::new();
        run(
            &http,
            &store,
            &open,
            Some("this is an SSH session without a display".into()),
            &mut steps,
            &|| false,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            http.paths(),
            ["POST /v1/auth/device", "POST /v1/auth/device/token"]
        );
        assert!(steps[0].message().starts_with(
            "Using a sign-in code: this is an SSH session without a display. Open https://github.com/login/device and enter the code ABCD-EFGH."
        ));
        // A browser that cannot be opened also falls back.
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({}));
        device_replies(&http);
        let open = |_: &str| -> Result<(), String> { Err("no opener".into()) };
        let mut steps = Vec::new();
        run(
            &http,
            &store,
            &open,
            None,
            &mut steps,
            &|| false,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(
            &steps[0],
            SignInStep::Device { reason: Some(reason), .. } if reason == "cannot open a browser: no opener"
        ));
    }

    #[test]
    fn browser_wait_is_cancellable_and_bounded() {
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({}));
        let store = CredentialStore::at(temp_dir("cancel"));
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let open = move |_: &str| {
            flag.store(true, Ordering::Release);
            Ok(())
        };
        let error = run(
            &http,
            &store,
            &open,
            None,
            &mut Vec::new(),
            &|| cancel.load(Ordering::Acquire),
            Duration::from_secs(60),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "hangar: sign-in cancelled");
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({}));
        let open = |_: &str| Ok(());
        let error = run(
            &http,
            &store,
            &open,
            None,
            &mut Vec::new(),
            &|| false,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(
            http.sent().len(),
            1,
            "a timed out sign-in exchanges nothing"
        );
    }

    #[test]
    fn ssh_sessions_and_missing_displays_disable_the_browser() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert!(browser_unavailable(env(&[("SSH_CONNECTION", "1 2 3 4")]), false).is_some());
        assert!(browser_unavailable(env(&[("SSH_TTY", "/dev/ttys001")]), false).is_some());
        assert!(
            browser_unavailable(env(&[("SSH_TTY", ""), ("SSH_CONNECTION", "")]), false).is_none()
        );
        assert!(
            browser_unavailable(env(&[("SSH_TTY", "/dev/pts/1"), ("DISPLAY", ":10")]), true)
                .is_none()
        );
        assert!(browser_unavailable(env(&[]), true).is_some());
        assert!(browser_unavailable(env(&[("WAYLAND_DISPLAY", "wayland-0")]), true).is_none());
        assert!(browser_unavailable(env(&[]), false).is_none());
    }

    #[test]
    fn query_decoding_and_challenge_match_rfc_7636() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(
            parse_query("a=1%2B2&b=x+y&c=%zz&d"),
            [
                ("a".to_owned(), "1+2".to_owned()),
                ("b".to_owned(), "x y".to_owned()),
                ("c".to_owned(), "%zz".to_owned()),
                ("d".to_owned(), String::new()),
            ]
        );
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
    const INVITE: &str = "hgi_ABCDEFGHIJKLMNOPQRST";

    /// Captures every tracing event of the current thread, at any level.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn traced<T>(work: impl FnOnce() -> T) -> (T, String) {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, work);
        let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
        (result, log)
    }

    /// Every file the credential store left behind, concatenated.
    fn stored_files(dir: &std::path::Path) -> String {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    #[test]
    fn sign_up_sends_the_invite_in_the_start_url_and_never_keeps_it() {
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({})).reply(200, tokens());
        let dir = temp_dir("signup");
        let store = CredentialStore::at(dir.clone());
        let opened = Arc::new(Mutex::new(Vec::new()));
        let open = browser(
            opened.clone(),
            |state| vec![format!("code=hgc_1&state={state}")],
            Arc::new(Mutex::new(Vec::new())),
        );
        let (result, log) = traced(|| {
            run_with_invite(
                &http,
                &store,
                &open,
                None,
                Some(INVITE),
                &mut Vec::new(),
                &|| false,
                Duration::from_secs(20),
            )
        });
        result.unwrap();
        let url = opened.lock().unwrap()[0].clone();
        assert_eq!(query_param(&url, "invite"), INVITE);
        assert!(url.ends_with(&format!("&invite={INVITE}")), "{url}");
        // The exchange itself does not carry the invite.
        let exchange = http.sent().last().unwrap().body.clone().unwrap();
        assert!(!exchange.contains(INVITE), "{exchange}");
        assert!(!log.contains(INVITE), "{log}");
        let files = stored_files(&dir);
        assert!(files.contains("\"a1\""), "{files}");
        assert!(!files.contains(INVITE), "{files}");
        assert!(!files.contains("hgi_"), "{files}");
    }

    /// Runs a browser sign-in whose callback returns `error`; returns the error.
    fn browser_error(error: &'static str, invite: Option<&str>) -> HangarError {
        let http = FakeHttp::new();
        http.reply(400, serde_json::json!({}));
        let dir = temp_dir("invite-error");
        let store = CredentialStore::at(dir.clone());
        let open = browser(
            Arc::new(Mutex::new(Vec::new())),
            move |state| vec![format!("error={error}&state={state}")],
            Arc::new(Mutex::new(Vec::new())),
        );
        let (result, log) = traced(|| {
            run_with_invite(
                &http,
                &store,
                &open,
                None,
                invite,
                &mut Vec::new(),
                &|| false,
                Duration::from_secs(20),
            )
        });
        assert_eq!(http.sent().len(), 1, "a refused sign-in is never exchanged");
        assert!(store.load().unwrap().is_none());
        assert!(!log.contains(INVITE), "{log}");
        assert!(!stored_files(&dir).contains(INVITE));
        result.unwrap_err()
    }

    #[test]
    fn browser_callback_errors_map_to_invite_problems() {
        let required = browser_error("invite_required", None);
        assert_eq!(
            invite_problem(&required, false),
            Some(InviteProblem::Required)
        );
        assert!(required.to_string().contains("isn't on hangar yet"));
        let invalid = browser_error("invite_invalid", Some(INVITE));
        assert_eq!(invite_problem(&invalid, true), Some(InviteProblem::Invalid));
        assert!(!invalid.to_string().contains(INVITE));
        // A server without invite codes ignores the invite and denies the sign-in.
        let denied = browser_error("access_denied", Some(INVITE));
        assert_eq!(
            invite_problem(&denied, true),
            Some(InviteProblem::NotAccepted)
        );
        // Without an invite a denial is just a denial.
        let denied = browser_error("access_denied", None);
        assert_eq!(invite_problem(&denied, false), None);
        assert_eq!(denied.to_string(), "hangar: sign-in was denied");
        let failed = browser_error("server_error", Some(INVITE));
        assert_eq!(invite_problem(&failed, true), None);
        assert_eq!(failed.to_string(), "hangar: sign-in failed (server_error)");
    }

    /// Runs a device sign-up whose first poll fails with `code`.
    fn device_error(
        code: &str,
        invite: Option<&str>,
    ) -> (HangarError, Vec<super::super::api::HttpRequest>) {
        let http = FakeHttp::new();
        // No browser sign-in on this server: the fallback is logged.
        http.reply(404, serde_json::json!({}));
        http.reply(
            200,
            serde_json::json!({"deviceCode": "dc", "userCode": "ABCD-EFGH", "verificationUri": "https://github.com/login/device", "interval": 1}),
        )
        .error(400, code);
        let dir = temp_dir("device-invite");
        let store = CredentialStore::at(dir.clone());
        let open = |_: &str| -> Result<(), String> { Ok(()) };
        let (result, log) = traced(|| {
            run_with_invite(
                &http,
                &store,
                &open,
                None,
                invite,
                &mut Vec::new(),
                &|| false,
                Duration::from_secs(5),
            )
        });
        assert!(
            log.contains("using the hangar device code sign-in"),
            "{log}"
        );
        assert!(!log.contains(INVITE), "{log}");
        assert!(!stored_files(&dir).contains(INVITE));
        // Skip the browser probe.
        (result.unwrap_err(), http.sent()[1..].to_vec())
    }

    #[test]
    fn device_sign_up_sends_the_invite_and_maps_poll_errors() {
        let (error, sent) = device_error("invite_invalid", Some(INVITE));
        assert_eq!(
            sent[0].body.as_deref(),
            Some(r#"{"invite":"hgi_ABCDEFGHIJKLMNOPQRST"}"#)
        );
        assert_eq!(sent[1].body.as_deref(), Some(r#"{"deviceCode":"dc"}"#));
        assert_eq!(invite_problem(&error, true), Some(InviteProblem::Invalid));
        let (error, sent) = device_error("invite_required", None);
        assert_eq!(sent[0].body, None, "a plain sign-in sends no invite");
        assert_eq!(invite_problem(&error, false), Some(InviteProblem::Required));
        let (error, _) = device_error("access_denied", Some(INVITE));
        assert_eq!(
            invite_problem(&error, true),
            Some(InviteProblem::NotAccepted)
        );
        let (error, _) = device_error("permission_denied", Some(INVITE));
        assert_eq!(
            invite_problem(&error, true),
            Some(InviteProblem::NotAccepted)
        );
        let (error, _) = device_error("access_denied", None);
        assert_eq!(invite_problem(&error, false), None);
        assert_eq!(error.to_string(), "hangar: sign-in was denied");
    }

    #[test]
    fn invite_codes_are_trimmed_and_checked_lightly() {
        assert_eq!(
            normalize_invite(&format!("  {INVITE}\n")).as_deref(),
            Ok(INVITE)
        );
        assert_eq!(
            normalize_invite("   "),
            Err("Enter the invite code you received.".into())
        );
        for wrong in [
            "ABCDEFGHIJKLMNOPQRST",
            "hgi_ABCDEFGHIJKLMNOPQRS",
            "hgi_ABCDEFGHIJKLMNOPQRSTU",
            "hgx_ABCDEFGHIJKLMNOPQRST",
            "hgi_ABCDEFGHIJ-LMNOPQRST",
        ] {
            let error = normalize_invite(wrong).unwrap_err();
            assert!(error.contains("start with hgi_"), "{wrong}: {error}");
        }
    }
}
