//! The remotes, images and account tabs of Settings: a list of remotes and the actions
//! of the selected one, a list of images and their actions, and the hangar account.
//! Everything here is TUI presentation state: tab, selection, focus and the last fetched
//! images and usage. Forms and confirmations stay modal dialogs of the same
//! `LocationDialog`; when one closes, the view (tab, selection, focus) is what it was
//! before.
//!
//! Images and usage are fetched on worker threads when their tab opens; results
//! carry the dialog epoch so a late result never overrides a newer dialog.
use super::*;
use crate::client::locations::hangar::{MachineDetails, MachineSource};
use crate::hangar::api::{HangarError, Image, Usage};

/// The settings tabs this view shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) enum RemotesTab {
    #[default]
    Remotes,
    Images,
    Account,
}

impl RemotesTab {
    pub(in crate::client::shell) fn section(self) -> ClientSettingsSection {
        match self {
            Self::Remotes => ClientSettingsSection::Remotes,
            Self::Images => ClientSettingsSection::Images,
            Self::Account => ClientSettingsSection::Account,
        }
    }
}

/// Which pane of the remotes and images tabs takes ↑/↓ and ↵.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) enum Focus {
    #[default]
    List,
    Actions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum RemoteAction {
    Start,
    Suspend,
    Stop,
    Test,
    Edit,
    Default,
    Hide,
    SaveImage,
    Fork,
    Remove,
}

/// Actions are listed in groups, separated by a blank line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum ActionGroup {
    Lifecycle,
    Connection,
    Copy,
    Remove,
}

/// One action of the selected remote. A `reason` means it applies to this kind of
/// remote but not now: it is shown dimmed and the reason is shown when selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct ActionEntry {
    pub action: RemoteAction,
    pub group: ActionGroup,
    pub label: &'static str,
    pub reason: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum RemoteKind {
    Local,
    Ssh,
    Hangar,
}

/// What decides which actions a remote offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct RemoteFacts {
    pub kind: RemoteKind,
    /// Last known hangar state; `None` when unknown.
    pub state: Option<MachineState>,
    /// Herdr connects to it (SSH: not disabled; hangar: running, shown, not fenced).
    pub enabled: bool,
    pub hidden: bool,
    pub default: bool,
    /// Its hangar server's list could not be fetched because nobody is signed in.
    pub signed_out: bool,
    /// Its hangar server could not be reached; the state is the last synced one.
    pub offline: bool,
    /// Whether its template allows images and forks; `None` when unknown.
    pub snapshots: Option<bool>,
}

const SIGN_IN_FIRST: &str = "Sign in on the account tab first";
const OFFLINE: &str = "hangar is unreachable; showing the last synced state";

fn transitional(state: MachineState) -> Option<&'static str> {
    Some(match state {
        MachineState::Creating => "Wait until the machine is created",
        MachineState::Starting => "Wait until the machine has started",
        MachineState::Stopping => "Wait until the machine has stopped",
        MachineState::Suspending => "Wait until the machine is suspended",
        MachineState::Resuming => "Wait until the machine has resumed",
        MachineState::Deleting => "The machine is being deleted",
        MachineState::Deleted => "The machine was deleted",
        _ => return None,
    })
}

