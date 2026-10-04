use super::*;
use crate::client::endpoint::{EndpointCatalog, ProfileId};
use crate::client::locations::hangar::{
    MachineDetails, SaveCheck, SavePlan, SnapshotUse, CLONE_CONTENTS, IMAGE_CONTENTS,
};
use crate::client::locations::{self as backend, LocationPreferences, RemoteOptions};
use crate::hangar::api::MachineState;
use crate::hangar::auth::AccountStatus;
use crate::hangar::binding::HangarBinding;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(super) mod account;
pub(super) mod add;
pub(in crate::client::shell) mod view;
use view::{RemoteAction, RemotesView};

#[derive(Debug)]
pub(super) enum LocationDialogKind {
    /// Settings → Remotes; the forms and confirmations below it return here.
    Manage,
    Add(Box<add::AddRemoteForm>),
    Edit(Option<ProfileId>),
    Stop,
    Suspend,
    Delete(Box<DeleteRequest>),
    /// Copy machine…: Clone now or Save as image, then that choice's form.
    Copy(Box<CopyRequest>),
    DeleteImage(Box<ImageDeleteRequest>),
    /// Confirms Sign out….
    SignOut,
    /// Sign up with invite code…: the invite code, then the sign-in flow with it.
    SignUp,
}

/// The two ways Copy machine… copies a hangar machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CopyChoice {
    /// A second machine with both disks (hangar's fork).
    #[default]
    Clone,
    /// The root disk as a reusable image.
    Image,
}

impl CopyChoice {
    pub const ALL: [Self; 2] = [Self::Clone, Self::Image];

    pub fn other(self) -> Self {
        match self {
            Self::Clone => Self::Image,
            Self::Image => Self::Clone,
        }
    }

    pub fn usage(self) -> SnapshotUse {
        match self {
            Self::Clone => SnapshotUse::Fork,
            Self::Image => SnapshotUse::Image,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::Clone => 0,
            Self::Image => 1,
        }
    }
}

/// Copy machine… for a hangar remote. The chooser comes first; once a choice is made,
/// its form is shown and the machine check runs on a worker.
#[derive(Clone, Debug)]
pub(super) struct CopyRequest {
    pub profile: SavedSshEndpoint,
    pub options: RemoteOptions,
    pub machine_name: String,
    pub choice: CopyChoice,
    /// The form of `choice` is shown; otherwise the chooser.
    pub chosen: bool,
    /// `None` while the machine is being checked.
    pub check: Option<SaveCheck>,
    /// The last failed attempt, kept visible while the machine is checked again.
    pub error: Option<String>,
}

impl CopyRequest {
    pub fn new(profile: SavedSshEndpoint, options: RemoteOptions, machine_name: String) -> Self {
        Self {
            profile,
            options,
            machine_name,
            choice: CopyChoice::Clone,
            chosen: false,
            check: None,
            error: None,
        }
    }

    pub fn plan(&self) -> Option<SavePlan> {
        self.check.as_ref().and_then(|check| check.plan)
    }

    /// What the form says: a failure, how the machine becomes copyable, and what the
    /// choice copies.
    pub fn message(&self) -> String {
        let status = match &self.check {
            None => format!("Checking {}…", self.machine_name),
            Some(check) => check.note.clone(),
        };
        let contents = match self.choice {
            CopyChoice::Clone => CLONE_CONTENTS,
            CopyChoice::Image => IMAGE_CONTENTS,
        };
        [self.error.as_deref().unwrap_or(""), &status, contents]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn primary_label(&self) -> &'static str {
        let stops = matches!(self.plan(), Some(SavePlan::Stop | SavePlan::ResumeThenStop));
        match (self.choice, stops) {
            (CopyChoice::Clone, true) => " ↵ stop machine and clone ",
            (CopyChoice::Clone, false) => " ↵ clone ",
            (CopyChoice::Image, true) => " ↵ stop machine and save ",
            (CopyChoice::Image, false) => " ↵ save ",
        }
    }
}

/// Delete image… from the Images view or Add remote's source picker.
#[derive(Clone, Debug)]
pub(super) struct ImageDeleteRequest {
    pub server: String,
    pub id: String,
    pub name: String,
}

/// A confirmed Delete machine… deletes this hangar machine.
#[derive(Clone, Debug)]
pub(super) struct DeleteRequest {
    pub binding: HangarBinding,
    pub profile: SavedSshEndpoint,
    pub options: RemoteOptions,
}

#[derive(Debug)]
pub(super) struct LocationDialog {
    pub kind: LocationDialogKind,
    pub fields: Vec<TextEditor>,
    pub selected: usize,
    pub location: usize,
    pub profiles: Vec<SavedSshEndpoint>,
    pub prefs: LocationPreferences,
    pub message: String,
    pub busy: bool,
    /// Last known hangar machine states by (server, machine ID); presentation only,
    /// refreshed by a worker. Missing means unknown.
    pub machine_states: BTreeMap<(String, String), MachineState>,
    /// hangar machines hidden from the sidebar (still listed here).
    pub hidden: BTreeSet<ProfileId>,
    /// Why a hangar machine's listing may be outdated (offline, signed out).
    pub sync_notes: BTreeMap<ProfileId, &'static str>,
    /// Last known hangar sign-in; presentation only, refreshed by a worker. `None`
    /// while it is being checked.
    pub account: Option<AccountStatus>,
    /// Settings → Remotes: sub-view, selection and fetched data. Kept while a form or
    /// confirmation opened from it is shown.
    pub view: Box<RemotesView>,
}

impl LocationDialog {
    pub fn title(&self) -> String {
        match &self.kind {
            LocationDialogKind::Manage => "remotes".into(),
            LocationDialogKind::Add(_) => "add remote".into(),
            LocationDialogKind::Edit(_) => "remote settings".into(),
            LocationDialogKind::Stop => "stop machine".into(),
            LocationDialogKind::Suspend => "suspend machine".into(),
            LocationDialogKind::Delete(_) => "delete machine".into(),
            LocationDialogKind::Copy(request) => match (request.chosen, request.choice) {
                (false, _) => format!("copy {}", request.machine_name),
                (true, CopyChoice::Clone) => format!("clone {}", request.machine_name),
                (true, CopyChoice::Image) => format!("save {} as image", request.machine_name),
            },
            LocationDialogKind::DeleteImage(_) => "delete image".into(),
            LocationDialogKind::SignOut => "sign out of hangar".into(),
            LocationDialogKind::SignUp => "sign up for hangar".into(),
        }
    }
    pub fn labels(&self) -> &[&str] {
        match &self.kind {
            LocationDialogKind::Add(_) => &["Provider", "Machine", "Name", "Source", ""],
            LocationDialogKind::Edit(_) => {
                &["Name", "SSH target", "Herdr session", "Default directory"]
            }
            LocationDialogKind::Copy(request) => match (request.chosen, request.choice) {
                (false, _) => &[],
                (true, CopyChoice::Clone) => &["Name"],
                (true, CopyChoice::Image) => &["Image name", "Description"],
            },
            LocationDialogKind::SignUp => &["Invite code"],
            LocationDialogKind::Manage
            | LocationDialogKind::Stop
            | LocationDialogKind::Suspend
            | LocationDialogKind::Delete(_)
            | LocationDialogKind::DeleteImage(_)
            | LocationDialogKind::SignOut => &[],
        }
    }

    /// The Copy machine… chooser is shown.
    pub fn copy_chooser(&self) -> Option<&CopyRequest> {
        match &self.kind {
            LocationDialogKind::Copy(request) if !request.chosen => Some(request),
            _ => None,
        }
    }
    /// The selected remote's hangar machine state, when known.
    pub fn machine_state(&self) -> Option<MachineState> {
        let binding = self
            .profile()
            .and_then(|profile| self.prefs.binding(&profile.id))?
            .hangar();
        self.machine_states
            .get(&(binding.server.clone(), binding.machine_id.clone()))
            .copied()
    }

    pub fn profile(&self) -> Option<&SavedSshEndpoint> {
        self.location
            .checked_sub(1)
            .and_then(|i| self.profiles.get(i))
    }

    /// Takes a freshly loaded list, keeping the selected remote while it is listed.
    fn apply_snapshot(&mut self, snapshot: RemotesSnapshot) {
        self.prefs = snapshot.prefs;
        if let Some(details) = snapshot.details {
            self.view.details.extend(details);
        }
        self.machine_states = snapshot.machine_states;
        self.hidden = snapshot.hidden;
        self.sync_notes = snapshot.sync_notes;
        self.replace_profiles(snapshot.profiles);
    }

    fn replace_profiles(&mut self, profiles: Vec<SavedSshEndpoint>) {
        let selected = self.profile().map(|p| p.id.clone());
        self.location = selected
            .and_then(|id| profiles.iter().position(|p| p.id == id))
            .map_or(0, |i| i + 1);
        self.profiles = profiles;
    }
    pub fn location_label(&self) -> String {
        self.profile()
            .map(|p| {
                let mut label = p.label.clone();
                if self.prefs.binding(&p.id).is_some() {
                    match self.machine_state() {
                        Some(MachineState::Running)
                            if !p.enabled && !self.hidden.contains(&p.id) =>
                        {
                            label.push_str(" (stopping)")
                        }
                        Some(MachineState::Running) => {}
                        Some(MachineState::Unknown) | None => label.push_str(" (state unknown)"),
                        Some(state) => label.push_str(&format!(" ({})", state.as_str())),
                    }
                    if self.hidden.contains(&p.id) {
                        label.push_str(" · hidden");
                    }
                    if let Some(note) = self.sync_notes.get(&p.id) {
                        label.push_str(&format!(" · {note}"));
                    }
                } else if !p.enabled {
                    label.push_str(" (disabled)");
                }
                label
            })
            .unwrap_or_else(|| "Local".into())
    }
    pub fn editor_mut(&mut self) -> Option<&mut TextEditor> {
        if self.busy
            || matches!(
                self.kind,
                LocationDialogKind::Manage
                    | LocationDialogKind::Stop
                    | LocationDialogKind::Suspend
                    | LocationDialogKind::Delete(_)
                    | LocationDialogKind::DeleteImage(_)
                    | LocationDialogKind::SignOut
            )
        {
            return None;
        }
        if let LocationDialogKind::Add(form) = &self.kind {
            // The only text field of Add remote is the new machine's name.
            return if self.selected == add::NAME_FIELD && form.edits_name() {
                self.fields.get_mut(0)
            } else {
                None
            };
        }
        self.fields.get_mut(self.selected)
    }
}

