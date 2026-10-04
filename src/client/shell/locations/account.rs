//! The Account view of Settings → Remotes: who is signed in, Sign in (in the browser,
//! or with a device code) and Sign out…. Herdr shares the sign-in with the hangar CLI.
//! HTTP runs in workers; results carry the dialog epoch so a late result never
//! overrides a newer dialog.
use super::view::{AccountAction, RemotesTab};
use super::*;
use crate::hangar::api::HangarError;
use crate::hangar::login::SignInStep;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

    pub(in crate::client::shell) fn run_account_action(&mut self, action: AccountAction) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        match action {
            AccountAction::SignIn | AccountAction::SwitchAccount => self.start_account_sign_in(),
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
                if matches!(dialog.kind, LocationDialogKind::Manage) {
                    match event {
                        AccountEvent::Step(step) => dialog.message = step.message(),
                        AccountEvent::Finished(Ok(())) => {
                            dialog.busy = false;
                            dialog.message = "Signed in to hangar.".into();
                            dialog.account = None;
                            dialog.view.usage = None;
                            dialog.view.images = None;
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
            assert_eq!(current(&state).account_actions(), [AccountAction::SignIn]);
            assert_eq!(
                current(&state).selected_account_action(),
                AccountAction::SignIn
            );
            let text = screen(&state);
            assert!(text.contains("▸ Sign in"), "{text}");
            assert!(!text.contains("Sign out…"), "{text}");
            assert!(!text.contains("Switch account…"), "{text}");
        }
        state.run_account_action(AccountAction::SignOut);
        let dialog = current(&state);
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert_eq!(dialog.message, "Not signed in to hangar.");
    }
}