/// The actions of a remote in display order: lifecycle, connection, copy, remove.
/// Pure; table-tested.
pub(in crate::client::shell) fn remote_actions(facts: &RemoteFacts) -> Vec<ActionEntry> {
    let entry = |action, group, label, reason| ActionEntry {
        action,
        group,
        label,
        reason,
    };
    let default_reason = facts
        .default
        .then_some("Already the default for new workspaces");
    match facts.kind {
        RemoteKind::Local => vec![entry(
            RemoteAction::Default,
            ActionGroup::Connection,
            "Use as default",
            default_reason,
        )],
        // An SSH remote has no machine to manage; Start session starts its installed
        // Herdr session and enables it.
        RemoteKind::Ssh => vec![
            entry(
                RemoteAction::Test,
                ActionGroup::Connection,
                "Test connection",
                (!facts.enabled).then_some("Disabled; Start session enables it"),
            ),
            entry(RemoteAction::Edit, ActionGroup::Connection, "Edit…", None),
            entry(
                RemoteAction::Default,
                ActionGroup::Connection,
                "Use as default",
                default_reason,
            ),
            entry(
                RemoteAction::Start,
                ActionGroup::Connection,
                "Start session",
                None,
            ),
            entry(
                RemoteAction::Remove,
                ActionGroup::Remove,
                "Remove remote",
                None,
            ),
        ],
        RemoteKind::Hangar => {
            let state = facts.state.unwrap_or(MachineState::Unknown);
            let waiting = transitional(state);
            let server = |reason: Option<&'static str>| {
                if facts.signed_out {
                    Some(SIGN_IN_FIRST)
                } else if facts.offline {
                    Some(OFFLINE)
                } else {
                    reason
                }
            };
            // Lifecycle actions show only where they make sense: Start for a stopped (or
            // failed) machine, Resume for a suspended one, Suspend… and Stop… for a
            // running one (Stop… also for a failed one). During a transition the action
            // that will apply next is shown, dimmed until it finishes. An unknown state
            // offers Start and Stop… and lets the server decide.
            use MachineState as S;
            let (start, resume, suspend, stop) = match state {
                S::Running => (false, false, true, true),
                S::Stopped => (true, false, false, false),
                S::Error | S::Unknown => (true, false, false, true),
                S::Suspended => (false, true, false, false),
                S::Creating | S::Starting | S::Resuming => (false, false, true, true),
                S::Stopping => (true, false, false, false),
                S::Suspending => (false, true, false, false),
                S::Deleting | S::Deleted => (false, false, false, false),
            };
            let mut lifecycle = Vec::new();
            if start || resume {
                lifecycle.push(entry(
                    RemoteAction::Start,
                    ActionGroup::Lifecycle,
                    if resume { "Resume" } else { "Start" },
                    server(waiting),
                ));
            }
            if suspend {
                lifecycle.push(entry(
                    RemoteAction::Suspend,
                    ActionGroup::Lifecycle,
                    "Suspend…",
                    server(waiting),
                ));
            }
            if stop {
                lifecycle.push(entry(
                    RemoteAction::Stop,
                    ActionGroup::Lifecycle,
                    "Stop…",
                    server(waiting),
                ));
            }
            let test = if facts.enabled {
                None
            } else if facts.hidden {
                Some("Hidden from the sidebar; Show in sidebar first")
            } else if state == MachineState::Running {
                Some("Not connected yet; try again shortly")
            } else {
                Some("Start the machine first")
            };
            let snapshot = |too_old| match facts.snapshots {
                Some(false) => Some(too_old),
                _ => waiting,
            };
            let delete = match state {
                MachineState::Deleting | MachineState::Deleted => waiting,
                _ => None,
            };
            lifecycle
                .into_iter()
                .chain([
                    entry(
                        RemoteAction::Test,
                        ActionGroup::Connection,
                        "Test connection",
                        test,
                    ),
                    entry(RemoteAction::Edit, ActionGroup::Connection, "Edit…", None),
                    entry(
                        RemoteAction::Default,
                        ActionGroup::Connection,
                        "Use as default",
                        default_reason,
                    ),
                    entry(
                        RemoteAction::Hide,
                        ActionGroup::Connection,
                        if facts.hidden {
                            "Show in sidebar"
                        } else {
                            "Hide from sidebar"
                        },
                        None,
                    ),
                    entry(
                        RemoteAction::SaveImage,
                        ActionGroup::Copy,
                        "Save as image…",
                        server(snapshot("Template too old for images")),
                    ),
                    entry(
                        RemoteAction::Fork,
                        ActionGroup::Copy,
                        "Fork…",
                        server(snapshot("Template too old to fork")),
                    ),
                    entry(
                        RemoteAction::Remove,
                        ActionGroup::Remove,
                        "Delete machine…",
                        server(delete),
                    ),
                ])
                .collect()
        }
    }
}

/// A row of the remotes list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum ListRow {
    Local,
    /// Index into `LocationDialog::profiles`.
    Remote(usize),
    Add,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) enum ImageAction {
    #[default]
    NewMachine,
    Delete,
}

impl ImageAction {
    pub(in crate::client::shell) const ALL: [Self; 2] = [Self::NewMachine, Self::Delete];

