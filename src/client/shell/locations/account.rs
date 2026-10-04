//! The Account view of Settings → Remotes: who is signed in, Sign in (in the browser,
//! or with a device code), Sign up with invite code… and Sign out…. Herdr shares the
//! sign-in with the hangar CLI. HTTP runs in workers; results carry the dialog epoch so
//! a late result never overrides a newer dialog.
//!
//! An invite code lives only in the sign-up form's field and the worker that sends it;
//! it is never logged or written anywhere.
use super::view::{AccountAction, RemotesTab};
use super::*;
use crate::hangar::api::HangarError;
use crate::hangar::login::{InviteProblem, SignInStep};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(in crate::client::shell) const SIGN_UP_NOTE: &str = "hangar is invite-only. Enter the invite code you received, then approve with GitHub in your browser. Your GitHub account becomes your hangar account.";
pub(in crate::client::shell) const INVITE_REQUIRED: &str =
    "This GitHub account isn't on hangar yet. Use Sign up with invite code…";
pub(in crate::client::shell) const INVITE_INVALID: &str =
    "That invite code isn't valid (it may be expired, revoked or already used).";
pub(in crate::client::shell) const SIGN_UP_NOT_ACCEPTED: &str =
    "Sign-up isn't available on this server, or the code was not accepted.";

pub(in crate::client::shell) const SHARED_NOTE: &str = "Herdr and the hangar CLI share this sign-in (~/.config/hangar/credentials.json). Sign in opens your browser; over SSH or without a browser it shows a code to enter instead.";

/// Shown at the right of the sub-view switcher.
pub(in crate::client::shell) fn badge(status: Option<&AccountStatus>) -> String {
    match status {
        Some(AccountStatus::SignedIn { login, .. }) => format!("@{login}"),
        Some(AccountStatus::SignedOut { .. }) => "not signed in".into(),
        Some(AccountStatus::Expired { .. }) => "sign-in expired".into(),
        Some(AccountStatus::Unverified { .. }) | None => String::new(),
    }
}

fn sign_out_confirmation(status: Option<&AccountStatus>) -> String {
    let server = match status {
        Some(
            AccountStatus::SignedIn { server, .. }
            | AccountStatus::Expired { server }
            | AccountStatus::Unverified { server, .. },
        ) => format!(" on {server}"),
        _ => String::new(),
    };
    format!("Sign out of hangar{server}? This revokes the sign-in on the server and removes ~/.config/hangar/credentials.json, which the hangar CLI shares, so `hangar` commands are signed out too. hangar remotes cannot start, stop or reconnect until you sign in again. Machines keep running.")
}

/// A failed sign-in or sign-up: the error text and, when it is about invite codes,
/// which problem.
#[derive(Debug)]
struct SignInFailure {
    message: String,
    invite: Option<InviteProblem>,
}

enum AccountEvent {
    Step(SignInStep),
    Finished(Result<(), SignInFailure>),
}

/// Signs in, with an invite code to sign up.
type SignIn =
    fn(Option<&str>, &mut dyn FnMut(SignInStep), &dyn Fn() -> bool) -> Result<(), HangarError>;

/// The sign-up form's text: what went wrong, if anything, then the explanation.
fn sign_up_message(problem: &str) -> String {
    if problem.is_empty() {
        SIGN_UP_NOTE.to_owned()
    } else {
        format!("{problem}\n\n{SIGN_UP_NOTE}")
    }
}

#[derive(Default)]
pub(super) struct AccountController {
    status: Option<(u64, mpsc::Receiver<AccountStatus>)>,
    /// The last status read, shown while it is checked again.
    last: Option<AccountStatus>,
    sign_in: Option<(u64, mpsc::Receiver<AccountEvent>)>,
    cancel: Option<Arc<AtomicBool>>,
    /// Replace the hangar requests in tests.
    status_reader: Option<fn() -> AccountStatus>,
    signer: Option<SignIn>,
}

impl std::fmt::Debug for AccountController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountController")
            .field("signing_in", &self.sign_in.is_some())
            .finish()
    }
}

