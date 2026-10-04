//! hangar account in Settings → remotes: who is signed in, Sign in (in the browser, or
//! with a device code) and Sign out…. Herdr shares the sign-in with the hangar CLI.
//! HTTP runs in workers; results carry the dialog epoch so a late result never
//! overrides a newer dialog.
use super::*;
use crate::hangar::api::HangarError;
use crate::hangar::login::SignInStep;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(in crate::client::shell) const ROWS: [&str; 2] = ["Sign in", "Sign out…"];
const SIGN_IN_ROW: usize = 0;
const SIGN_OUT_ROW: usize = 1;
const SHARED_NOTE: &str = "Herdr and the hangar CLI share this sign-in (~/.config/hangar/credentials.json). Sign in opens your browser; over SSH or without a browser it shows a code to enter instead.";

/// The Settings → remotes row for the account.
pub(super) fn manage_row_label(status: Option<&AccountStatus>) -> Cow<'static, str> {
    match status {
        Some(AccountStatus::SignedIn { login, .. }) => format!("hangar account: @{login}").into(),
        Some(AccountStatus::SignedOut { .. }) => "hangar account: not signed in".into(),
        Some(AccountStatus::Expired { .. }) => "hangar account: sign-in expired".into(),
        Some(AccountStatus::Unverified { .. }) | None => "hangar account…".into(),
    }
}