    pub(in crate::client::shell) fn label(self) -> &'static str {
        match self {
            Self::NewMachine => "New machine from image…",
            Self::Delete => "Delete image…",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) enum AccountAction {
    #[default]
    SignIn,
    /// Signs in again, possibly as someone else; offered while signed in.
    SwitchAccount,
    SignOut,
}

impl AccountAction {
    pub(in crate::client::shell) fn label(self) -> &'static str {
        match self {
            Self::SignIn => "Sign in",
            Self::SwitchAccount => "Switch account…",
            Self::SignOut => "Sign out…",
        }
    }
}

/// Where a click in the view lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum RemotesHit {
    Row(ListRow),
    Action(RemoteAction),
    /// Index into the listed images.
    Image(usize),
    ImageAction(ImageAction),
    AccountAction(AccountAction),
}

/// The caller's images on one server, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct ImageList {
    pub server: String,
    pub images: Vec<Image>,
}

/// Presentation state of the remotes, images and account tabs; it survives the dialogs opened from it.
#[derive(Clone, Debug, Default)]
pub(in crate::client::shell) struct RemotesView {
    pub tab: RemotesTab,
    pub focus: Focus,
    /// "+ Add remote" is selected instead of `LocationDialog::location`.
    pub add_selected: bool,
    /// The selected action; when it is no longer offered (the state changed), the
    /// one at its last position.
    pub action: Option<RemoteAction>,
    /// Where the selected action was in the list.
    pub action_slot: usize,
    /// The selected image's ID; the first image when it is not listed.
    pub image: Option<String>,
    pub image_action: ImageAction,
    pub account_action: AccountAction,
    /// hangar machine details by (server, machine ID), from the settings sync.
    pub details: BTreeMap<(String, String), MachineDetails>,
    /// `None` until listed.
    pub images: Option<Result<ImageList, String>>,
    /// `None` until read.
    pub usage: Option<Result<Usage, String>>,
    /// SSH remotes whose connection is online.
    pub connected: BTreeSet<ProfileId>,
}

impl LocationDialog {
    pub(in crate::client::shell) fn remotes_open(&self) -> bool {
        matches!(self.kind, LocationDialogKind::Manage)
    }

    /// Local, hangar machines, SSH remotes, then Add remote.
    pub(in crate::client::shell) fn list_rows(&self) -> Vec<ListRow> {
        let hangar = |index: &usize| self.prefs.binding(&self.profiles[*index].id).is_some();
        let indices = 0..self.profiles.len();
        std::iter::once(ListRow::Local)
            .chain(indices.clone().filter(hangar).map(ListRow::Remote))
            .chain(indices.filter(|index| !hangar(index)).map(ListRow::Remote))
            .chain(std::iter::once(ListRow::Add))
            .collect()
    }

    pub(in crate::client::shell) fn selected_row(&self) -> ListRow {
        if self.view.add_selected {
            return ListRow::Add;
        }
        match self.location.checked_sub(1) {
            Some(index) if index < self.profiles.len() => ListRow::Remote(index),
            _ => ListRow::Local,
        }
    }

    pub(in crate::client::shell) fn select_row(&mut self, row: ListRow) {
        self.location_missing = false;
        match row {
            ListRow::Add => self.view.add_selected = true,
            ListRow::Local => {
                self.view.add_selected = false;
                self.location = 0;
            }
            ListRow::Remote(index) => {
                self.view.add_selected = false;
                self.location = index + 1;
            }
        }
    }