#[derive(Debug)]
enum JobResult {
    Message(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocationStamp {
    generation: Option<u64>,
    boot_id: String,
}

/// Where New workspace creates a workspace: the default machine (Settings → remotes →
/// Use as default; Local when none) with its default directory.
#[derive(Clone, Debug)]
pub(super) struct WorkspaceDestination {
    /// `None` is Local.
    pub profile: Option<SavedSshEndpoint>,
    pub options: RemoteOptions,
}

impl WorkspaceDestination {
    pub fn local() -> Self {
        Self {
            profile: None,
            options: RemoteOptions::default(),
        }
    }

    pub fn endpoint(&self) -> ClientEndpointId {
        self.profile
            .as_ref()
            .map(|p| ClientEndpointId::Ssh(p.id.clone()))
            .unwrap_or(ClientEndpointId::Local)
    }

    /// The machine's default directory; `None` lets its server choose.
    pub fn cwd(&self) -> Option<String> {
        let cwd = self.options.cwd.trim();
        (!cwd.is_empty()).then(|| cwd.to_owned())
    }
}

/// A workspace created on a machine other than the displayed one, focused once that
/// machine's snapshot lists it.
#[derive(Debug)]
struct CreatedLocationWorkspace {
    endpoint: ClientEndpointId,
    workspace: String,
    profile: Option<SavedSshEndpoint>,
    stamp: LocationStamp,
    /// The machine displayed when New workspace was chosen; focus is not taken once
    /// another one is shown.
    origin: ClientEndpointId,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub(super) struct LocationController {
    add: add::AddRemoteController,
    account: account::AccountController,
    epoch: u64,
    job: Option<(u64, mpsc::Receiver<Result<JobResult, String>>)>,
    /// New workspace on another machine: the create request, then the focus wait.
    create: Option<mpsc::Receiver<Result<CreatedLocationWorkspace, String>>>,
    created: Option<CreatedLocationWorkspace>,
    /// A hangar sync for the remotes dialog, then the reloaded list.
    sync: Option<(u64, mpsc::Receiver<Result<RemotesSnapshot, String>>)>,
    /// When the remotes dialog last asked for a sync.
    last_sync: Option<Instant>,
    /// The machine check of Copy machine…, for the choice it was made for.
    image_check: Option<(u64, SnapshotUse, SaveCheckJob)>,
    /// The images listing and usage read for Settings → Remotes.
    images: Option<(u64, mpsc::Receiver<Result<view::ImageList, String>>)>,
    usage: Option<(
        u64,
        mpsc::Receiver<Result<crate::hangar::api::Usage, String>>,
    )>,
    /// Replaces the hangar request behind Copy machine's check in tests.
    save_checker: Option<SaveChecker>,
    /// Replaces the on-disk check that the selected remote is unchanged in tests.
    binding_validator: Option<BindingValidator>,
    /// Replaces reading the remotes (and the default machine) from disk in tests.
    remotes_loader: Option<RemotesLoader>,
}

type BindingValidator =
    fn(&SavedSshEndpoint, Option<&crate::client::locations::CloudBinding>) -> Result<(), String>;

type SaveChecker = fn(&HangarBinding) -> Result<SaveCheck, crate::hangar::api::HangarError>;

type SaveCheckJob = mpsc::Receiver<Result<SaveCheck, String>>;

type RemotesLoader = fn() -> Result<RemotesSnapshot, String>;

/// The remotes as the dialog shows them, loaded from disk off the render path.
#[derive(Debug)]
pub(super) struct RemotesSnapshot {
    profiles: Vec<SavedSshEndpoint>,
    prefs: LocationPreferences,
    machine_states: BTreeMap<(String, String), MachineState>,
    hidden: BTreeSet<ProfileId>,
    sync_notes: BTreeMap<ProfileId, &'static str>,
    /// Shown when the list may be outdated.
    notice: Option<String>,
    /// Machine details from the fetch behind this snapshot; `None` when nothing was
    /// fetched.
    details: Option<BTreeMap<(String, String), MachineDetails>>,
}

/// Fetches every server's machine list (and template catalog, to tell which machines
/// can be saved or forked), then reloads the remotes.
#[cfg(not(test))]
fn sync_and_load() -> Result<RemotesSnapshot, String> {
    let (report, listed) = backend::sync::sync_now_listing();
    tracing::debug!(?report, "synced hangar machines for settings");
    let mut details = BTreeMap::new();
    for (server, machines) in &listed {
        let templates = backend::hangar::list_templates(server)
            .map_err(|error| tracing::debug!(%error, "could not read hangar templates"))
            .ok();
        for machine in machines {
            details.insert(
                (server.clone(), machine.id.clone()),
                MachineDetails::of(machine, templates.as_deref()),
            );
        }
    }
    let mut snapshot = RemotesSnapshot::load()?;
    snapshot.details = Some(details);
    Ok(snapshot)
}

/// Unit tests never reach a real hangar server; sync itself is tested with fake HTTP.
#[cfg(test)]
fn sync_and_load() -> Result<RemotesSnapshot, String> {
    Err("hangar sync is disabled in unit tests".into())
}

/// Asks the destination's server for a workspace in its default directory.
#[cfg(not(test))]
fn create_remote_workspace(
    destination: &WorkspaceDestination,
    label: String,
) -> Result<String, String> {
    backend::create_workspace(
        destination.profile.as_ref(),
        &destination.options,
        destination.options.cwd.clone(),
        label,
    )
}

/// Unit tests never reach another machine's server.
#[cfg(test)]
fn create_remote_workspace(
    _destination: &WorkspaceDestination,
    _label: String,
) -> Result<String, String> {
    Err("workspace creation on another machine is disabled in unit tests".into())
}

impl RemotesSnapshot {
    fn from_remotes(remotes: &backend::Remotes) -> Self {
        let mut notice = None;
        for remote in &remotes.hangar {
            if let Some(note) = remote.sync.note() {
                notice.get_or_insert_with(|| {
                    format!(
                        "hangar machines on {} are {note}; showing the last synced list.",
                        remote.binding.server
                    )
                });
            }
        }
        Self {
            profiles: remotes.profiles(true),
            prefs: remotes.view_prefs(),
            machine_states: remotes
                .hangar
                .iter()
                .map(|remote| {
                    (
                        (
                            remote.binding.server.clone(),
                            remote.binding.machine_id.clone(),
                        ),
                        remote.state,
                    )
                })
                .collect(),
            hidden: remotes
                .hangar
                .iter()
                .filter(|remote| remote.hidden)
                .map(|remote| remote.profile.id.clone())
                .collect(),
            sync_notes: remotes
                .hangar
                .iter()
                .filter_map(|remote| Some((remote.profile.id.clone(), remote.sync.note()?)))
                .collect(),
            notice,
            details: None,
        }
    }

    fn load() -> Result<Self, String> {
        backend::Remotes::load().map(|remotes| Self::from_remotes(&remotes))
    }
}

impl ClientShellState {
    fn location_dialog(&mut self, kind: LocationDialogKind) -> Result<LocationDialog, String> {
        let snapshot = RemotesSnapshot::load()?;
        let location = snapshot.prefs.default_index(&snapshot.profiles);
        Ok(LocationDialog {
            kind,
            fields: Vec::new(),
            selected: 0,
            location,
            profiles: snapshot.profiles,
            prefs: snapshot.prefs,
            message: String::new(),
            busy: false,
            machine_states: snapshot.machine_states,
            hidden: snapshot.hidden,
            sync_notes: snapshot.sync_notes,
            account: self.locations.account.last_status(),
            view: Box::default(),
        })
    }

    /// A dialog of Settings → Remotes that keeps the current one's view and selected
    /// remote (none when opened afresh).
    fn remotes_dialog(&mut self, kind: LocationDialogKind) -> Result<LocationDialog, String> {
        let carried = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Locations(dialog)) => {
                Some((dialog.view.clone(), dialog.profile().map(|p| p.id.clone())))
            }
            _ => None,
        };
        let mut dialog = self.location_dialog(kind)?;
        if let Some((view, selected)) = carried {
            dialog.view = view;
            dialog.location = selected
                .and_then(|id| dialog.profiles.iter().position(|p| p.id == id))
                .map_or(0, |index| index + 1);
        }
        Ok(dialog)
    }

    /// Closes a form or confirmation opened from Settings → Remotes and shows the view
    /// as it was; on the view itself closes the dialog. A Copy machine… form goes back
    /// to its chooser first.
    pub(super) fn escape_location(&mut self) {
        if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
            if let LocationDialogKind::Copy(request) = &mut dialog.kind {
                if request.chosen && !dialog.busy {
                    request.chosen = false;
                    request.check = None;
                    request.error = None;
                    dialog.fields.clear();
                    dialog.selected = 0;
                    dialog.message.clear();
                    // A check still running for the form is no longer wanted.
                    self.locations.image_check = None;
                    return;
                }
            }
        }
        let returns = matches!(
            self.overlay,
            Some(ClientShellOverlay::Locations(ref dialog))
                if !matches!(dialog.kind, LocationDialogKind::Manage)
        );
        if !returns {
            self.close_location();
            return;
        }
        self.locations.add.cancel_sign_in();
        if matches!(
            self.overlay,
            Some(ClientShellOverlay::Locations(LocationDialog {
                kind: LocationDialogKind::SignUp,
                ..
            }))
        ) {
            // Esc stops a sign-up that is waiting for the browser or a device code.
            self.locations.account.cancel_sign_in();
        }
        let busy = self.locations.job.is_some();
        if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
            dialog.kind = LocationDialogKind::Manage;
            dialog.fields.clear();
            dialog.selected = 0;
            dialog.busy = busy;
            if !busy {
                dialog.message.clear();
            }
        }
        self.refresh_remote_connections();
    }

    pub(super) fn open_locations(&mut self) {
        self.open_locations_with(None);
    }

    /// Opens Settings on the remotes, images or account tab.
    pub(super) fn open_locations_on(&mut self, tab: view::RemotesTab) {
        self.open_locations_with(Some(tab));
    }

    /// Whether opening the remotes view shows it (and not a running add or an error).
    pub(super) fn remotes_view_can_open(&self) -> bool {
        !self.locations.add.running() && self.locations.job.is_none()
    }

    /// Opens the remotes view on `tab`, or on the tab it showed last.
    fn open_locations_with(&mut self, tab: Option<view::RemotesTab>) {
        if self.locations.add.running() {
            self.open_add_remote();
            return;
        }
        if self.locations.job.is_some() {
            self.set_endpoint_error("A remote operation is still running");
            return;
        }
        match self.remotes_dialog(LocationDialogKind::Manage) {
            Ok(mut dialog) => {
                self.locations.epoch = self.locations.epoch.wrapping_add(1);
                dialog.message = LocationPreferences::take_notice().unwrap_or_default();
                let tab = tab.unwrap_or(dialog.view.tab);
                self.overlay = Some(ClientShellOverlay::Locations(dialog));
                self.locations.sync = None;
                self.sync_machines();
                self.refresh_account();
                self.switch_remotes_tab(tab);
            }
            Err(error) => self.set_endpoint_error(error),
        }
    }

    /// Syncs hangar machines on a worker, then reloads the dialog's list. A failed fetch
    /// keeps the last list (marked offline or signed out); a result for an older dialog
    /// is dropped.
    fn sync_machines(&mut self) {
        if self.locations.sync.is_some()
            || !matches!(self.overlay, Some(ClientShellOverlay::Locations(_)))
        {
            return;
        }
        self.locations.last_sync = Some(Instant::now());
        self.refresh_remote_connections();
        let (send, receive) = mpsc::channel();
        self.locations.sync = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let _ = send.send(sync_and_load());
        });
    }

    fn location_job(&mut self, job: impl FnOnce() -> Result<JobResult, String> + Send + 'static) {
        if self.locations.job.is_some() {
            return;
        }
        let (send, receive) = mpsc::channel();
        self.locations.job = Some((self.locations.epoch, receive));
        if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
            dialog.busy = true;
            dialog.message =
                "Working… Esc closes this dialog; an accepted operation continues.".into();
        }
        std::thread::spawn(move || {
            let _ = send.send(job());
        });
    }

    fn edit_location(&mut self, id: Option<ProfileId>) {
        let result = (|| {
            let mut dialog = self.remotes_dialog(LocationDialogKind::Edit(id.clone()))?;
            let profile = id
                .as_ref()
                .and_then(|id| dialog.profiles.iter().find(|p| &p.id == id));
            let options = profile
                .and_then(|p| dialog.prefs.remotes.get(&p.id))
                .cloned()
                .unwrap_or_default();
            dialog.fields = [
                profile.map_or("", |p| p.label.as_str()),
                profile.map_or("", |p| p.target.as_str()),
                profile.map_or("herdr-remote", |p| p.session.as_str()),
                &options.cwd,
            ]
            .iter()
            .map(|value| TextEditor::new(value, true))
            .collect();
            dialog.message = if options.cloud.is_some() {
                "This remote is a hangar machine; Herdr manages its SSH target. The name is shown only in Herdr (empty restores the hangar name).".into()
            } else {
                "Use an existing SSH alias. hangar machines appear on their own; create one from Add remote.".into()
            };
            Ok::<_, String>(dialog)
        })();
        match result {
            Ok(dialog) => self.overlay = Some(ClientShellOverlay::Locations(dialog)),
            Err(error) => self.set_endpoint_error(error),
        }
    }

    fn save_location(&mut self) -> Result<(), String> {
        {
            let _guard = backend::operation_lock()?;
            let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_ref() else {
                return Ok(());
            };
            let LocationDialogKind::Edit(id) = &dialog.kind else {
                return Ok(());
            };
            let old_binding = id.as_ref().and_then(|id| dialog.prefs.binding(id));
            if let Some(id) = id {
                let old = dialog
                    .profiles
                    .iter()
                    .find(|p| &p.id == id)
                    .ok_or("Remote was removed")?;
                backend::validate_binding(old, old_binding)?;
            }
            let values = dialog
                .fields
                .iter()
                .map(|f| f.trim().to_owned())
                .collect::<Vec<_>>();
            if let (Some(id), Some(cloud)) = (id, old_binding) {
                let old = dialog
                    .profiles
                    .iter()
                    .find(|p| &p.id == id)
                    .ok_or("Remote was removed")?;
                if values[1] != old.target {
                    return Err("Herdr manages the SSH target of a hangar machine".into());
                }
                RemoteOptions {
                    cwd: values[3].clone(),
                    cloud: None,
                }
                .validate()?;
                let binding = cloud.hangar().clone();
                drop(_guard);
                backend::edit_machine_prefs(
                    &binding,
                    Some(&values[0]),
                    Some(&values[2]),
                    Some(&values[3]),
                )?;
                self.open_locations();
                return Ok(());
            }
            let mut profile = SavedSshEndpoint::new(&values[0], &values[1], &values[2])?;
            if let Some(id) = id {
                if old_binding.is_some()
                    && dialog
                        .profiles
                        .iter()
                        .find(|p| &p.id == id)
                        .is_some_and(|p| p.target != profile.target)
                {
                    return Err("Herdr manages the SSH target of a hangar machine".into());
                }
            } else if crate::hangar::binding::is_hangar_target(&profile.target) {
                return Err("hangar machines appear on their own once you sign in; create one from Add remote → Provider: hangar".into());
            }
            let options = RemoteOptions {
                cwd: values[3].clone(),
                cloud: None,
            };
            options.validate()?;
            let mut catalog = EndpointCatalog::load()?;
            let mut prefs = LocationPreferences::load()?;
            if let Some(id) = id {
                let Some(old) = catalog.ssh.iter_mut().find(|p| &p.id == id) else {
                    return Err("Remote was removed by another client".into());
                };
                profile.id = id.clone();
                profile.enabled = old.enabled;
                *old = profile.clone();
            } else {
                if catalog.ssh.len() >= 64 {
                    return Err("At most 64 remote profiles can be saved".into());
                }
                catalog.ssh.push(profile.clone());
            }
            prefs.remotes.insert(profile.id, options);
            // Metadata first: a new endpoint must not appear with another endpoint's defaults.
            prefs.store()?;
            catalog.store_profiles()?;
        }
        self.open_locations();
        Ok(())
    }

    pub(super) fn accept_location(&mut self, outcome: &mut ClientShellInput) {
        if matches!(
            self.overlay,
            Some(ClientShellOverlay::Locations(LocationDialog {
                kind: LocationDialogKind::Add(_),
                ..
            }))
        ) {
            self.accept_add_remote(outcome);
            return;
        }
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_ref() else {
            return;
        };
        if dialog.busy {
            return;
        }
        outcome.repaint = true;
        let profile = dialog.profile().cloned();
        let options = profile
            .as_ref()
            .and_then(|p| dialog.prefs.remotes.get(&p.id))
            .cloned()
            .unwrap_or_default();
        let result = match &dialog.kind {
            LocationDialogKind::Add(_) => Ok(()),
            LocationDialogKind::Edit(_) => self.save_location(),
            LocationDialogKind::Stop => {
                if let Some(profile) = profile {
                    self.location_job(move || {
                        backend::stop_remote(&profile, &options).map(JobResult::Message)
                    });
                }
                Ok(())
            }
            LocationDialogKind::Suspend => {
                if let Some(profile) = profile {
                    self.location_job(move || {
                        backend::suspend_remote(&profile, &options).map(JobResult::Message)
                    });
                }
                Ok(())
            }
            LocationDialogKind::Copy(request) => {
                let request = (**request).clone();
                if !request.chosen {
                    self.choose_copy();
                } else {
                    match request.choice {
                        CopyChoice::Clone => self.submit_clone(request),
                        CopyChoice::Image => self.submit_save_image(request),
                    }
                }
                Ok(())
            }
            LocationDialogKind::SignOut => {
                self.location_job(|| backend::hangar::sign_out().map(JobResult::Message));
                Ok(())
            }
            LocationDialogKind::SignUp => {
                self.submit_sign_up();
                Ok(())
            }
            LocationDialogKind::DeleteImage(request) => {
                let request = (**request).clone();
                self.location_job(move || {
                    backend::delete_image(&request.server, &request.id, &request.name)
                        .map(JobResult::Message)
                });
                Ok(())
            }
            LocationDialogKind::Delete(request) => {
                let request = (**request).clone();
                self.location_job(move || {
                    backend::delete_remote(&request.profile, &request.options, &mut |_| {})
                        .map(JobResult::Message)
                });
                Ok(())
            }
            LocationDialogKind::Manage => {
                self.activate_remotes(outcome);
                Ok(())
            }
        };
        if let Err(error) = result {
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                dialog.message = error;
            }
        }
    }

    fn remote_location_action(
        &mut self,
        action: RemoteAction,
        profile: SavedSshEndpoint,
        options: RemoteOptions,
    ) -> Result<(), String> {
        let validate = self
            .locations
            .binding_validator
            .unwrap_or(backend::validate_binding);
        validate(&profile, options.cloud.as_ref())?;
        match action {
            RemoteAction::Edit => self.edit_location(Some(profile.id)),
            RemoteAction::Test => {
                if !profile.enabled {
                    return Err("Remote is disabled. Start it before testing SSH.".into());
                }
                self.location_job(move || {
                    crate::remote::check_saved_ssh(&profile.target, &profile.session)
                        .map(|()| JobResult::Message("SSH and Herdr session are ready".into()))
                        .map_err(|error| error.to_string())
                });
            }
            RemoteAction::Start => {
                let resuming = matches!(
                    self.overlay.as_ref(),
                    Some(ClientShellOverlay::Locations(dialog))
                        if dialog.machine_state() == Some(MachineState::Suspended)
                );
                // hangar's start resumes a suspended machine from its snapshot.
                self.location_job(move || {
                    backend::start_remote(&profile, &options)?;
                    Ok(JobResult::Message(if resuming {
                        "Remote resumed; automatic connection enabled".into()
                    } else {
                        "Remote is ready; automatic connection enabled".into()
                    }))
                })
            }
            RemoteAction::Suspend => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Suspend remote requires a hangar machine.".into());
                };
                let name = cloud.hangar().machine_name.clone();
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.kind = LocationDialogKind::Suspend;
                    dialog.selected = 0;
                    dialog.message = format!("Suspend hangar machine {name}? Its memory is saved to a snapshot, so running programs and Herdr sessions continue after Resume. (Stop machine… shuts everything down instead; only files on disk remain.) It does not reconnect until it is resumed.");
                }
            }
            RemoteAction::Stop => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Stop machine requires a hangar machine. Manage SSH-only server lifetime on its host.".into());
                };
                let name = cloud.hangar().machine_name.clone();
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.kind = LocationDialogKind::Stop;
                    dialog.selected = 0;
                    dialog.message = format!("Stop hangar machine {name}? All sessions and jobs on this machine stop. Files on its persistent disk remain; running processes do not survive (Suspend… keeps them). It does not reconnect until it is started again.");
                }
            }
            RemoteAction::Copy => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Copy machine requires a hangar machine.".into());
                };
                let machine_name = cloud.hangar().machine_name.clone();
                self.show_copy(CopyRequest::new(profile, options, machine_name));
            }
            RemoteAction::Remove => match options.cloud.clone() {
                Some(cloud) => self.confirm_delete(DeleteRequest {
                    binding: cloud.hangar().clone(),
                    profile,
                    options,
                }),
                None => {
                    backend::remove_remote(&profile, &options)?;
                    self.open_locations();
                }
            },
            RemoteAction::Hide => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Hide from sidebar applies to hangar machines. An SSH remote can be removed, or disabled with `herdr machine disable`.".into());
                };
                let hidden = matches!(
                    self.overlay.as_ref(),
                    Some(ClientShellOverlay::Locations(dialog)) if dialog.hidden.contains(&profile.id)
                );
                backend::set_hidden(cloud.hangar(), !hidden)?;
                let name = profile.label.clone();
                self.reload_location_list();
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.message = if hidden {
                        format!("{name} is shown in the sidebar again and connects while it is running.")
                    } else {
                        format!("{name} is hidden from the sidebar and not connected. It stays listed here; Show in sidebar brings it back.")
                    };
                }
            }
            RemoteAction::Default => {}
        }
        Ok(())
    }

    /// Switches the dialog to the Copy machine… chooser.
    fn show_copy(&mut self, request: CopyRequest) {
        if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
            dialog.message.clear();
            dialog.kind = LocationDialogKind::Copy(Box::new(request));
            dialog.fields.clear();
            dialog.selected = 0;
        }
    }

    /// The chooser's other choice.
    pub(super) fn switch_copy_choice(&mut self, choice: CopyChoice) {
        if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
            if let LocationDialogKind::Copy(request) = &mut dialog.kind {
                if !request.chosen {
                    request.choice = choice;
                }
            }
        }
    }

    /// Continues from the chooser to the chosen form (clone: the suggested
    /// `<source>-clone` name; image: empty name and description) and checks the
    /// machine.
    fn choose_copy(&mut self) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        let LocationDialogKind::Copy(request) = &mut dialog.kind else {
            return;
        };
        request.chosen = true;
        request.check = None;
        request.error = None;
        dialog.fields = match request.choice {
            CopyChoice::Clone => vec![TextEditor::new(
                &backend::hangar::default_clone_name(&request.machine_name),
                false,
            )],
            CopyChoice::Image => vec![TextEditor::new("", false), TextEditor::new("", false)],
        };
        dialog.message = request.message();
        dialog.selected = 0;
        self.check_save_image();
    }

    /// Reads the machine's state and template on a worker for the chosen Copy machine…
    /// form; the result decides whether the form proceeds directly, stops first, or
    /// explains why it cannot proceed.
    fn check_save_image(&mut self) {
        let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::Copy(request),
            ..
        })) = self.overlay.as_ref()
        else {
            return;
        };
        if !request.chosen {
            return;
        }
        let Some(cloud) = request.options.cloud.clone() else {
            return;
        };
        let usage = request.choice.usage();
        let checker: SaveChecker = match usage {
            SnapshotUse::Image => backend::hangar::check_save,
            SnapshotUse::Fork => backend::hangar::check_fork,
        };
        let checker = self.locations.save_checker.unwrap_or(checker);
        let (send, receive) = mpsc::channel();
        self.locations.image_check = Some((self.locations.epoch, usage, receive));
        std::thread::spawn(move || {
            let result = checker(cloud.hangar()).map_err(|error| {
                format!(
                    "Could not check {}: {error} Close and reopen Copy machine… to retry.",
                    cloud.hangar().machine_name
                )
            });
            let _ = send.send(result);
        });
    }

    fn submit_save_image(&mut self, request: CopyRequest) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        let name = dialog
            .fields
            .first()
            .map(|field| field.trim().to_owned())
            .unwrap_or_default();
        let description = dialog
            .fields
            .get(1)
            .map(|field| field.trim().to_owned())
            .unwrap_or_default();
        let Some(plan) = request.plan() else {
            // Still checking, or the machine cannot be saved; the message says which.
            return;
        };
        let invalid = backend::hangar::validate_image_name(&name)
            .err()
            .or_else(|| {
                (description.len() > 1000)
                    .then(|| "The description can be at most 1000 characters.".to_owned())
            });
        if let Some(error) = invalid {
            dialog.selected = if description.len() > 1000 && !name.is_empty() {
                1
            } else {
                0
            };
            dialog.message = CopyRequest {
                error: Some(error),
                ..request
            }
            .message();
            return;
        }
        self.location_job(move || {
            backend::save_image_remote(
                &request.profile,
                &request.options,
                plan,
                &name,
                &description,
            )
            .map(JobResult::Message)
        });
    }

    fn submit_clone(&mut self, request: CopyRequest) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        let name = dialog
            .fields
            .first()
            .map(|field| field.trim().to_owned())
            .unwrap_or_default();
        let Some(plan) = request.plan() else {
            // Still checking, or the machine cannot be cloned; the message says which.
            return;
        };
        if let Err(error) = backend::hangar::validate_fork_name(&name) {
            dialog.selected = 0;
            dialog.message = CopyRequest {
                error: Some(error),
                ..request
            }
            .message();
            return;
        }
        self.location_job(move || {
            backend::fork_remote(&request.profile, &request.options, plan, &name)
                .map(JobResult::Message)
        });
    }

    /// Shows what a confirmed delete destroys: the machine, its disks and snapshots.
    pub(super) fn confirm_delete(&mut self, request: DeleteRequest) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        dialog.message = backend::delete_confirmation(&request.binding.machine_name);
        dialog.kind = LocationDialogKind::Delete(Box::new(request));
        dialog.selected = 0;
    }

    /// Reloads the list from disk (after a local preference change).
    fn reload_location_list(&mut self) {
        match RemotesSnapshot::load() {
            Ok(snapshot) => {
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.apply_snapshot(snapshot);
                }
            }
            Err(error) => self.set_endpoint_error(error),
        }
    }

    pub(super) fn close_location(&mut self) {
        self.locations.epoch = self.locations.epoch.wrapping_add(1);
        self.locations.add.cancel_sign_in();
        self.locations.account.cancel_sign_in();
        self.overlay = None;
    }

    pub(super) fn route_location_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.route_add_remote_key(key, outcome) || self.route_remotes_key(key, outcome) {
            return true;
        }
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return false;
        };
        outcome.repaint = true;
        if key.code == KeyCode::Esc {
            self.escape_location();
            return true;
        }
        if dialog.busy {
            return true;
        }
        if let Some(request) = dialog.copy_chooser() {
            // The chooser: ←/→ (h/l, tab) switch, ↵ continues.
            let choice = request.choice;
            match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('h')
                | KeyCode::Char('l') => self.switch_copy_choice(choice.other()),
                KeyCode::Enter | KeyCode::Char(' ') => self.accept_location(outcome),
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                dialog.selected = (dialog.selected + 1) % (dialog.labels().len() + 1)
            }
            KeyCode::BackTab | KeyCode::Up => {
                dialog.selected =
                    (dialog.selected + dialog.labels().len()) % (dialog.labels().len() + 1)
            }
            KeyCode::Enter => self.accept_location(outcome),
            _ => {
                if let Some(editor) = dialog.editor_mut() {
                    editor.handle_key(key);
                }
            }
        }
        true
    }

    fn location_stamp(&self, endpoint: &ClientEndpointId) -> Option<LocationStamp> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|e| &e.endpoint_id == endpoint && e.status == ClientEndpointStatus::Online)?;
        Some(LocationStamp {
            generation: endpoint.snapshot_generation,
            boot_id: endpoint.snapshot.as_ref()?.boot_id.clone(),
        })
    }

    /// New workspace, from the keybinding, the sidebar and the mobile menu: on the
    /// default machine with its default directory, after the name prompt when
    /// `prompt_new_workspace_name` is set. A default machine that is not connected is
    /// never started; a notice says so and nothing is created.
    pub(super) fn new_workspace(&mut self, outcome: &mut ClientShellInput) {
        outcome.repaint = true;
        if self.locations.create.is_some() {
            self.set_endpoint_error("A new workspace is still being created");
            return;
        }
        let load = self
            .locations
            .remotes_loader
            .unwrap_or(RemotesSnapshot::load);
        let destination = match load() {
            Ok(snapshot) => self.new_workspace_destination(snapshot),
            Err(error) => {
                // Remote settings that cannot be read never block a local workspace.
                tracing::warn!(%error, "remote settings are unavailable; new workspace uses Local");
                Ok(WorkspaceDestination::local())
            }
        };
        match destination {
            Err(notice) => self.set_endpoint_error(notice),
            Ok(destination) if self.config.prompt_new_workspace_name => {
                self.open_new_workspace_overlay(destination)
            }
            Ok(destination) => self.create_workspace_at(destination, None, outcome),
        }
    }

    /// The default machine when it can host a new workspace now; otherwise the notice
    /// to show.
    fn new_workspace_destination(
        &self,
        snapshot: RemotesSnapshot,
    ) -> Result<WorkspaceDestination, String> {
        let index = snapshot.prefs.default_index(&snapshot.profiles);
        let Some(profile) = index
            .checked_sub(1)
            .and_then(|index| snapshot.profiles.get(index))
            .cloned()
        else {
            return Ok(WorkspaceDestination::local());
        };
        let options = snapshot
            .prefs
            .remotes
            .get(&profile.id)
            .cloned()
            .unwrap_or_default();
        let endpoint = ClientEndpointId::Ssh(profile.id.clone());
        if profile.enabled && self.endpoint_is_online(&endpoint) {
            return Ok(WorkspaceDestination {
                profile: Some(profile),
                options,
            });
        }
        let label = &profile.label;
        Err(if snapshot.hidden.contains(&profile.id) {
            format!("Default machine {label} is hidden from the sidebar — show it in Settings → remotes, or choose another default.")
        } else {
            format!("Default machine {label} isn't connected — start it in Settings → remotes, or choose another default.")
        })
    }

    /// Creates a workspace on `destination`. On the displayed machine this is the
    /// usual request (next to the current workspace); on another machine its server is
    /// asked directly and the workspace is focused once that machine lists it.
    pub(super) fn create_workspace_at(
        &mut self,
        destination: WorkspaceDestination,
        label: Option<String>,
        outcome: &mut ClientShellInput,
    ) {
        let endpoint = destination.endpoint();
        if endpoint == self.active_endpoint_id {
            self.push_endpoint_method(
                crate::api::schema::Method::WorkspaceCreate(
                    crate::api::schema::WorkspaceCreateParams {
                        source_workspace_id: self.workspace_action_id(),
                        cwd: destination.cwd(),
                        focus: true,
                        label,
                        env: Default::default(),
                    },
                ),
                outcome,
            );
            return;
        }
        let name = self.endpoint_label(&endpoint).to_owned();
        let Some(stamp) = self.location_stamp(&endpoint) else {
            self.set_endpoint_error(format!("{name} isn't connected; no workspace was created."));
            return;
        };
        let origin = self.active_endpoint_id.clone();
        let (send, receive) = mpsc::channel();
        self.locations.create = Some(receive);
        std::thread::spawn(move || {
            let result =
                create_remote_workspace(&destination, label.unwrap_or_default()).map(|workspace| {
                    CreatedLocationWorkspace {
                        endpoint,
                        workspace,
                        profile: destination.profile,
                        stamp,
                        origin,
                        deadline: Instant::now() + Duration::from_secs(20),
                    }
                });
            let _ = send.send(result);
        });
        self.set_endpoint_error(format!("Creating a workspace on {name}…"));
    }

    /// The request of New workspace on another machine, then focus once that machine's
    /// snapshot lists the workspace. Focus is not taken once the user moved on: another
    /// machine shown, a dialog opened, or the machine's server restarted.
    fn tick_created_workspace(&mut self, outcome: &mut ClientShellInput) {
        let received =
            self.locations
                .create
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("Workspace creation stopped unexpectedly".into()))
                    }
                    Err(mpsc::TryRecvError::Empty) => None,
                });
        if let Some(result) = received {
            self.locations.create = None;
            match result {
                Ok(created) => self.locations.created = Some(created),
                Err(error) => self.set_endpoint_error(error),
            }
            outcome.repaint = true;
        }
        let Some(created) = self.locations.created.as_ref() else {
            return;
        };
        let stamp = self.location_stamp(&created.endpoint);
        let stale = stamp.as_ref().is_some_and(|stamp| stamp != &created.stamp);
        let visible = stamp.as_ref() == Some(&created.stamp)
            && self
                .endpoints
                .iter()
                .find(|e| e.endpoint_id == created.endpoint)
                .and_then(|e| e.snapshot.as_deref())
                .is_some_and(|s| {
                    s.workspaces
                        .iter()
                        .any(|w| w.workspace_id == created.workspace)
                });
        let moved_on = self.overlay.is_some() || self.active_endpoint_id != created.origin;
        if moved_on || stale || Instant::now() > created.deadline {
            let name = self.endpoint_label(&created.endpoint).to_owned();
            self.locations.created = None;
            self.set_endpoint_error(format!(
                "Workspace created on {name}; select it in the sidebar."
            ));
            outcome.repaint = true;
        } else if visible {
            let unchanged = created.profile.as_ref().is_none_or(|profile| {
                backend::effective_profiles().is_ok_and(|profiles| {
                    profiles
                        .iter()
                        .any(|p| backend::same_destination(p, profile) && p.enabled)
                })
            });
            let endpoint = created.endpoint.clone();
            let workspace = created.workspace.clone();
            self.locations.created = None;
            if unchanged {
                self.focus_or_activate(
                    endpoint,
                    ClientEndpointFocusTarget::Workspace(workspace),
                    outcome,
                );
            } else {
                self.set_endpoint_error(
                    "Workspace created, but the remote profile changed. Select it in the sidebar.",
                );
            }
            outcome.repaint = true;
        }
    }

    pub(crate) fn tick_locations(&mut self, outcome: &mut ClientShellInput) {
        self.tick_created_workspace(outcome);
        self.tick_add_remote(outcome);
        self.tick_account(outcome);
        self.tick_remotes_view(outcome);
        let synced =
            self.locations
                .sync
                .as_ref()
                .and_then(|(epoch, receiver)| match receiver.try_recv() {
                    Ok(snapshot) => Some((*epoch, Some(snapshot))),
                    Err(mpsc::TryRecvError::Disconnected) => Some((*epoch, None)),
                    Err(mpsc::TryRecvError::Empty) => None,
                });
        if let Some((epoch, snapshot)) = synced {
            self.locations.sync = None;
            if let (Some(snapshot), Some(ClientShellOverlay::Locations(dialog))) =
                (snapshot, self.overlay.as_mut())
            {
                if epoch == self.locations.epoch {
                    match snapshot {
                        Ok(snapshot) => {
                            if matches!(dialog.kind, LocationDialogKind::Manage) && !dialog.busy {
                                if let Some(notice) = &snapshot.notice {
                                    if !dialog.message.contains(notice.as_str()) {
                                        dialog.message = notice.clone();
                                    }
                                }
                            }
                            dialog.apply_snapshot(snapshot);
                        }
                        Err(error) => tracing::debug!(%error, "could not reload remotes"),
                    }
                    outcome.repaint = true;
                }
            }
        }
        // While the remotes list is open, hangar is asked again every few seconds.
        let manage_open = matches!(
            self.overlay,
            Some(ClientShellOverlay::Locations(LocationDialog {
                kind: LocationDialogKind::Manage,
                ..
            }))
        );
        if manage_open
            && self
                .locations
                .last_sync
                .is_none_or(|at| at.elapsed() >= backend::sync::SETTINGS_INTERVAL)
        {
            self.sync_machines();
        }
        let checked = self
            .locations
            .image_check
            .as_ref()
            .and_then(|(epoch, usage, receiver)| match receiver.try_recv() {
                Ok(result) => Some((*epoch, *usage, result)),
                Err(mpsc::TryRecvError::Disconnected) => Some((
                    *epoch,
                    *usage,
                    Err("The machine check stopped unexpectedly.".into()),
                )),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some((epoch, usage, result)) = checked {
            self.locations.image_check = None;
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                if let LocationDialogKind::Copy(request) = &mut dialog.kind {
                    // Only for the form it was made for.
                    if epoch == self.locations.epoch
                        && request.chosen
                        && request.choice.usage() == usage
                    {
                        request.check =
                            Some(result.unwrap_or_else(|note| SaveCheck { plan: None, note }));
                        if !dialog.busy {
                            dialog.message = request.message();
                        }
                        outcome.repaint = true;
                    }
                }
            }
        }
        let received =
            self.locations
                .job
                .as_ref()
                .and_then(|(epoch, receiver)| match receiver.try_recv() {
                    Ok(result) => Some((*epoch, result)),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some((*epoch, Err("Remote worker exited unexpectedly".into())))
                    }
                    Err(mpsc::TryRecvError::Empty) => None,
                });
        let mut refresh_states = false;
        let mut recheck_image = false;
        let mut refresh_account = false;
        let mut refresh_images = false;
        if let Some((epoch, result)) = received {
            self.locations.job = None;
            let succeeded = result.is_ok();
            let current = epoch == self.locations.epoch
                && matches!(self.overlay, Some(ClientShellOverlay::Locations(_)));
            let message = match result {
                Ok(JobResult::Message(message)) => message,
                Err(error) => error,
            };
            if current {
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.busy = false;
                    dialog.message = message.clone();
                    let saved = succeeded && matches!(dialog.kind, LocationDialogKind::Copy(_));
                    if !succeeded {
                        // The attempt may have stopped the machine: check it again and
                        // keep the error visible meanwhile.
                        if let LocationDialogKind::Copy(request) = &mut dialog.kind {
                            request.error = Some(message.clone());
                            request.check = None;
                            dialog.message = request.message();
                            recheck_image = true;
                        }
                    }
                    if matches!(dialog.kind, LocationDialogKind::SignOut) {
                        dialog.account = None;
                        dialog.view.usage = None;
                        dialog.view.images = None;
                        refresh_account = true;
                    }
                    if succeeded && matches!(dialog.kind, LocationDialogKind::DeleteImage(_)) {
                        refresh_images = true;
                    }
                    if saved
                        || matches!(
                            dialog.kind,
                            LocationDialogKind::Stop
                                | LocationDialogKind::Suspend
                                | LocationDialogKind::Delete(_)
                                | LocationDialogKind::DeleteImage(_)
                                | LocationDialogKind::SignOut
                        )
                    {
                        // A finished confirmation or copy never stays armed for a second
                        // Enter: the view is shown again as it was.
                        dialog.kind = LocationDialogKind::Manage;
                        dialog.fields.clear();
                        dialog.selected = 0;
                    }
                    if let Ok(snapshot) = RemotesSnapshot::load() {
                        dialog.apply_snapshot(snapshot);
                    }
                    refresh_states = true;
                }
            } else {
                self.set_endpoint_error(message);
            }
            outcome.repaint = true;
        }
        if refresh_states {
            // After Herdr's own operations: fetch the server's view at once.
            self.locations.sync = None;
            self.sync_machines();
        }
        if recheck_image {
            self.check_save_image();
        }
        if refresh_account {
            self.refresh_account();
        }
        if refresh_images {
            self.locations.images = None;
            self.request_images();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn dialog() -> LocationDialog {
        let profile = SavedSshEndpoint::new("Remote A", "demo-a", "work").unwrap();
        let mut prefs = LocationPreferences::default();
        prefs.remotes.insert(
            profile.id.clone(),
            RemoteOptions {
                cwd: "/remote/project".into(),
                cloud: None,
            },
        );
        LocationDialog {
            kind: LocationDialogKind::Manage,
            fields: Vec::new(),
            selected: 0,
            location: 0,
            profiles: vec![profile],
            prefs,
            message: String::new(),
            busy: false,
            machine_states: BTreeMap::new(),
            hidden: BTreeSet::new(),
            sync_notes: BTreeMap::new(),
            account: None,
            view: Box::default(),
        }
    }

    pub(super) fn shell() -> ClientShellState {
        ClientShellState::new(ClientShellConfig::from_config(&Config::default()))
    }

    pub(super) const MACHINE: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";

    /// A Manage dialog with the running hangar machine `box` (selected), listed as the
    /// remotes view builds it, and the SSH remote `Remote A`.
    pub(super) fn hangar_dialog() -> LocationDialog {
        hangar_dialog_with(MachineState::Running, false)
    }

    pub(super) fn hangar_snapshot(state: MachineState, hidden: bool) -> RemotesSnapshot {
        use crate::client::locations::sync::{CachedMachine, MachineCache};
        let plain = dialog().profiles.remove(0);
        let mut cache = MachineCache::default();
        let entry = cache
            .servers
            .entry("https://hangar.test".into())
            .or_default();
        entry.status = crate::client::locations::sync::SyncStatus::Ok;
        entry.machines.push(CachedMachine {
            id: MACHINE.into(),
            name: "box".into(),
            state,
            fence_until_ms: 0,
        });
        let mut prefs = LocationPreferences::default();
        prefs.remotes.insert(
            plain.id.clone(),
            RemoteOptions {
                cwd: "/remote/project".into(),
                cloud: None,
            },
        );
        prefs
            .edit_machine("https://hangar.test", MACHINE, |entry| {
                entry.hidden = hidden
            })
            .unwrap();
        let remotes = backend::Remotes::build(vec![plain], prefs, &cache, 0);
        RemotesSnapshot::from_remotes(&remotes)
    }

    pub(super) fn hangar_dialog_with(state: MachineState, hidden: bool) -> LocationDialog {
        let mut dialog = dialog();
        dialog.kind = LocationDialogKind::Manage;
        dialog.apply_snapshot(hangar_snapshot(state, hidden));
        // Remote A, then box.
        dialog.location = 2;
        dialog.selected = 0;
        dialog
    }

    #[test]
    fn hangar_machines_offer_delete_and_hide_while_ssh_remotes_offer_remove() {
        let label = |dialog: &LocationDialog, action| {
            dialog
                .actions()
                .into_iter()
                .find(|entry| entry.action == action)
                .map(|entry| entry.label)
        };
        let mut dialog = hangar_dialog();
        assert_eq!(dialog.profile().unwrap().label, "box");
        assert_eq!(
            label(&dialog, RemoteAction::Remove),
            Some("Delete machine…")
        );
        assert_eq!(
            label(&dialog, RemoteAction::Hide),
            Some("Hide from sidebar")
        );
        dialog.location = 1;
        assert_eq!(dialog.profile().unwrap().label, "Remote A");
        assert_eq!(label(&dialog, RemoteAction::Remove), Some("Remove remote"));
        assert_eq!(label(&dialog, RemoteAction::Hide), None);
        let hidden = hangar_dialog_with(MachineState::Running, true);
        assert_eq!(label(&hidden, RemoteAction::Hide), Some("Show in sidebar"));
        assert!(
            hidden.location_label().ends_with("· hidden"),
            "{}",
            hidden.location_label()
        );
        assert!(
            !hidden.profile().unwrap().enabled,
            "hidden machines do not connect"
        );
    }

    #[test]
    fn hide_on_an_ssh_remote_explains_itself_and_removes_nothing() {
        let mut state = shell();
        let mut dialog = hangar_dialog();
        dialog.location = 1;
        let profile = dialog.profiles[0].clone();
        let options = dialog.prefs.remotes[&profile.id].clone();
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        let error = state
            .remote_location_action(RemoteAction::Hide, profile, options)
            .unwrap_err();
        // Validation of the remote runs against disk first; either refusal is fine,
        // but nothing is hidden or removed.
        assert!(
            error.contains("Hide from sidebar applies to hangar machines")
                || error.contains("removed or changed"),
            "{error}"
        );
    }

    #[test]
    fn delete_machine_confirmation_names_the_machine_and_what_is_lost() {
        let mut state = shell();
        let dialog = hangar_dialog();
        let profile = dialog.profile().unwrap().clone();
        let options = dialog.prefs.remotes[&profile.id].clone();
        let binding = options.cloud.as_ref().unwrap().hangar().clone();
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.confirm_delete(DeleteRequest {
            binding,
            profile,
            options,
        });
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert!(matches!(dialog.kind, LocationDialogKind::Delete(_)));
        assert!(dialog.message.contains("'box'"), "{}", dialog.message);
        assert!(dialog
            .message
            .contains("disks and snapshots are permanently deleted"));
        assert!(dialog.message.contains("closes its workspaces"));
        assert!(!dialog.message.contains("Remote A"));
        assert!(state.locations.job.is_none(), "nothing runs before Enter");
        state.compose(110, 35).unwrap();
    }

    #[test]
    fn a_finished_delete_returns_to_the_remote_list_instead_of_staying_armed() {
        let mut state = shell();
        let mut dialog = hangar_dialog();
        let profile = dialog.profile().unwrap().clone();
        let options = dialog.prefs.remotes[&profile.id].clone();
        let binding = options.cloud.as_ref().unwrap().hangar().clone();
        dialog.kind = LocationDialogKind::Delete(Box::new(DeleteRequest {
            binding,
            profile,
            options,
        }));
        dialog.busy = true;
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        send.send(Err("Could not delete 'box': hangar error".into()))
            .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert!(!dialog.busy);
        assert!(dialog.message.contains("Could not delete"));
    }

    /// Copy machine… on `box`, past the chooser on `choice`. With `check`, the machine
    /// check has answered; otherwise it is still running.
    fn copy_shell(choice: CopyChoice, check: Option<SaveCheck>) -> ClientShellState {
        let mut state = shell();
        state.locations.save_checker = Some(|_| {
            Err(crate::hangar::api::HangarError::Invalid(
                "offline in tests".into(),
            ))
        });
        let dialog = hangar_dialog();
        let profile = dialog.profile().unwrap().clone();
        let options = dialog.prefs.remotes[&profile.id].clone();
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.show_copy(CopyRequest::new(profile, options, "box".into()));
        state.switch_copy_choice(choice);
        state.choose_copy();
        if let Some(check) = check {
            state.locations.image_check = None;
            let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() else {
                panic!("dialog");
            };
            let LocationDialogKind::Copy(request) = &mut dialog.kind else {
                panic!("copy machine");
            };
            request.check = Some(check);
            dialog.message = request.message();
        }
        state
    }

    fn copy_request(state: &ClientShellState) -> (&LocationDialog, &CopyRequest) {
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        let LocationDialogKind::Copy(request) = &dialog.kind else {
            panic!("copy machine");
        };
        (dialog, request)
    }

    fn deliver_check(state: &mut ClientShellState, epoch: u64, check: SaveCheck) {
        let usage = copy_request(state).1.choice.usage();
        let (send, receive) = mpsc::channel();
        state.locations.image_check = Some((epoch, usage, receive));
        send.send(Ok(check)).unwrap();
        state.tick_locations(&mut ClientShellInput::default());
    }

    fn type_name(state: &mut ClientShellState, name: &str) {
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() else {
            panic!("dialog");
        };
        dialog.fields[0] = TextEditor::new(name, false);
    }

    #[test]
    fn copy_machine_is_one_action_and_save_as_image_states_what_is_saved() {
        let copies = hangar_dialog()
            .actions()
            .into_iter()
            .filter(|entry| entry.group == view::ActionGroup::Copy)
            .map(|entry| entry.label)
            .collect::<Vec<_>>();
        assert_eq!(copies, ["Copy machine…"]);
        let mut state = copy_shell(CopyChoice::Image, None);
        assert!(
            matches!(
                state.locations.image_check,
                Some((_, SnapshotUse::Image, _))
            ),
            "checks the machine for an image"
        );
        let (dialog, request) = copy_request(&state);
        assert_eq!(dialog.title(), "save box as image");
        assert_eq!(dialog.labels(), ["Image name", "Description"]);
        assert!(dialog.message.contains("Checking box…"));
        for part in [
            "root disk only: installed software and system settings",
            "Repositories, home directory files and logins on /data are not included",
            "`sudo gh auth`",
            "/etc/environment",
            "stopped first",
            "images tab",
            "New machine from image…",
            "private",
        ] {
            assert!(dialog.message.contains(part), "{part}: {}", dialog.message);
        }
        assert_eq!(request.plan(), None);
        // Enter does nothing until the check arrives.
        type_name(&mut state, "base");
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none());
        let typed = crate::input::TerminalKey::from(crossterm::event::KeyEvent::new(
            KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        ));
        state.route_location_key(&typed, &mut ClientShellInput::default());
        assert_eq!(copy_request(&state).0.fields[0].as_str(), "basex");
        state.compose(110, 35).unwrap();
    }

    #[test]
    fn a_running_machine_offers_stop_machine_and_save_and_old_templates_refuse() {
        let mut state = copy_shell(CopyChoice::Image, None);
        let epoch = state.locations.epoch;
        let running = SaveCheck {
            plan: Some(SavePlan::Stop),
            note: "box is running. Only a stopped machine can be saved.".into(),
        };
        deliver_check(&mut state, epoch, running);
        let (dialog, request) = copy_request(&state);
        assert_eq!(request.primary_label(), " ↵ stop machine and save ");
        assert!(dialog.message.starts_with("box is running."));
        assert!(dialog.message.contains("root disk only"));
        state.compose(110, 35).unwrap();
        // A check for an older dialog is ignored.
        deliver_check(
            &mut state,
            epoch.wrapping_sub(1),
            SaveCheck {
                plan: None,
                note: "stale".into(),
            },
        );
        assert_eq!(copy_request(&state).1.plan(), Some(SavePlan::Stop));
        // So is a check made for the other choice.
        let (send, receive) = mpsc::channel();
        state.locations.image_check = Some((epoch, SnapshotUse::Fork, receive));
        send.send(Ok(SaveCheck {
            plan: None,
            note: "for a clone".into(),
        }))
        .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        assert_eq!(copy_request(&state).1.plan(), Some(SavePlan::Stop));

        let mut state = copy_shell(
            CopyChoice::Image,
            Some(SaveCheck {
                plan: None,
                note:
                    "box was created from template herdr@old, which is too old to save images from."
                        .into(),
            }),
        );
        type_name(&mut state, "base");
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none(), "an old template never saves");
        assert_eq!(copy_request(&state).1.primary_label(), " ↵ save ");
    }

    #[test]
    fn an_invalid_image_name_stops_nothing() {
        let mut state = copy_shell(
            CopyChoice::Image,
            Some(SaveCheck {
                plan: Some(SavePlan::Stop),
                note: String::new(),
            }),
        );
        type_name(&mut state, "Not Valid");
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none());
        let (dialog, _) = copy_request(&state);
        assert!(dialog.message.starts_with("Image names use"));
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn a_failed_save_stays_open_and_rechecks_while_a_saved_one_returns_to_the_list() {
        let mut state = copy_shell(
            CopyChoice::Image,
            Some(SaveCheck {
                plan: Some(SavePlan::Save),
                note: String::new(),
            }),
        );
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        send.send(Err(
            "Could not save image base: an image named \"base\" already exists".into(),
        ))
        .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let (dialog, request) = copy_request(&state);
        assert!(dialog.message.contains("already exists"));
        assert!(dialog.message.contains("Checking box…"));
        assert!(request.check.is_none());
        assert!(request.chosen, "the form stays open");
        assert!(state.locations.image_check.is_some(), "checks again");
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        send.send(Ok(JobResult::Message("Saved image base from box.".into())))
            .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert!(dialog.message.contains("Saved image base"));
    }

    #[test]
    fn clone_now_suggests_a_name_and_states_what_is_copied() {
        let mut state = copy_shell(CopyChoice::Clone, None);
        assert!(
            matches!(state.locations.image_check, Some((_, SnapshotUse::Fork, _))),
            "checks the machine for a clone"
        );
        let (dialog, request) = copy_request(&state);
        assert_eq!(dialog.title(), "clone box");
        assert_eq!(dialog.labels(), ["Name"]);
        assert_eq!(dialog.fields[0].as_str(), "box-clone");
        assert!(dialog.message.contains("Checking box…"));
        for part in [
            "root disk and its /data disk",
            "installed software, system settings, repositories",
            "home directory",
            "signed-in credentials (gh, Claude, Codex, SSH keys)",
            "its own SSH host keys",
            "starts as a new machine",
            "remotes tab",
            "stopped first and stays stopped",
        ] {
            assert!(dialog.message.contains(part), "{part}: {}", dialog.message);
        }
        assert!(!dialog.message.to_lowercase().contains("fork"));
        assert_eq!(request.plan(), None);
        // Enter does nothing until the check arrives.
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none());
        state.compose(110, 35).unwrap();
        let epoch = state.locations.epoch;
        deliver_check(
            &mut state,
            epoch,
            SaveCheck {
                plan: Some(SavePlan::ResumeThenStop),
                note: "box is suspended.".into(),
            },
        );
        let (dialog, request) = copy_request(&state);
        assert_eq!(request.primary_label(), " ↵ stop machine and clone ");
        assert!(dialog.message.starts_with("box is suspended."));
        state.compose(110, 35).unwrap();
        deliver_check(
            &mut state,
            epoch,
            SaveCheck {
                plan: Some(SavePlan::Save),
                note: String::new(),
            },
        );
        assert_eq!(copy_request(&state).1.primary_label(), " ↵ clone ");
    }

    #[test]
    fn an_invalid_clone_name_or_uncopyable_machine_stops_nothing() {
        let mut state = copy_shell(CopyChoice::Clone, None);
        let epoch = state.locations.epoch;
        deliver_check(
            &mut state,
            epoch,
            SaveCheck {
                plan: Some(SavePlan::Stop),
                note: String::new(),
            },
        );
        type_name(&mut state, "Box Copy");
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none());
        assert!(copy_request(&state)
            .0
            .message
            .starts_with("Machine names use"));
        deliver_check(
            &mut state,
            epoch,
            SaveCheck {
                plan: None,
                note: "box was created from template herdr@old, which is too old to clone.".into(),
            },
        );
        type_name(&mut state, "box-copy");
        state.accept_location(&mut ClientShellInput::default());
        assert!(
            state.locations.job.is_none(),
            "an old template never clones"
        );
    }

    #[test]
    fn a_failed_clone_stays_open_and_rechecks_while_a_clone_returns_to_the_list() {
        let mut state = copy_shell(CopyChoice::Clone, None);
        let epoch = state.locations.epoch;
        deliver_check(
            &mut state,
            epoch,
            SaveCheck {
                plan: Some(SavePlan::Save),
                note: String::new(),
            },
        );
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        send.send(Err(
            "Could not clone into box-clone: a machine named \"box-clone\" already exists".into(),
        ))
        .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let (dialog, request) = copy_request(&state);
        assert!(dialog.message.contains("already exists"));
        assert!(dialog.message.contains("Checking box…"));
        assert!(request.check.is_none());
        assert!(state.locations.image_check.is_some(), "checks again");
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        send.send(Ok(JobResult::Message(
            "Cloned box into box-clone. box-clone is ready.".into(),
        )))
        .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert!(matches!(dialog.kind, LocationDialogKind::Manage));
        assert!(dialog.message.contains("Cloned box into box-clone"));
    }

    #[test]
    fn a_suspended_machine_offers_resume_and_is_labelled_suspended() {
        let mut state = shell();
        let start = |dialog: &LocationDialog| dialog.actions()[0].clone();
        let dialog = hangar_dialog_with(MachineState::Stopped, false);
        assert_eq!(start(&dialog).label, "Start");
        assert!(
            dialog.location_label().ends_with("(stopped)"),
            "{}",
            dialog.location_label()
        );
        assert!(
            !dialog.profile().unwrap().enabled,
            "stopped machines never connect"
        );
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        let (send, receive) = mpsc::channel();
        state.locations.sync = Some((state.locations.epoch, receive));
        state.locations.last_sync = Some(Instant::now());
        send.send(Ok(hangar_snapshot(MachineState::Suspended, false)))
            .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert_eq!(dialog.profile().unwrap().label, "box", "selection is kept");
        assert_eq!(start(dialog).label, "Resume");
        assert_eq!(start(dialog).reason, None);
        assert!(dialog.location_label().ends_with("(suspended)"));
        assert_eq!(dialog.state_text(1), "suspended");
        // A sync result for an older dialog is ignored.
        let (send, receive) = mpsc::channel();
        state.locations.sync = Some((state.locations.epoch.wrapping_sub(1), receive));
        send.send(Ok(hangar_snapshot(MachineState::Running, false)))
            .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert_eq!(dialog.machine_state(), Some(MachineState::Suspended));
    }

    #[test]
    fn the_remotes_list_syncs_again_while_open_and_marks_an_offline_list() {
        let mut state = shell();
        state.overlay = Some(ClientShellOverlay::Locations(hangar_dialog()));
        state.locations.last_sync = Some(Instant::now());
        state.tick_locations(&mut ClientShellInput::default());
        assert!(state.locations.sync.is_none(), "not before the interval");
        state.locations.last_sync =
            Some(Instant::now() - backend::sync::SETTINGS_INTERVAL - Duration::from_secs(1));
        state.tick_locations(&mut ClientShellInput::default());
        assert!(state.locations.sync.is_some(), "every interval while open");
        // An offline list keeps its machines and says so.
        let mut snapshot = hangar_snapshot(MachineState::Running, false);
        let id = snapshot.profiles[1].id.clone();
        snapshot.sync_notes.insert(id, "offline");
        snapshot.notice = Some(
            "hangar machines on https://hangar.test are offline; showing the last synced list."
                .into(),
        );
        let (send, receive) = mpsc::channel();
        state.locations.sync = Some((state.locations.epoch, receive));
        send.send(Ok(snapshot)).unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert!(dialog.location_label().ends_with("· offline"));
        assert!(dialog.message.contains("showing the last synced list"));
        state.compose(110, 35).unwrap();
    }

    /// The remotes with `default` (an index into the profiles: Remote A, then box) as the
    /// default machine; box is in `state`.
    fn remotes_with_default(
        default: Option<usize>,
        state: MachineState,
        hidden: bool,
    ) -> RemotesSnapshot {
        let mut snapshot = hangar_snapshot(state, hidden);
        // A fixed ID for Remote A, so every load names the same remote (box's ID is
        // derived from its machine).
        let fixed = ProfileId::parse("a".repeat(32)).unwrap();
        let old = std::mem::replace(&mut snapshot.profiles[0].id, fixed.clone());
        if let Some(options) = snapshot.prefs.remotes.remove(&old) {
            snapshot.prefs.remotes.insert(fixed, options);
        }
        snapshot.prefs.default_profile = default.map(|index| snapshot.profiles[index].id.clone());
        snapshot
    }

    /// A shell showing Local (`ws_1` focused), with Remote A and box known; `online`
    /// remotes are connected with a snapshot.
    fn routing_shell(prompt: bool, online: &[usize]) -> ClientShellState {
        let mut config = Config::default();
        config.ui.prompt_new_workspace_name = prompt;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        let profiles = remotes_with_default(None, MachineState::Running, false).profiles;
        state.set_endpoint_catalog(&profiles);
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        for index in online {
            let endpoint = ClientEndpointId::Ssh(profiles[*index].id.clone());
            state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
            state.set_endpoint_snapshot(&endpoint, Box::new(super::super::tests::snapshot()));
        }
        state
    }

    fn new_workspace(state: &mut ClientShellState) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        state.record_binding(
            crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewWorkspace),
            &mut outcome,
        );
        outcome
    }

    /// The endpoint and parameters of the one WorkspaceCreate request in `outcome`.
    fn create_request(
        outcome: &ClientShellInput,
    ) -> (ClientEndpointId, crate::api::schema::WorkspaceCreateParams) {
        let [ClientShellAction::Endpoint {
            endpoint_id,
            request,
            ..
        }] = &outcome.actions[..]
        else {
            panic!("one endpoint request: {:?}", outcome.actions);
        };
        let crate::api::schema::Method::WorkspaceCreate(params) = &request.method else {
            panic!("workspace create: {:?}", request.method);
        };
        (endpoint_id.clone(), params.clone())
    }

    #[test]
    fn new_workspace_on_a_local_default_creates_like_upstream_without_a_dialog() {
        for default in [None, Some(5)] {
            let mut state = routing_shell(false, &[0]);
            // No default, or a default that no longer exists: Local.
            state.locations.remotes_loader = Some(if default.is_none() {
                || Ok(remotes_with_default(None, MachineState::Running, false))
            } else {
                || {
                    let mut snapshot = hangar_snapshot(MachineState::Running, false);
                    snapshot.prefs.default_profile =
                        Some(SavedSshEndpoint::new("Gone", "gone", "work").unwrap().id);
                    Ok(snapshot)
                }
            });
            let outcome = new_workspace(&mut state);
            assert!(state.overlay.is_none(), "no location dialog");
            let (endpoint, params) = create_request(&outcome);
            assert_eq!(endpoint, ClientEndpointId::Local);
            assert_eq!(params.source_workspace_id.as_deref(), Some("ws_1"));
            assert_eq!(params.cwd, None);
            assert_eq!(params.label, None);
            assert!(params.focus);
            assert!(state.locations.create.is_none());
        }
    }

    #[test]
    fn prompt_new_workspace_name_asks_for_a_name_then_creates_on_the_default_machine() {
        // Local default: the upstream name prompt, then the usual request.
        let mut state = routing_shell(true, &[]);
        state.locations.remotes_loader =
            Some(|| Ok(remotes_with_default(None, MachineState::Running, false)));
        let outcome = new_workspace(&mut state);
        assert!(outcome.actions.is_empty());
        assert!(matches!(
            state.overlay.as_ref(),
            Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                title: "new workspace",
                input,
                target: ClientRenameTarget::NewWorkspace {
                    source_workspace_id,
                    destination: None,
                    ..
                },
            })) if input.as_str() == "repo" && source_workspace_id.as_deref() == Some("ws_1")
        ));
        let mut outcome = ClientShellInput::default();
        if let Some(ClientShellOverlay::Rename(rename)) = state.overlay.as_mut() {
            rename.input = TextEditor::new("named", true);
        }
        state.save_rename_overlay(&mut outcome);
        let (endpoint, params) = create_request(&outcome);
        assert_eq!(endpoint, ClientEndpointId::Local);
        assert_eq!(params.label.as_deref(), Some("named"));

        // A connected remote default that is not displayed: the prompt suggests its
        // directory's name, and the request goes to that machine.
        let mut state = routing_shell(true, &[0]);
        state.locations.remotes_loader =
            Some(|| Ok(remotes_with_default(Some(0), MachineState::Running, false)));
        new_workspace(&mut state);
        let Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            input,
            target:
                ClientRenameTarget::NewWorkspace {
                    source_workspace_id,
                    cwd,
                    destination: Some(destination),
                    ..
                },
            ..
        })) = state.overlay.as_ref()
        else {
            panic!(
                "name prompt for another machine: {:?}",
                state.overlay.is_some()
            );
        };
        assert_eq!(input.as_str(), "project");
        assert_eq!(
            source_workspace_id, &None,
            "never an ID from another machine"
        );
        assert_eq!(cwd.as_deref(), Some("/remote/project"));
        assert_eq!(destination.profile.as_ref().unwrap().label, "Remote A");
        let mut outcome = ClientShellInput::default();
        state.save_rename_overlay(&mut outcome);
        assert!(
            outcome.actions.is_empty(),
            "not sent to the displayed machine"
        );
        assert!(state.locations.create.is_some(), "asked on Remote A");
    }

    #[test]
    fn new_workspace_on_a_connected_remote_default_uses_its_directory() {
        // Displayed: the usual request with the machine's default directory.
        let mut state = routing_shell(false, &[0]);
        let remote = ClientEndpointId::Ssh(
            remotes_with_default(None, MachineState::Running, false).profiles[0]
                .id
                .clone(),
        );
        assert!(state.activate_endpoint_projection(&remote));
        state.locations.remotes_loader =
            Some(|| Ok(remotes_with_default(Some(0), MachineState::Running, false)));
        let outcome = new_workspace(&mut state);
        assert!(state.overlay.is_none());
        let (endpoint, params) = create_request(&outcome);
        assert_eq!(endpoint, remote);
        assert_eq!(params.cwd.as_deref(), Some("/remote/project"));
        assert_eq!(params.source_workspace_id.as_deref(), Some("ws_1"));

        // Not displayed: its server is asked directly; nothing goes to Local.
        let mut state = routing_shell(false, &[0]);
        state.locations.remotes_loader =
            Some(|| Ok(remotes_with_default(Some(0), MachineState::Running, false)));
        let outcome = new_workspace(&mut state);
        assert!(outcome.actions.is_empty());
        assert!(state.overlay.is_none());
        assert!(state.locations.create.is_some());
        assert_eq!(
            state.endpoint_notice(),
            Some("Creating a workspace on Remote A…")
        );
        // One at a time.
        new_workspace(&mut state);
        assert_eq!(
            state.endpoint_notice(),
            Some("A new workspace is still being created")
        );
        // A failed request is shown (unit tests never reach another machine).
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.locations.create.is_some() && Instant::now() < deadline {
            state.tick_locations(&mut ClientShellInput::default());
            std::thread::yield_now();
        }
        assert!(state.locations.create.is_none());
        assert!(state
            .endpoint_notice()
            .is_some_and(|notice| notice.contains("disabled in unit tests")));

        // The destination: the default machine with its preference.
        let state = routing_shell(false, &[0]);
        let destination = state
            .new_workspace_destination(remotes_with_default(Some(0), MachineState::Running, false))
            .unwrap();
        assert_eq!(destination.endpoint(), remote);
        assert_eq!(destination.cwd().as_deref(), Some("/remote/project"));
        let destination = state
            .new_workspace_destination(remotes_with_default(None, MachineState::Running, false))
            .unwrap();
        assert_eq!(destination.endpoint(), ClientEndpointId::Local);
        assert_eq!(destination.cwd(), None, "empty: the server's default");
    }

    #[test]
    fn a_default_machine_that_is_not_connected_is_never_started_and_nothing_is_created() {
        let cases: [(RemotesLoader, &[usize], &str); 4] = [
            // A stopped hangar machine.
            (
                || Ok(remotes_with_default(Some(1), MachineState::Stopped, false)),
                &[],
                "Default machine box isn't connected — start it in Settings → remotes, or choose another default.",
            ),
            // A running one that is still connecting.
            (
                || Ok(remotes_with_default(Some(1), MachineState::Running, false)),
                &[],
                "Default machine box isn't connected — start it in Settings → remotes, or choose another default.",
            ),
            // A hidden one.
            (
                || Ok(remotes_with_default(Some(1), MachineState::Running, true)),
                &[1],
                "Default machine box is hidden from the sidebar — show it in Settings → remotes, or choose another default.",
            ),
            // An SSH remote that is offline.
            (
                || Ok(remotes_with_default(Some(0), MachineState::Running, false)),
                &[],
                "Default machine Remote A isn't connected — start it in Settings → remotes, or choose another default.",
            ),
        ];
        for prompt in [false, true] {
            for (loader, online, notice) in cases {
                let mut state = routing_shell(prompt, online);
                state.locations.remotes_loader = Some(loader);
                let outcome = new_workspace(&mut state);
                assert!(outcome.actions.is_empty(), "{notice}");
                assert!(state.overlay.is_none(), "no prompt either: {notice}");
                assert!(state.locations.create.is_none());
                assert!(state.locations.job.is_none(), "nothing is started");
                assert_eq!(state.endpoint_notice(), Some(notice));
            }
        }
    }

    #[test]
    fn unreadable_remote_settings_still_create_a_local_workspace() {
        let mut state = routing_shell(false, &[]);
        state.locations.remotes_loader = Some(|| Err("Invalid remote locations".into()));
        let (endpoint, _) = create_request(&new_workspace(&mut state));
        assert_eq!(endpoint, ClientEndpointId::Local);
    }

    #[test]
    fn every_new_workspace_entry_point_uses_the_default_machine() {
        // The sidebar's new workspace (several machines) and the mobile menu's entry
        // reach the same routing as the keybinding: here, a stopped default.
        let mut state = routing_shell(false, &[0]);
        state.locations.remotes_loader =
            Some(|| Ok(remotes_with_default(Some(1), MachineState::Stopped, false)));
        state.compose(120, 40).unwrap();
        let button = state.hits.new_workspace;
        assert!(!button.is_empty());
        let mut outcome = ClientShellInput::default();
        state.handle_mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: button.x + 1,
                row: button.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &mut outcome,
        );
        assert!(outcome.actions.is_empty());
        assert!(state.overlay.is_none());
        assert!(state
            .endpoint_notice()
            .is_some_and(|notice| notice.starts_with("Default machine box isn't connected")));
    }

    fn created(endpoint: ClientEndpointId, boot_id: &str) -> CreatedLocationWorkspace {
        CreatedLocationWorkspace {
            endpoint,
            workspace: "ws_1".into(),
            profile: None,
            stamp: LocationStamp {
                generation: None,
                boot_id: boot_id.into(),
            },
            origin: ClientEndpointId::Local,
            deadline: Instant::now() + Duration::from_secs(10),
        }
    }

    #[test]
    fn a_created_workspace_never_steals_focus_once_a_dialog_opened() {
        let mut state = shell();
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        let (send, receive) = mpsc::channel();
        state.locations.create = Some(receive);
        state.overlay = Some(ClientShellOverlay::Locations(dialog()));
        send.send(Ok(created(ClientEndpointId::Local, "boot-1")))
            .unwrap();
        let mut outcome = ClientShellInput::default();
        state.tick_locations(&mut outcome);
        assert!(state.locations.create.is_none());
        assert!(state.locations.created.is_none());
        assert!(outcome.actions.is_empty());
        assert_eq!(
            state.endpoint_notice(),
            Some("Workspace created on Local; select it in the sidebar.")
        );
    }

    #[test]
    fn create_focus_waits_for_destination_snapshot_despite_colliding_local_ids() {
        let mut state = shell();
        let dialog = dialog();
        let endpoint = ClientEndpointId::Ssh(dialog.profiles[0].id.clone());
        state.set_endpoint_catalog(&dialog.profiles);
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        state.locations.created = Some(created(endpoint.clone(), "boot-1"));
        let mut outcome = ClientShellInput::default();
        state.tick_locations(&mut outcome);
        assert!(outcome.actions.is_empty());
        state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
        state.set_endpoint_snapshot(&endpoint, Box::new(super::super::tests::snapshot()));
        state.tick_locations(&mut outcome);
        assert!(
            matches!(&outcome.actions[..], [ClientShellAction::ActivateEndpoint {endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(id))}] if endpoint_id == &endpoint && id == "ws_1")
        );
        outcome.actions.clear();
        state.tick_locations(&mut outcome);
        assert!(outcome.actions.is_empty());
    }

    #[test]
    fn popup_takeover_another_machine_or_server_reboot_cancels_delayed_create_focus() {
        for case in ["popup", "reboot", "moved"] {
            let mut state = shell();
            let dialog = dialog();
            let endpoint = ClientEndpointId::Ssh(dialog.profiles[0].id.clone());
            state.set_endpoint_catalog(&dialog.profiles);
            state.set_snapshot(Box::new(super::super::tests::snapshot()));
            state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
            state.set_endpoint_snapshot(&endpoint, Box::new(super::super::tests::snapshot()));
            let mut pending = created(
                endpoint.clone(),
                if case == "reboot" {
                    "prior-boot"
                } else {
                    "boot-1"
                },
            );
            match case {
                "popup" => state.overlay = Some(ClientShellOverlay::Locations(dialog)),
                // Another machine was displayed when New workspace was chosen.
                "moved" => pending.origin = endpoint.clone(),
                _ => {}
            }
            state.locations.created = Some(pending);
            let mut outcome = ClientShellInput::default();
            state.tick_locations(&mut outcome);
            assert!(outcome.actions.is_empty(), "{case}");
            assert!(state.locations.created.is_none(), "{case}");
        }
    }

    #[test]
    fn remote_management_survives_disconnection_and_consumes_paste_locally() {
        let mut state = shell();
        let mut dialog = dialog();
        dialog.kind = LocationDialogKind::Edit(None);
        dialog.fields = vec![TextEditor::default()];
        dialog.selected = 0;
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.reset_endpoint_projection();
        assert!(state.modal_paste_target_active());
        assert!(state.insert_overlay_text("hello\nworld"));
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert_eq!(dialog.fields[0].as_str(), "hello world");
    }

    #[test]
    fn refreshing_locations_preserves_identity_when_an_earlier_profile_is_removed() {
        let mut dialog = dialog();
        let second = SavedSshEndpoint::new("B", "host-b", "work").unwrap();
        dialog.profiles.push(second.clone());
        dialog.location = 2;
        dialog.replace_profiles(vec![second.clone()]);
        assert_eq!(dialog.profile().map(|p| &p.id), Some(&second.id));
        dialog.replace_profiles(Vec::new());
        assert!(dialog.profile().is_none());
        assert_eq!(dialog.location_label(), "Local");
    }

    #[test]
    fn settings_and_remote_entry_are_visible_without_any_pane_surface() {
        let mut state = shell();
        state.compose(120, 40).unwrap();
        let launcher = state.hits.global_launcher;
        assert!(!launcher.is_empty());
        state.handle_mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: launcher.x,
                row: launcher.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::Settings(_))
        ));
        state.compose(120, 40).unwrap();
        let (rect, _) = state
            .hits
            .settings_tabs
            .iter()
            .find(|(_, section)| *section == ClientSettingsSection::Remotes)
            .unwrap();
        state.handle_mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::Locations(_))
        ));
        state.compose(120, 40).unwrap();
        assert!(!state.hits.remotes.is_empty());

        // The Remotes tab keeps the settings tabs, and they lead back to other sections.
        let (rect, _) = *state
            .hits
            .settings_tabs
            .iter()
            .find(|(_, section)| *section == ClientSettingsSection::Theme)
            .unwrap();
        state.handle_mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                section: ClientSettingsSection::Theme,
                ..
            }))
        ));
    }
}