/// Who is signed in, then the latest message (or what the dialog does).
pub(super) fn body(status: Option<&AccountStatus>, message: &str) -> String {
    let summary = status
        .map(AccountStatus::summary)
        .unwrap_or_else(|| "Checking your hangar sign-in…".into());
    let detail = if message.is_empty() {
        SHARED_NOTE
    } else {
        message
    };
    format!("{summary}\n\n{detail}")
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

enum AccountEvent {
    Step(SignInStep),
    Finished(Result<(), String>),
}

type SignIn = fn(&mut dyn FnMut(SignInStep), &dyn Fn() -> bool) -> Result<(), HangarError>;

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

    pub(super) fn open_account(&mut self) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        dialog.kind = LocationDialogKind::Account;
        dialog.selected = SIGN_IN_ROW;
        dialog.message.clear();
        self.refresh_account();
    }

    pub(super) fn accept_account(&mut self, selected: usize) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        match selected {
            SIGN_IN_ROW => self.start_account_sign_in(),
            SIGN_OUT_ROW => match &dialog.account {
                Some(status) if !status.signed_in() => {
                    dialog.message = "Not signed in to hangar.".into();
                }
                status => {
                    dialog.message = sign_out_confirmation(status.as_ref());
                    dialog.kind = LocationDialogKind::SignOut;
                    dialog.selected = 0;
                }
            },
            _ => {}
        }
    }

    fn start_account_sign_in(&mut self) {
        if self.locations.account.sign_in.is_some() {
            return;
        }
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        dialog.busy = true;
        dialog.message = "Starting sign-in…".into();
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
                &mut |step| {
                    let _ = send.send(AccountEvent::Step(step));
                },
                &|| cancel.load(Ordering::Acquire),
            );
            let _ = send.send(AccountEvent::Finished(
                result.map_err(|error| error.to_string()),
            ));
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
                            AccountEvent::Finished(Err("Sign-in stopped unexpectedly.".into())),
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
                if matches!(dialog.kind, LocationDialogKind::Account) {
                    match event {
                        AccountEvent::Step(step) => dialog.message = step.message(),
                        AccountEvent::Finished(Ok(())) => {
                            dialog.busy = false;
                            dialog.message = "Signed in to hangar.".into();
                            dialog.account = None;
                            signed_in = true;
                        }
                        AccountEvent::Finished(Err(error)) => {
                            dialog.busy = false;
                            dialog.message = error;
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

    fn manage_shell() -> ClientShellState {
        let mut state = super::super::tests::shell();
        let mut dialog = super::super::tests::dialog();
        dialog.kind = LocationDialogKind::Manage;
        dialog.selected = ACCOUNT_ROW;
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.locations.account.status_reader = Some(signed_in);
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
    fn the_account_row_names_the_signed_in_user_and_opens_the_account_dialog() {
        let mut state = manage_shell();
        assert_eq!(current(&state).row_label(ACCOUNT_ROW), "hangar account…");
        state.refresh_account();
        tick_until(&mut state, |state| current(state).account.is_some());
        assert_eq!(
            current(&state).row_label(ACCOUNT_ROW),
            "hangar account: @octo"
        );
        state.accept_location(&mut ClientShellInput::default());
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Account));
        assert_eq!(dialog.title(), "hangar account");
        assert!(dialog.action_rows());
        assert!(dialog
            .body()
            .starts_with("Signed in as @octo on https://hangar.test."));
        assert!(dialog.body().contains("hangar CLI share"));
        let signed_out = AccountStatus::SignedOut {
            server: "https://hangar.test".into(),
        };
        assert_eq!(
            manage_row_label(Some(&signed_out)),
            "hangar account: not signed in"
        );
    }

    fn browser_then_ok(
        notify: &mut dyn FnMut(SignInStep),
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HangarError> {
        notify(SignInStep::Browser {
            url: "https://hangar.test/auth/cli/start?state=s".into(),
        });
        Ok(())
    }

    fn device_until_cancelled(
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
    fn sign_in_reports_progress_then_rereads_the_account() {
        let mut state = manage_shell();
        state.locations.account.signer = Some(browser_then_ok);
        state.open_account();
        state.accept_location(&mut ClientShellInput::default());
        assert!(current(&state).busy, "a running sign-in blocks the rows");
        tick_until(&mut state, |state| {
            current(state).message == "Signed in to hangar." && current(state).account.is_some()
        });
        assert!(!current(&state).busy);
        assert_eq!(current(&state).account, Some(signed_in()));
    }

    #[test]
    fn closing_the_dialog_cancels_a_device_code_sign_in() {
        let mut state = manage_shell();
        state.locations.account.signer = Some(device_until_cancelled);
        state.open_account();
        state.accept_location(&mut ClientShellInput::default());
        tick_until(&mut state, |state| {
            current(state).message.contains("ABCD-EFGH")
        });
        assert!(current(&state)
            .message
            .starts_with("Using a sign-in code: this is an SSH session"));
        let cancel = state.locations.account.cancel.clone().unwrap();
        state.close_location();
        assert!(cancel.load(Ordering::Acquire));
        assert!(state.locations.account.sign_in.is_none());
    }

    fn screen(state: &ClientShellState) -> String {
        let area = ratatui::layout::Rect::new(0, 0, 100, 30);
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
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_account_renders_as_rows_with_the_status_below_them() {
        let mut state = manage_shell();
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.account = Some(signed_in());
        }
        assert!(screen(&state).contains("hangar account: @octo"));
        state.open_account();
        let text = screen(&state);
        assert!(text.contains(" Sign in "), "{text}");
        assert!(text.contains(" Sign out…"), "{text}");
        assert!(
            text.contains("Signed in as @octo on https://hangar.test."),
            "{text}"
        );
        assert!(text.contains("↵ select"), "{text}");
    }

    #[test]
    fn sign_out_asks_first_and_warns_about_the_hangar_cli() {
        let mut state = manage_shell();
        state.open_account();
        tick_until(&mut state, |state| current(state).account.is_some());
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.selected = SIGN_OUT_ROW;
        }
        state.accept_location(&mut ClientShellInput::default());
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::SignOut));
        assert!(dialog.labels().is_empty());
        assert!(dialog.message.contains("on https://hangar.test"));
        assert!(dialog.message.contains("hangar CLI shares"));
        // Nobody signed in: nothing to confirm.
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.kind = LocationDialogKind::Account;
            dialog.account = Some(AccountStatus::SignedOut {
                server: "https://hangar.test".into(),
            });
        }
        state.accept_account(SIGN_OUT_ROW);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Account));
        assert_eq!(dialog.message, "Not signed in to hangar.");
    }
}