    fn move_list(&mut self, delta: isize) {
        let rows = self.list_rows();
        let current = self.selected_row();
        let position = rows.iter().position(|row| *row == current).unwrap_or(0);
        let next = (position as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.select_row(rows[next]);
    }

    pub(in crate::client::shell) fn is_default(&self, row: ListRow) -> bool {
        match row {
            ListRow::Local => self
                .prefs
                .default_profile
                .as_ref()
                .is_none_or(|id| !self.profiles.iter().any(|profile| &profile.id == id)),
            ListRow::Remote(index) => {
                self.prefs.default_profile.as_ref() == Some(&self.profiles[index].id)
            }
            ListRow::Add => false,
        }
    }

    /// The hangar binding of a listed remote.
    pub(in crate::client::shell) fn binding_of(&self, index: usize) -> Option<&HangarBinding> {
        self.profiles
            .get(index)
            .and_then(|profile| self.prefs.binding(&profile.id))
            .map(|binding| binding.hangar())
    }

    pub(in crate::client::shell) fn state_of(&self, index: usize) -> Option<MachineState> {
        let binding = self.binding_of(index)?;
        self.machine_states
            .get(&(binding.server.clone(), binding.machine_id.clone()))
            .copied()
    }

    pub(in crate::client::shell) fn details_of(&self, index: usize) -> Option<&MachineDetails> {
        let binding = self.binding_of(index)?;
        self.view
            .details
            .get(&(binding.server.clone(), binding.machine_id.clone()))
    }

    pub(in crate::client::shell) fn facts(&self, row: ListRow) -> Option<RemoteFacts> {
        let default = self.is_default(row);
        match row {
            ListRow::Add => None,
            ListRow::Local => Some(RemoteFacts {
                kind: RemoteKind::Local,
                state: None,
                enabled: true,
                hidden: false,
                default,
                signed_out: false,
                offline: false,
                snapshots: None,
            }),
            ListRow::Remote(index) => {
                let profile = &self.profiles[index];
                let hangar = self.binding_of(index).is_some();
                Some(RemoteFacts {
                    kind: if hangar {
                        RemoteKind::Hangar
                    } else {
                        RemoteKind::Ssh
                    },
                    state: self.state_of(index),
                    enabled: profile.enabled,
                    hidden: self.hidden.contains(&profile.id),
                    default,
                    signed_out: self.sync_notes.get(&profile.id) == Some(&"signed out"),
                    offline: self.sync_notes.get(&profile.id) == Some(&"offline"),
                    snapshots: self.details_of(index).and_then(|details| details.snapshots),
                })
            }
        }
    }

    pub(in crate::client::shell) fn actions(&self) -> Vec<ActionEntry> {
        self.facts(self.selected_row())
            .map(|facts| remote_actions(&facts))
            .unwrap_or_default()
    }

    /// The selected action, falling back to the first one offered.
    pub(in crate::client::shell) fn selected_action(&self) -> Option<ActionEntry> {
        let actions = self.actions();
        actions
            .iter()
            .find(|entry| Some(entry.action) == self.view.action)
            .or_else(|| actions.get(self.view.action_slot.min(actions.len().saturating_sub(1))))
            .cloned()
    }

    pub(in crate::client::shell) fn select_action(&mut self, action: RemoteAction) {
        self.view.action = Some(action);
        if let Some(slot) = self
            .actions()
            .iter()
            .position(|entry| entry.action == action)
        {
            self.view.action_slot = slot;
        }
    }

    fn move_action(&mut self, delta: isize) {
        let actions = self.actions();
        if actions.is_empty() {
            return;
        }
        let position = self
            .selected_action()
            .and_then(|selected| actions.iter().position(|entry| entry == &selected))
            .unwrap_or(0);
        let next = (position as isize + delta).clamp(0, actions.len() as isize - 1) as usize;
        self.view.action = Some(actions[next].action);
        self.view.action_slot = next;
    }

    pub(in crate::client::shell) fn images(&self) -> &[Image] {
        match &self.view.images {
            Some(Ok(list)) => &list.images,
            _ => &[],
        }
    }

    pub(in crate::client::shell) fn selected_image_index(&self) -> Option<usize> {
        let images = self.images();
        if images.is_empty() {
            return None;
        }
        Some(
            self.view
                .image
                .as_ref()
                .and_then(|id| images.iter().position(|image| &image.id == id))
                .unwrap_or(0),
        )
    }

    pub(in crate::client::shell) fn selected_image(&self) -> Option<&Image> {
        self.selected_image_index()
            .and_then(|index| self.images().get(index))
    }

    fn move_image(&mut self, delta: isize) {
        let Some(position) = self.selected_image_index() else {
            return;
        };
        let images = self.images();
        let next = (position as isize + delta).clamp(0, images.len() as isize - 1) as usize;
        self.view.image = Some(images[next].id.clone());
    }

    /// The listed name of the machine with this ID, if Herdr lists it.
    pub(in crate::client::shell) fn machine_name(&self, machine_id: &str) -> Option<&str> {
        (0..self.profiles.len())
            .find(|index| {
                self.binding_of(*index)
                    .is_some_and(|binding| binding.machine_id == machine_id)
            })
            .map(|index| self.profiles[index].label.as_str())
    }

    /// The listed name of the image with this ID, if listed.
    pub(in crate::client::shell) fn image_name(&self, image_id: &str) -> Option<&str> {
        self.images()
            .iter()
            .find(|image| image.id == image_id)
            .map(|image| image.name.as_str())
    }

    /// Switch account… and Sign out… while signed in; otherwise Sign in.
    pub(in crate::client::shell) fn account_actions(&self) -> Vec<AccountAction> {
        match &self.account {
            Some(AccountStatus::SignedIn { .. } | AccountStatus::Unverified { .. }) => {
                vec![AccountAction::SwitchAccount, AccountAction::SignOut]
            }
            _ => vec![AccountAction::SignIn],
        }
    }

    /// The selected account action by identity, else the first offered (Sign in and
    /// Switch account… share the first place).
    pub(in crate::client::shell) fn selected_account_action(&self) -> AccountAction {
        let actions = self.account_actions();
        if actions.contains(&self.view.account_action) {
            self.view.account_action
        } else {
            actions[0]
        }
    }

    /// Why an image action does nothing now.
    pub(in crate::client::shell) fn image_reason(&self) -> Option<&'static str> {
        self.selected_image()
            .is_none()
            .then_some("No image selected")
    }
}