impl AccountController {
    pub fn last_status(&self) -> Option<AccountStatus> {
        self.last.clone()
    }

    pub fn cancel_sign_in(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.sign_in = None;
    }
}

impl ClientShellState {
    /// Reads who is signed in on a worker.
    pub(super) fn refresh_account(&mut self) {
        let reader = self
            .locations
            .account
            .status_reader
            .unwrap_or(backend::hangar::account_status);
        let (send, receive) = mpsc::channel();
        self.locations.account.status = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let _ = send.send(reader());
        });
    }

    pub(in crate::client::shell) fn run_account_action(&mut self, action: AccountAction) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        match action {
            AccountAction::SignIn | AccountAction::SwitchAccount => {
                self.start_account_sign_in(None)
            }
            AccountAction::SignUp => {
                if self.locations.account.sign_in.is_some() {
                    return;
                }
                dialog.kind = LocationDialogKind::SignUp;
                dialog.fields = vec![TextEditor::new("", false)];
                dialog.selected = 0;
                dialog.message = sign_up_message("");
            }
            AccountAction::SignOut => match &dialog.account {
                Some(status) if !status.signed_in() => {
                    dialog.message = "Not signed in to hangar.".into();
                }
                status => {
                    dialog.message = sign_out_confirmation(status.as_ref());
                    dialog.kind = LocationDialogKind::SignOut;
                    dialog.selected = 0;
                }
            },
        }
    }

    /// ↵ in the sign-up form: checks the code's shape, then signs in with it.
    pub(in crate::client::shell) fn submit_sign_up(&mut self) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        if !matches!(dialog.kind, LocationDialogKind::SignUp) || dialog.busy {
            return;
        }
        let entered = dialog
            .fields
            .first()
            .map(|field| field.as_str())
            .unwrap_or_default();
        match crate::hangar::login::normalize_invite(entered) {
            Ok(invite) => self.start_account_sign_in(Some(invite)),
            Err(problem) => {
                dialog.selected = 0;
                dialog.message = sign_up_message(&problem);
            }
        }
    }

    fn start_account_sign_in(&mut self, invite: Option<String>) {
        if self.locations.account.sign_in.is_some() {
            return;
        }
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        dialog.busy = true;
        dialog.message = if invite.is_some() {
            "Starting sign-up…".into()
        } else {
            "Starting sign-in…".into()
        };
        let signer = self
            .locations
            .account
            .signer
            .unwrap_or(backend::hangar::sign_in);
        let cancel = Arc::new(AtomicBool::new(false));
        self.locations.account.cancel = Some(cancel.clone());
        let (send, receive) = mpsc::channel();
        self.locations.account.sign_in = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let result = signer(
                invite.as_deref(),
                &mut |step| {
                    let _ = send.send(AccountEvent::Step(step));
                },
                &|| cancel.load(Ordering::Acquire),
            );
            let result = result.map_err(|error| SignInFailure {
                invite: crate::hangar::login::invite_problem(&error, invite.is_some()),
                message: error.to_string(),
            });
            let _ = send.send(AccountEvent::Finished(result));
        });
    }

    pub(super) fn tick_account(&mut self, outcome: &mut ClientShellInput) {
        let status = self
            .locations
            .account
            .status
            .as_ref()
            .and_then(|(epoch, receiver)| match receiver.try_recv() {
                Ok(status) => Some((*epoch, Some(status))),
                Err(mpsc::TryRecvError::Disconnected) => Some((*epoch, None)),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some((epoch, status)) = status {
            self.locations.account.status = None;
            if let Some(status) = status {
                self.locations.account.last = Some(status.clone());
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    if epoch == self.locations.epoch {
                        dialog.account = Some(status);
                        outcome.repaint = true;
                    }
                }
            }
        }
        let mut signed_in = false;
        loop {
            let received =
                self.locations.account.sign_in.as_ref().and_then(
                    |(epoch, receiver)| match receiver.try_recv() {
                        Ok(event) => Some((*epoch, event)),
                        Err(mpsc::TryRecvError::Disconnected) => Some((
                            *epoch,
                            AccountEvent::Finished(Err(SignInFailure {
                                message: "Sign-in stopped unexpectedly.".into(),
                                invite: None,
                            })),
                        )),
                        Err(mpsc::TryRecvError::Empty) => None,
                    },
                );
            let Some((epoch, event)) = received else {
                break;
            };
            let finished = matches!(event, AccountEvent::Finished(_));
            if finished {
                self.locations.account.sign_in = None;
                self.locations.account.cancel = None;
            }
            let current = epoch == self.locations.epoch;
            if let (true, Some(ClientShellOverlay::Locations(dialog))) =
                (current, self.overlay.as_mut())
            {
                let sign_up = matches!(dialog.kind, LocationDialogKind::SignUp);
                if sign_up || matches!(dialog.kind, LocationDialogKind::Manage) {
                    match event {
                        AccountEvent::Step(step) => dialog.message = step.message(),
                        AccountEvent::Finished(Ok(())) => {
                            dialog.busy = false;
                            // A finished sign-up returns to the account with the code
                            // dropped.
                            dialog.kind = LocationDialogKind::Manage;
                            dialog.fields.clear();
                            dialog.selected = 0;
                            dialog.message = "Signed in to hangar.".into();
                            dialog.account = None;
                            dialog.view.usage = None;
                            dialog.view.images = None;
                            signed_in = true;
                        }
                        AccountEvent::Finished(Err(failure)) => {
                            dialog.busy = false;
                            let text = match failure.invite {
                                Some(InviteProblem::Required) => INVITE_REQUIRED.to_owned(),
                                Some(InviteProblem::Invalid) => INVITE_INVALID.to_owned(),
                                Some(InviteProblem::NotAccepted) => SIGN_UP_NOT_ACCEPTED.to_owned(),
                                None => failure.message,
                            };
                            if sign_up {
                                // The form stays open with the code for editing.
                                dialog.selected = 0;
                                dialog.message = sign_up_message(&text);
                            } else {
                                if failure.invite == Some(InviteProblem::Required) {
                                    dialog.view.account_action = AccountAction::SignUp;
                                }
                                dialog.message = text;
                            }
                        }
                    }
                    outcome.repaint = true;
                }
            }
            if finished {
                break;
            }
        }
        if signed_in {
            self.refresh_account();
            self.locations.usage = None;
            self.locations.sync = None;
            self.sync_machines();
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_ref() {
                if dialog.view.tab == RemotesTab::Account {
                    self.request_usage();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hangar::api::DeviceStart;

    fn signed_in() -> AccountStatus {
        AccountStatus::SignedIn {
            server: "https://hangar.test".into(),
            login: "octo".into(),
        }
    }

    /// Settings → Remotes on the Account view.
    fn account_shell() -> ClientShellState {
        let mut state = super::super::tests::shell();
        let mut dialog = super::super::tests::dialog();
        dialog.kind = LocationDialogKind::Manage;
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.locations.account.status_reader = Some(signed_in);
        state.switch_remotes_tab(RemotesTab::Account);
        state
    }

    fn current(state: &ClientShellState) -> &LocationDialog {
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        dialog
    }

    /// Runs ticks until `done` holds (workers are real threads).
    fn tick_until(state: &mut ClientShellState, done: impl Fn(&ClientShellState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(state) && Instant::now() < deadline {
            state.tick_locations(&mut ClientShellInput::default());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(done(state), "timed out");
    }

    #[test]
    fn the_badge_names_the_signed_in_user() {
        let mut state = account_shell();
        tick_until(&mut state, |state| current(state).account.is_some());
        assert_eq!(badge(current(&state).account.as_ref()), "@octo");
        let signed_out = AccountStatus::SignedOut {
            server: "https://hangar.test".into(),
        };
        assert_eq!(badge(Some(&signed_out)), "not signed in");
        assert_eq!(badge(None), "");
        assert!(screen(&state).contains("@octo"));
    }

    fn browser_then_ok(
        _invite: Option<&str>,
        notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        notify(SignInStep::Browser {
            url: "https://hangar.test/auth/cli/start?state=s".into(),
        });
        Ok(())
    }

    fn device_until_cancelled(
        _invite: Option<&str>,
        notify: &mut dyn FnMut(SignInStep),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        notify(SignInStep::Device {
            device: DeviceStart {
                device_code: "dc".into(),
                user_code: "ABCD-EFGH".into(),
                verification_uri: "https://github.com/login/device".into(),
                interval: 5,
                expires_in: 900,
            },
            reason: Some("this is an SSH session without a display".into()),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(HangarError::Invalid("sign-in cancelled".into()))
    }

    #[test]
    fn sign_in_runs_inline_reports_progress_then_rereads_the_account() {
        let mut state = account_shell();
        state.locations.account.signer = Some(browser_then_ok);
        state.accept_location(&mut ClientShellInput::default());
        assert!(current(&state).busy, "a running sign-in blocks the actions");
        assert!(matches!(current(&state).kind, LocationDialogKind::Manage));
        tick_until(&mut state, |state| {
            current(state).message == "Signed in to hangar." && current(state).account.is_some()
        });
        assert!(!current(&state).busy);
        assert_eq!(current(&state).account, Some(signed_in()));
        assert_eq!(current(&state).view.tab, RemotesTab::Account);
        // Signed in: the first action is Switch account…, which signs in again.
        assert_eq!(
            current(&state).selected_account_action(),
            AccountAction::SwitchAccount
        );
        state.accept_location(&mut ClientShellInput::default());
        assert!(current(&state).busy, "Switch account… runs the sign-in");
        tick_until(&mut state, |state| !current(state).busy);
    }

    #[test]
    fn closing_the_dialog_cancels_a_device_code_sign_in() {
        let mut state = account_shell();
        state.locations.account.signer = Some(device_until_cancelled);
        state.accept_location(&mut ClientShellInput::default());
        tick_until(&mut state, |state| {
            current(state).message.contains("ABCD-EFGH")
        });
        assert!(current(&state)
            .message
            .starts_with("Using a sign-in code: this is an SSH session"));
        assert!(screen(&state).contains("ABCD-EFGH"), "{}", screen(&state));
        let cancel = state.locations.account.cancel.clone().unwrap();
        state.close_location();
        assert!(cancel.load(Ordering::Acquire));
        assert!(state.locations.account.sign_in.is_none());
    }

    fn screen_at(state: &ClientShellState, width: u16, height: u16) -> String {
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        crate::client::shell::render::render_locations(
            &mut buffer,
            current(state),
            &state.config.palette,
        )
        .expect("rendered");
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(state: &ClientShellState) -> String {
        screen_at(state, 100, 30)
    }

    #[test]
    fn the_account_view_shows_the_user_actions_and_usage() {
        let mut state = account_shell();
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.account = Some(signed_in());
            dialog.view.usage = Some(Ok(crate::hangar::api::Usage {
                computed_at: Some("2026-10-03T08:00:00Z".into()),
                stored_bytes: 3 << 30,
                machines: 2,
                images: 1,
                limits: crate::hangar::api::UsageLimits {
                    max_machines: 5,
                    max_images: 10,
                    max_stored_gib: 20,
                },
                ..Default::default()
            }));
        }
        let text = screen(&state);
        assert!(!text.contains("Sign in\n"), "{text}");
        for part in [
            "Signed in as @octo on https://hangar.test.",
            "▸ Switch account…",
            "  Sign out…",
            "Storage   3.0 GiB of 20.0 GiB stored (15%)",
            "Machines  2 of 5",
            "Images    1 of 10",
            "Measured 2026-10-03 08:00 UTC",
            "hangar CLI share",
        ] {
            assert!(text.contains(part), "{part}\n{text}");
        }
    }

    #[test]
    fn sign_out_asks_first_returns_to_the_account_view_and_warns_about_the_hangar_cli() {
        let mut state = account_shell();
        tick_until(&mut state, |state| current(state).account.is_some());
        let key = |state: &mut ClientShellState, code| {
            state.route_location_key(
                &crate::input::TerminalKey::from(crossterm::event::KeyEvent::new(
                    code,
                    crossterm::event::KeyModifiers::NONE,
                )),
                &mut ClientShellInput::default(),
            );
        };
        assert_eq!(
            current(&state).account_actions(),
            [AccountAction::SwitchAccount, AccountAction::SignOut]
        );
        key(&mut state, KeyCode::Down);
        key(&mut state, KeyCode::Enter);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::SignOut));
        assert!(dialog.labels().is_empty());
        assert!(dialog.message.contains("on https://hangar.test"));
        assert!(dialog.message.contains("hangar CLI shares"));
        // Esc returns to the Account view with Sign out… still selected.
        key(&mut state, KeyCode::Esc);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert_eq!(dialog.view.tab, RemotesTab::Account);
        assert_eq!(dialog.view.account_action, AccountAction::SignOut);
        // Nobody signed in (or an expired sign-in): only Sign in is offered, and the
        // selection follows.
        for status in [
            AccountStatus::Expired {
                server: "https://hangar.test".into(),
            },
            AccountStatus::SignedOut {
                server: "https://hangar.test".into(),
            },
        ] {
            if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
                dialog.account = Some(status);
            }
            assert_eq!(
                current(&state).account_actions(),
                [AccountAction::SignIn, AccountAction::SignUp]
            );
            assert_eq!(
                current(&state).selected_account_action(),
                AccountAction::SignIn
            );
            let text = screen(&state);
            assert!(text.contains("▸ Sign in"), "{text}");
            assert!(text.contains("  Sign up with invite code…"), "{text}");
            assert!(!text.contains("Sign out…"), "{text}");
            assert!(!text.contains("Switch account…"), "{text}");
        }
        state.run_account_action(AccountAction::SignOut);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert_eq!(dialog.message, "Not signed in to hangar.");
    }
    const INVITE: &str = "hgi_ABCDEFGHIJKLMNOPQRST";

    fn signed_out() -> AccountStatus {
        AccountStatus::SignedOut {
            server: "https://hangar.test".into(),
        }
    }

    /// Settings → Remotes on the Account view, signed out.
    fn signed_out_shell() -> ClientShellState {
        let mut state = super::super::tests::shell();
        let mut dialog = super::super::tests::dialog();
        dialog.kind = LocationDialogKind::Manage;
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.locations.account.status_reader = Some(signed_out);
        state.switch_remotes_tab(RemotesTab::Account);
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.account = Some(signed_out());
        }
        state
    }

    fn key(state: &mut ClientShellState, code: KeyCode) {
        state.route_location_key(
            &crate::input::TerminalKey::from(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut ClientShellInput::default(),
        );
    }

    fn refused(code: &str) -> HangarError {
        HangarError::SignInRefused {
            code: crate::hangar::api::ErrorCode::parse(code),
            detail: String::new(),
        }
    }

    /// The invites each test signer received, so tests can check what was sent.
    static RECEIVED: std::sync::Mutex<Vec<Option<String>>> = std::sync::Mutex::new(Vec::new());

    fn record(invite: Option<&str>) {
        RECEIVED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(invite.map(str::to_owned));
    }

    fn sign_up_ok(
        invite: Option<&str>,
        _notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        record(invite);
        Ok(())
    }

    fn invite_required(
        _invite: Option<&str>,
        _notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        Err(refused("invite_required"))
    }

    fn invite_invalid(
        _invite: Option<&str>,
        _notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        Err(refused("invite_invalid"))
    }

    /// A server without invite codes: it ignores the invite and denies the sign-in.
    fn old_server_denies(
        _invite: Option<&str>,
        _notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        Err(refused("access_denied"))
    }

    fn old_server_forbids(
        _invite: Option<&str>,
        _notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        Err(HangarError::Api(crate::hangar::api::ApiError {
            status: 403,
            code: crate::hangar::api::ErrorCode::PermissionDenied,
            message: "not allowed".into(),
            operation_id: None,
        }))
    }

    /// Opens the sign-up form and enters `code`.
    fn sign_up_form(code: &str) -> ClientShellState {
        let mut state = signed_out_shell();
        key(&mut state, KeyCode::Down);
        assert_eq!(
            current(&state).selected_account_action(),
            AccountAction::SignUp
        );
        key(&mut state, KeyCode::Enter);
        assert!(matches!(current(&state).kind, LocationDialogKind::SignUp));
        state.insert_overlay_text(code);
        state
    }

    fn field(state: &ClientShellState) -> String {
        current(state)
            .fields
            .first()
            .map(|field| field.as_str().to_owned())
            .unwrap_or_default()
    }

    #[test]
    fn a_plain_sign_in_that_needs_an_invite_points_at_sign_up() {
        let mut state = signed_out_shell();
        state.locations.account.signer = Some(invite_required);
        assert_eq!(
            current(&state).selected_account_action(),
            AccountAction::SignIn
        );
        key(&mut state, KeyCode::Enter);
        tick_until(&mut state, |state| !current(state).busy);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert_eq!(dialog.message, INVITE_REQUIRED);
        assert_eq!(dialog.selected_account_action(), AccountAction::SignUp);
        let text = screen(&state);
        assert!(text.contains("▸ Sign up with invite code…"), "{text}");
        assert!(text.contains("isn't on hangar yet"), "{text}");
    }

    #[test]
    fn sign_up_checks_the_code_then_signs_in_with_it_and_drops_it() {
        RECEIVED.lock().unwrap_or_else(|p| p.into_inner()).clear();
        let mut state = sign_up_form("");
        state.locations.account.signer = Some(sign_up_ok);
        let dialog = current(&state);
        assert_eq!(dialog.title(), "sign up for hangar");
        assert_eq!(dialog.labels(), ["Invite code"]);
        assert_eq!(dialog.message, SIGN_UP_NOTE);
        // Nothing entered, then something that is not an invite code: nothing is sent.
        key(&mut state, KeyCode::Enter);
        assert!(current(&state)
            .message
            .starts_with("Enter the invite code you received.\n\n"));
        state.insert_overlay_text("hgi_short");
        key(&mut state, KeyCode::Enter);
        assert!(current(&state).message.contains("start with hgi_"));
        assert!(current(&state).message.ends_with(SIGN_UP_NOTE));
        assert!(!current(&state).busy);
        assert!(RECEIVED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty());
        // A valid code (surrounding spaces trimmed) signs up.
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.fields[0] = TextEditor::new("", false);
        }
        state.insert_overlay_text(&format!("  {INVITE} "));
        key(&mut state, KeyCode::Enter);
        assert!(current(&state).busy);
        assert_eq!(current(&state).message, "Starting sign-up…");
        tick_until(&mut state, |state| {
            current(state).message == "Signed in to hangar." && current(state).account.is_some()
        });
        assert_eq!(
            *RECEIVED.lock().unwrap_or_else(|p| p.into_inner()),
            [Some(INVITE.to_owned())]
        );
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert!(dialog.fields.is_empty(), "the code is dropped");
        assert!(!format!("{:?}", state.locations.account).contains(INVITE));
    }

    #[test]
    fn an_invalid_invite_keeps_the_form_open_with_the_code() {
        let mut state = sign_up_form(INVITE);
        state.locations.account.signer = Some(invite_invalid);
        key(&mut state, KeyCode::Enter);
        tick_until(&mut state, |state| !current(state).busy);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::SignUp));
        assert_eq!(field(&state), INVITE);
        assert_eq!(dialog.selected, 0);
        assert_eq!(
            dialog.message,
            format!("{INVITE_INVALID}\n\n{SIGN_UP_NOTE}")
        );
        // Edit and resubmit.
        key(&mut state, KeyCode::Backspace);
        assert_eq!(field(&state), &INVITE[..INVITE.len() - 1]);
    }

    #[test]
    fn a_server_without_sign_up_is_reported_honestly() {
        for signer in [old_server_denies as SignIn, old_server_forbids] {
            let mut state = sign_up_form(INVITE);
            state.locations.account.signer = Some(signer);
            key(&mut state, KeyCode::Enter);
            tick_until(&mut state, |state| !current(state).busy);
            let dialog = current(&state);
            assert!(matches!(dialog.kind, LocationDialogKind::SignUp));
            assert!(dialog.message.starts_with(SIGN_UP_NOT_ACCEPTED));
            assert!(!dialog.message.contains("Signed in"));
            assert!(state.locations.account.last_status() != Some(signed_in()));
        }
        // Without an invite the same denial is reported as it is.
        let mut state = signed_out_shell();
        state.locations.account.signer = Some(old_server_denies);
        key(&mut state, KeyCode::Enter);
        tick_until(&mut state, |state| !current(state).busy);
        assert_eq!(current(&state).message, "hangar: sign-in was denied");
    }

    #[test]
    fn esc_cancels_a_running_sign_up_and_returns_to_the_account() {
        let mut state = sign_up_form(INVITE);
        state.locations.account.signer = Some(device_until_cancelled);
        key(&mut state, KeyCode::Enter);
        tick_until(&mut state, |state| {
            current(state).message.contains("ABCD-EFGH")
        });
        assert!(matches!(current(&state).kind, LocationDialogKind::SignUp));
        let cancel = state.locations.account.cancel.clone().unwrap();
        key(&mut state, KeyCode::Esc);
        assert!(cancel.load(Ordering::Acquire));
        assert!(state.locations.account.sign_in.is_none());
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert!(!dialog.busy);
        assert!(dialog.fields.is_empty());
        assert_eq!(dialog.view.tab, RemotesTab::Account);
        assert_eq!(dialog.selected_account_action(), AccountAction::SignUp);
        // Esc on the idle form also goes back.
        let mut state = sign_up_form(INVITE);
        key(&mut state, KeyCode::Esc);
        assert!(matches!(current(&state).kind, LocationDialogKind::Manage));
        assert!(current(&state).fields.is_empty());
    }

    #[test]
    fn signed_in_accounts_offer_no_sign_up() {
        let mut state = account_shell();
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.account = Some(signed_in());
            dialog.view.account_action = AccountAction::SignUp;
        }
        assert_eq!(
            current(&state).account_actions(),
            [AccountAction::SwitchAccount, AccountAction::SignOut]
        );
        assert_eq!(
            current(&state).selected_account_action(),
            AccountAction::SwitchAccount
        );
        assert!(!screen(&state).contains("Sign up"));
    }

    #[test]
    fn signed_out_account_and_sign_up_form_render_at_large_and_small_sizes() {
        let state = signed_out_shell();
        for (width, height) in [(120, 40), (64, 20)] {
            let text = screen_at(&state, width, height);
            println!("--- account tab, signed out, {width}x{height}\n{text}");
            assert!(text.contains("▸ Sign in"), "{text}");
            assert!(text.contains("Sign up with invite code…"), "{text}");
        }
        let mut state = sign_up_form(INVITE);
        state.locations.account.signer = Some(invite_invalid);
        key(&mut state, KeyCode::Enter);
        tick_until(&mut state, |state| !current(state).busy);
        let text = screen_at(&state, 120, 40);
        println!("--- sign-up form, invalid code, 120x40\n{text}");
        assert!(text.contains("That invite code isn't valid"), "{text}");
        assert!(text.contains("hangar is invite-only."), "{text}");
        let state = sign_up_form("hgi_ABCDEFGHIJ");
        for (width, height) in [(120, 40), (64, 20)] {
            let text = screen_at(&state, width, height);
            println!("--- sign-up form, {width}x{height}\n{text}");
            for part in [
                "sign up for hangar",
                "Invite code: hgi_ABCDEFGHIJ",
                "hangar is invite-only.",
                "↵ sign up",
                "esc back",
            ] {
                assert!(text.contains(part), "{part}\n{text}");
            }
        }
    }
}