/// How a state word is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum Tone {
    Good,
    Warn,
    Bad,
    Muted,
}

impl LocationDialog {
    /// The state word at the right of a list row (no dots or circles: those are agent
    /// states). Local and Add remote have none.
    pub(in crate::client::shell) fn list_state(&self, row: ListRow) -> Option<(String, Tone)> {
        let ListRow::Remote(index) = row else {
            return None;
        };
        let profile = &self.profiles[index];
        if self.binding_of(index).is_none() {
            let text = self.state_text(index);
            let tone = if text == "connected" {
                Tone::Good
            } else {
                Tone::Muted
            };
            return Some((text.into(), tone));
        }
        if let Some(note) = self.sync_notes.get(&profile.id) {
            return Some(((*note).into(), Tone::Muted));
        }
        let text = self.state_text(index);
        Some(match self.state_of(index) {
            _ if text == "stopping" => ("stopping…".into(), Tone::Warn),
            Some(MachineState::Running) => (text.into(), Tone::Good),
            Some(MachineState::Error) => (text.into(), Tone::Bad),
            Some(state) if transitional(state).is_some() && state != MachineState::Deleted => {
                (format!("{text}…"), Tone::Warn)
            }
            Some(MachineState::Unknown) | None => ("unknown".into(), Tone::Muted),
            Some(_) => (text.into(), Tone::Muted),
        })
    }

    /// Short notes before a row's state: hidden, default.
    pub(in crate::client::shell) fn suffix(&self, row: ListRow) -> String {
        let mut notes = Vec::new();
        if let ListRow::Remote(index) = row {
            if self.hidden.contains(&self.profiles[index].id) {
                notes.push("hidden");
            }
        }
        if self.is_default(row) {
            notes.push("default");
        }
        notes.join(" · ")
    }

    /// The state named in the details header.
    pub(in crate::client::shell) fn state_text(&self, index: usize) -> &'static str {
        let profile = &self.profiles[index];
        if self.binding_of(index).is_none() {
            return if self.view.connected.contains(&profile.id) {
                "connected"
            } else if !profile.enabled {
                "disabled"
            } else {
                "offline"
            };
        }
        match self.state_of(index) {
            Some(MachineState::Running)
                if !profile.enabled && !self.hidden.contains(&profile.id) =>
            {
                "stopping"
            }
            Some(MachineState::Unknown) | None => "state unknown",
            Some(state) => state.as_str(),
        }
    }
}

/// `2026-10-03T08:00:00Z` → `2026-10-03`.
pub(in crate::client::shell) fn date(created_at: &str) -> &str {
    created_at.get(..10).unwrap_or(created_at)
}

/// `4294967296` → `4.0 GiB`.
pub(in crate::client::shell) fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

fn images_error(error: &HangarError) -> String {
    if error.needs_sign_in() {
        "Sign in on the account tab to list your images.".into()
    } else {
        format!("Could not list images: {error}")
    }
}

fn usage_error(error: &HangarError) -> String {
    if error.needs_sign_in() {
        "Sign in to see your usage.".into()
    } else {
        format!("Could not read usage: {error}")
    }
}

#[cfg(not(test))]
fn fetch_images() -> Result<ImageList, String> {
    let server = crate::hangar::default_server();
    backend::hangar::list_images(&server)
        .map(|images| ImageList { server, images })
        .map_err(|error| images_error(&error))
}

/// Unit tests never reach a real hangar server.
#[cfg(test)]
fn fetch_images() -> Result<ImageList, String> {
    Err(images_error(&HangarError::Invalid(
        "disabled in unit tests".into(),
    )))
}

#[cfg(not(test))]
fn fetch_usage() -> Result<Usage, String> {
    backend::hangar::usage(&crate::hangar::default_server()).map_err(|error| usage_error(&error))
}

#[cfg(test)]
fn fetch_usage() -> Result<Usage, String> {
    Err(usage_error(&HangarError::Invalid(
        "disabled in unit tests".into(),
    )))
}

impl ClientShellState {
    fn remotes_dialog_mut(&mut self) -> Option<&mut LocationDialog> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::Locations(dialog)) if dialog.remotes_open() => Some(dialog),
            _ => None,
        }
    }

    /// The dialog that holds the view, also while a form or confirmation opened from it
    /// is shown.
    fn view_dialog_mut(&mut self) -> Option<&mut LocationDialog> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::Locations(dialog))
                if !matches!(dialog.kind, LocationDialogKind::New) =>
            {
                Some(dialog)
            }
            _ => None,
        }
    }

    /// Switches between the remotes, images and account tabs and fetches what the tab
    /// shows.
    pub(in crate::client::shell) fn switch_remotes_tab(&mut self, tab: RemotesTab) {
        let Some(dialog) = self.remotes_dialog_mut() else {
            return;
        };
        if dialog.view.tab != tab {
            dialog.view.tab = tab;
            dialog.view.focus = Focus::List;
            if !dialog.busy {
                dialog.message.clear();
            }
        }
        match tab {
            RemotesTab::Remotes => {}
            RemotesTab::Images => self.request_images(),
            RemotesTab::Account => {
                self.request_usage();
                self.refresh_account();
            }
        }
    }

    pub(in crate::client::shell) fn request_images(&mut self) {
        if self.locations.images.is_some() {
            return;
        }
        let (send, receive) = mpsc::channel();
        self.locations.images = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let _ = send.send(fetch_images());
        });
    }

    pub(in crate::client::shell) fn request_usage(&mut self) {
        if self.locations.usage.is_some() {
            return;
        }
        let (send, receive) = mpsc::channel();
        self.locations.usage = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let _ = send.send(fetch_usage());
        });
    }

    /// Applies finished image and usage fetches of the current dialog; results of an
    /// older dialog are dropped, and a tab whose data is missing asks again.
    pub(in crate::client::shell) fn tick_remotes_view(&mut self, outcome: &mut ClientShellInput) {
        let epoch = self.locations.epoch;
        if let Some((job_epoch, result)) = take_result(&mut self.locations.images) {
            if let (true, Some(dialog)) = (job_epoch == epoch, self.view_dialog_mut()) {
                dialog.view.images = Some(result);
                outcome.repaint = true;
            }
        }
        if let Some((job_epoch, result)) = take_result(&mut self.locations.usage) {
            if let (true, Some(dialog)) = (job_epoch == epoch, self.view_dialog_mut()) {
                dialog.view.usage = Some(result);
                outcome.repaint = true;
            }
        }
        let missing = self.remotes_dialog_mut().map(|dialog| {
            (
                dialog.view.tab == RemotesTab::Images && dialog.view.images.is_none(),
                dialog.view.tab == RemotesTab::Account && dialog.view.usage.is_none(),
            )
        });
        match missing {
            Some((true, _)) => self.request_images(),
            Some((_, true)) => self.request_usage(),
            _ => {}
        }
    }

    /// Marks SSH remotes whose connection is online. Cheap; called with each sync.
    pub(in crate::client::shell) fn refresh_remote_connections(&mut self) {
        let connected = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
            .filter_map(|endpoint| match &endpoint.endpoint_id {
                ClientEndpointId::Ssh(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        if let Some(dialog) = self.remotes_dialog_mut() {
            dialog.view.connected = connected;
        }
    }

    /// Keys of the remotes, images and account tabs. Like on every settings tab, ←/→,
    /// h/l and tab/shift-tab move between tabs (also while busy); ↑/↓ (k/j) move in the
    /// focused pane, ↵ moves from the list to the actions and runs an action, esc goes
    /// back to the list and then closes Settings.
    pub(in crate::client::shell) fn route_remotes_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(dialog) = self.remotes_dialog_mut() else {
            return false;
        };
        outcome.repaint = true;
        let busy = dialog.busy;
        let tab = dialog.view.tab;
        let focus = dialog.view.focus;
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        match code {
            KeyCode::Esc => {
                if !busy && focus == Focus::Actions && tab != RemotesTab::Account {
                    dialog.view.focus = Focus::List;
                } else {
                    self.close_location();
                }
                return true;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') if plain => {
                self.move_from_remotes_tab(1, outcome);
                return true;
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h')
                if modifiers
                    .difference(crossterm::event::KeyModifiers::SHIFT)
                    .is_empty() =>
            {
                self.move_from_remotes_tab(-1, outcome);
                return true;
            }
            _ if busy || !plain => return true,
            _ => {}
        }
        match code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char('k') | KeyCode::Char('j') => {
                let delta = if matches!(code, KeyCode::Up | KeyCode::Char('k')) {
                    -1
                } else {
                    1
                };
                match (tab, focus) {
                    (RemotesTab::Remotes, Focus::List) => dialog.move_list(delta),
                    (RemotesTab::Remotes, Focus::Actions) => dialog.move_action(delta),
                    (RemotesTab::Images, Focus::List) => dialog.move_image(delta),
                    (RemotesTab::Images, Focus::Actions) => {
                        dialog.view.image_action =
                            step(&ImageAction::ALL, dialog.view.image_action, delta)
                    }
                    (RemotesTab::Account, _) => {
                        dialog.view.account_action = step(
                            &dialog.account_actions(),
                            dialog.selected_account_action(),
                            delta,
                        )
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_remotes(outcome),
            _ => {}
        }
        true
    }

    /// ↵: a list item moves to its actions (Add remote opens), an action runs.
    pub(in crate::client::shell) fn activate_remotes(&mut self, outcome: &mut ClientShellInput) {
        let Some(dialog) = self.remotes_dialog_mut() else {
            return;
        };
        if dialog.busy {
            return;
        }
        match (dialog.view.tab, dialog.view.focus) {
            (RemotesTab::Remotes, Focus::List) if dialog.selected_row() == ListRow::Add => {
                self.open_add_remote()
            }
            (RemotesTab::Remotes | RemotesTab::Images, Focus::List) => {
                if can_focus_actions(dialog) {
                    dialog.view.focus = Focus::Actions;
                }
            }
            (RemotesTab::Remotes, Focus::Actions) => {
                if let Some(entry) = dialog.selected_action() {
                    self.run_remote_action(entry.action, outcome);
                }
            }
            (RemotesTab::Images, Focus::Actions) => {
                let action = dialog.view.image_action;
                self.run_image_action(action);
            }
            (RemotesTab::Account, _) => {
                let action = dialog.selected_account_action();
                self.run_account_action(action);
            }
        }
    }

    /// A click in Settings → Remotes; false when it hit nothing of the view.
    pub(in crate::client::shell) fn click_remotes(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hit) = self
            .hits
            .remotes
            .iter()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, hit)| *hit)
        else {
            return false;
        };
        let Some(dialog) = self.remotes_dialog_mut() else {
            return false;
        };
        outcome.repaint = true;
        if dialog.busy {
            return true;
        }
        match hit {
            RemotesHit::Row(row) => {
                dialog.select_row(row);
                dialog.view.focus = Focus::List;
                if row == ListRow::Add {
                    self.open_add_remote();
                }
            }
            RemotesHit::Action(action) => {
                dialog.select_action(action);
                dialog.view.focus = Focus::Actions;
                self.run_remote_action(action, outcome);
            }
            RemotesHit::Image(index) => {
                if let Some(image) = dialog.images().get(index) {
                    dialog.view.image = Some(image.id.clone());
                }
                dialog.view.focus = Focus::List;
            }
            RemotesHit::ImageAction(action) => {
                dialog.view.image_action = action;
                dialog.view.focus = Focus::Actions;
                self.run_image_action(action);
            }
            RemotesHit::AccountAction(action) => {
                dialog.view.account_action = action;
                self.run_account_action(action);
            }
        }
        true
    }

    /// Runs an action of the selected remote, or says why it does not apply now.
    pub(in crate::client::shell) fn run_remote_action(
        &mut self,
        action: RemoteAction,
        outcome: &mut ClientShellInput,
    ) {
        outcome.repaint = true;
        let Some(dialog) = self.remotes_dialog_mut() else {
            return;
        };
        let Some(entry) = dialog
            .actions()
            .into_iter()
            .find(|entry| entry.action == action)
        else {
            return;
        };
        if let Some(reason) = entry.reason {
            dialog.message = format!("{}: {reason}.", entry.label.trim_end_matches('…'));
            return;
        }
        let profile = dialog.profile().cloned();
        let options = profile
            .as_ref()
            .and_then(|p| dialog.prefs.remotes.get(&p.id))
            .cloned()
            .unwrap_or_default();
        let result = match (action, profile) {
            (RemoteAction::Default, profile) => {
                let result = (|| {
                    let _guard = backend::operation_lock()?;
                    if let Some(profile) = &profile {
                        backend::validate_binding(profile, options.cloud.as_ref())?;
                    }
                    let mut prefs = LocationPreferences::load()?;
                    prefs.default_profile = profile.as_ref().map(|p| p.id.clone());
                    prefs.store()
                })();
                if result.is_ok() {
                    self.reload_location_list();
                    if let Some(dialog) = self.remotes_dialog_mut() {
                        dialog.message =
                            format!("New workspace defaults to {}", dialog.location_label());
                    }
                }
                result
            }
            (_, Some(profile)) => self.remote_location_action(action, profile, options),
            (_, None) => Err("Select a remote first".into()),
        };
        if let Err(error) = result {
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                dialog.message = error;
            }
        }
    }

    fn run_image_action(&mut self, action: ImageAction) {
        let Some(dialog) = self.remotes_dialog_mut() else {
            return;
        };
        let Some(image) = dialog.selected_image().cloned() else {
            if let Some(reason) = dialog.image_reason() {
                dialog.message = format!("{reason}.");
            }
            return;
        };
        match action {
            ImageAction::NewMachine => self.open_add_remote_from(MachineSource::Image {
                id: image.id,
                name: image.name,
            }),
            ImageAction::Delete => {
                let server = match &dialog.view.images {
                    Some(Ok(list)) => list.server.clone(),
                    _ => return,
                };
                let request = ImageDeleteRequest {
                    server,
                    id: image.id,
                    name: image.name,
                };
                dialog.message = backend::image_delete_confirmation(&request.name);
                dialog.kind = LocationDialogKind::DeleteImage(Box::new(request));
                dialog.selected = 0;
            }
        }
    }
}

fn can_focus_actions(dialog: &LocationDialog) -> bool {
    match dialog.view.tab {
        RemotesTab::Remotes => !dialog.actions().is_empty(),
        RemotesTab::Images => dialog.selected_image().is_some(),
        RemotesTab::Account => true,
    }
}

fn step<T: Copy + PartialEq>(all: &[T], current: T, delta: isize) -> T {
    let position = all.iter().position(|item| *item == current).unwrap_or(0);
    all[(position as isize + delta).clamp(0, all.len() as isize - 1) as usize]
}

type Job<T> = Option<(u64, mpsc::Receiver<Result<T, String>>)>;

/// A finished job's epoch and result; a worker that died reports an error.
fn take_result<T>(job: &mut Job<T>) -> Option<(u64, Result<T, String>)> {
    let (epoch, receiver) = job.as_ref()?;
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Disconnected) => Err("The request stopped unexpectedly.".into()),
        Err(mpsc::TryRecvError::Empty) => return None,
    };
    let epoch = *epoch;
    *job = None;
    Some((epoch, result))
}

#[cfg(test)]
mod tests;
