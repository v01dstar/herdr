use super::*;
use crate::client::endpoint::{EndpointCatalog, ProfileId};
use crate::client::locations::hangar::{SaveCheck, SavePlan, IMAGE_CONTENTS};
use crate::client::locations::{self as backend, LocationPreferences, RemoteOptions};
use crate::hangar::api::MachineState;
use crate::hangar::binding::HangarBinding;
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(super) mod add;

const START_ROW: usize = 6;
const SAVE_IMAGE_ROW: usize = 9;

#[derive(Debug)]
pub(super) enum LocationDialogKind {
    Manage,
    Add(Box<add::AddRemoteForm>),
    Edit(Option<ProfileId>),
    New,
    Stop,
    Suspend,
    Delete(Box<DeleteRequest>),
    SaveImage(Box<SaveImageRequest>),
    DeleteImage(Box<ImageDeleteRequest>),
}

/// Save as image… for a hangar remote. The machine check runs on a worker.
#[derive(Clone, Debug)]
pub(super) struct SaveImageRequest {
    pub profile: SavedSshEndpoint,
    pub options: RemoteOptions,
    pub machine_name: String,
    /// `None` while the machine is being checked.
    pub check: Option<SaveCheck>,
    /// The last failed attempt, kept visible while the machine is checked again.
    pub error: Option<String>,
}

impl SaveImageRequest {
    pub fn plan(&self) -> Option<SavePlan> {
        self.check.as_ref().and_then(|check| check.plan)
    }

    /// What the dialog says: a failure, how the machine becomes saveable, and what an
    /// image contains.
    pub fn message(&self) -> String {
        let status = match &self.check {
            None => format!("Checking {}…", self.machine_name),
            Some(check) => check.note.clone(),
        };
        [self.error.as_deref().unwrap_or(""), &status, IMAGE_CONTENTS]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn primary_label(&self) -> &'static str {
        match self.plan() {
            Some(SavePlan::Stop | SavePlan::ResumeThenStop) => " ↵ stop machine and save ",
            _ => " ↵ save ",
        }
    }
}

/// Delete image… from Add remote's source picker.
#[derive(Clone, Debug)]
pub(super) struct ImageDeleteRequest {
    pub server: String,
    pub id: String,
    pub name: String,
}

/// A confirmed Remove remote deletes this hangar machine.
#[derive(Clone, Debug)]
pub(super) struct DeleteRequest {
    pub binding: HangarBinding,
    /// The remote whose machine is deleted; `None` for a machine no remote uses.
    pub remote: Option<(SavedSshEndpoint, RemoteOptions)>,
}

#[derive(Debug)]
pub(super) struct LocationDialog {
    pub kind: LocationDialogKind,
    pub fields: Vec<TextEditor>,
    pub selected: usize,
    pub location: usize,
    pub location_missing: bool,
    pub profiles: Vec<SavedSshEndpoint>,
    pub prefs: LocationPreferences,
    pub message: String,
    pub busy: bool,
    /// Last known hangar machine states by (server, machine ID); presentation only,
    /// refreshed by a worker. Missing means unknown.
    pub machine_states: BTreeMap<(String, String), MachineState>,
}

impl LocationDialog {
    pub fn title(&self) -> &str {
        match self.kind {
            LocationDialogKind::Manage => "remotes",
            LocationDialogKind::Add(_) => "add remote",
            LocationDialogKind::Edit(_) => "remote settings",
            LocationDialogKind::New => "new workspace",
            LocationDialogKind::Stop => "stop machine",
            LocationDialogKind::Suspend => "suspend machine",
            LocationDialogKind::Delete(_) => "delete machine",
            LocationDialogKind::SaveImage(_) => "save as image",
            LocationDialogKind::DeleteImage(_) => "delete image",
        }
    }
    pub fn labels(&self) -> &[&str] {
        match self.kind {
            LocationDialogKind::Add(_) => &["Provider", "Machine", "Name", "Source", ""],
            LocationDialogKind::Manage => &[
                "Location",
                "Add remote",
                "Edit remote",
                "Test connection",
                "Use as default",
                "Machine status",
                "Start remote",
                "Suspend remote…",
                "Stop machine…",
                "Save as image…",
                "Remove remote…",
                "Remove profile",
            ],
            LocationDialogKind::Edit(_) => {
                &["Name", "SSH target", "Herdr session", "Default directory"]
            }
            LocationDialogKind::New => &["Name", "Location", "Directory"],
            LocationDialogKind::SaveImage(_) => &["Image name", "Description"],
            LocationDialogKind::Stop
            | LocationDialogKind::Suspend
            | LocationDialogKind::Delete(_)
            | LocationDialogKind::DeleteImage(_) => &[],
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

    /// Row text; Start remote reads Resume remote for a suspended machine.
    pub fn row_label(&self, index: usize) -> &str {
        if matches!(self.kind, LocationDialogKind::Manage)
            && index == START_ROW
            && self.machine_state() == Some(MachineState::Suspended)
        {
            return "Resume remote";
        }
        self.labels().get(index).copied().unwrap_or("")
    }

    pub fn profile(&self) -> Option<&SavedSshEndpoint> {
        self.location
            .checked_sub(1)
            .and_then(|i| self.profiles.get(i))
    }

    fn replace_profiles(&mut self, profiles: Vec<SavedSshEndpoint>) {
        let selected = self.profile().map(|p| p.id.clone());
        self.location_missing = selected
            .as_ref()
            .is_some_and(|id| !profiles.iter().any(|p| &p.id == id))
            && matches!(self.kind, LocationDialogKind::New);
        self.location = selected
            .and_then(|id| profiles.iter().position(|p| p.id == id))
            .map_or(0, |i| i + 1);
        self.profiles = profiles;
    }
    pub fn location_label(&self) -> String {
        if self.location_missing {
            return "removed — choose a location".into();
        }
        self.profile()
            .map(|p| {
                format!(
                    "{}{}",
                    p.label,
                    if p.enabled {
                        ""
                    } else if self.machine_state() == Some(MachineState::Suspended) {
                        " (suspended)"
                    } else {
                        " (stopped / disabled)"
                    }
                )
            })
            .unwrap_or_else(|| "Local".into())
    }
    pub fn choice_field(&self, index: usize) -> bool {
        matches!(self.kind, LocationDialogKind::Manage) && index == 0
            || matches!(self.kind, LocationDialogKind::New) && index == 1
    }
    pub fn cycle_location(&mut self, delta: isize) {
        self.location_missing = false;
        self.location =
            (self.location as isize + delta).rem_euclid(self.profiles.len() as isize + 1) as usize;
        if matches!(self.kind, LocationDialogKind::New) {
            let cwd = self
                .profile()
                .and_then(|p| self.prefs.remotes.get(&p.id))
                .map(|o| o.cwd.as_str())
                .unwrap_or("");
            self.fields[2] = TextEditor::new(cwd, true);
        }
    }
    pub fn editor_mut(&mut self) -> Option<&mut TextEditor> {
        if self.busy
            || self.choice_field(self.selected)
            || matches!(
                self.kind,
                LocationDialogKind::Manage
                    | LocationDialogKind::Stop
                    | LocationDialogKind::Suspend
                    | LocationDialogKind::Delete(_)
                    | LocationDialogKind::DeleteImage(_)
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
    Ready(NewLocationWorkspace),
    Created {
        endpoint: ClientEndpointId,
        workspace: String,
        profile: Option<SavedSshEndpoint>,
        stamp: LocationStamp,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocationStamp {
    generation: Option<u64>,
    boot_id: String,
}

#[derive(Debug)]
struct NewLocationWorkspace {
    profile: Option<SavedSshEndpoint>,
    options: RemoteOptions,
    cwd: String,
    label: String,
}

impl NewLocationWorkspace {
    fn endpoint(&self) -> ClientEndpointId {
        self.profile
            .as_ref()
            .map(|p| ClientEndpointId::Ssh(p.id.clone()))
            .unwrap_or(ClientEndpointId::Local)
    }
}

#[derive(Debug)]
struct CreatedLocationWorkspace {
    epoch: u64,
    endpoint: ClientEndpointId,
    workspace: String,
    profile: Option<SavedSshEndpoint>,
    stamp: LocationStamp,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub(super) struct LocationController {
    add: add::AddRemoteController,
    epoch: u64,
    job: Option<(u64, mpsc::Receiver<Result<JobResult, String>>)>,
    prepared: Option<(u64, NewLocationWorkspace, Instant)>,
    created: Option<CreatedLocationWorkspace>,
    states: Option<(u64, mpsc::Receiver<MachineStates>)>,
    image_check: Option<(u64, mpsc::Receiver<Result<SaveCheck, String>>)>,
    /// Replaces the hangar request behind Save as image's check in tests.
    save_checker: Option<SaveChecker>,
}

type SaveChecker = fn(&HangarBinding) -> Result<SaveCheck, crate::hangar::api::HangarError>;

type MachineStates = Vec<((String, String), MachineState)>;

impl ClientShellState {
    fn location_dialog(&mut self, kind: LocationDialogKind) -> Result<LocationDialog, String> {
        let catalog = EndpointCatalog::load()?;
        let prefs = LocationPreferences::load()?;
        let location = prefs.default_index(&catalog.ssh);
        Ok(LocationDialog {
            kind,
            fields: Vec::new(),
            selected: 0,
            location,
            location_missing: false,
            profiles: catalog.ssh,
            prefs,
            message: String::new(),
            busy: false,
            machine_states: BTreeMap::new(),
        })
    }

    pub(super) fn open_locations(&mut self) {
        if self.locations.add.running() {
            self.open_add_remote();
            return;
        }
        if self.locations.job.is_some() {
            self.set_endpoint_error("A remote operation is still running");
            return;
        }
        self.locations.epoch = self.locations.epoch.wrapping_add(1);
        match self.location_dialog(LocationDialogKind::Manage) {
            Ok(mut dialog) => {
                dialog.message = LocationPreferences::take_notice().unwrap_or_else(|| {
                    "Choose a location with ←/→. Removing a hangar remote deletes its machine after you confirm."
                        .into()
                });
                self.overlay = Some(ClientShellOverlay::Locations(dialog));
                self.refresh_machine_states();
            }
            Err(error) => self.set_endpoint_error(error),
        }
    }

    /// Fetches hangar machine states for the remotes dialog on a worker. Unknown states
    /// (signed out, unreachable) just keep the plain labels.
    fn refresh_machine_states(&mut self) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_ref() else {
            return;
        };
        let mut servers = dialog
            .prefs
            .remotes
            .values()
            .filter_map(|options| options.cloud.as_ref())
            .map(|cloud| cloud.hangar().server.clone())
            .collect::<Vec<_>>();
        servers.sort();
        servers.dedup();
        if servers.is_empty() {
            return;
        }
        let (send, receive) = mpsc::channel();
        self.locations.states = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let mut states = Vec::new();
            for server in servers {
                match backend::hangar::machine_states(&server) {
                    Ok(list) => states.extend(
                        list.into_iter()
                            .map(|(id, state)| ((server.clone(), id), state)),
                    ),
                    Err(error) => tracing::debug!(%error, "could not read hangar machine states"),
                }
            }
            let _ = send.send(states);
        });
    }

    pub(super) fn open_location_workspace(&mut self) {
        if self.locations.job.is_some() {
            self.set_endpoint_error("A remote operation is still running");
            return;
        }
        self.locations.epoch = self.locations.epoch.wrapping_add(1);
        match self.location_dialog(LocationDialogKind::New) {
            Ok(mut dialog) => {
                dialog.fields = vec![
                    TextEditor::new("", false),
                    TextEditor::default(),
                    TextEditor::default(),
                ];
                dialog.cycle_location(0);
                dialog.message = "All tabs and panes in this workspace run at this location. Empty directory uses its server default.".into();
                self.overlay = Some(ClientShellOverlay::Locations(dialog));
            }
            Err(error) => self.set_endpoint_error(error),
        }
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
            let mut dialog = self.location_dialog(LocationDialogKind::Edit(id.clone()))?;
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
                "This remote is a hangar machine; its SSH target is managed by Herdr.".into()
            } else {
                "Use an existing SSH alias. hangar machines are added from Add remote.".into()
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
                return Err("Add hangar machines from Add remote → Provider: hangar".into());
            }
            let options = RemoteOptions {
                cwd: values[3].clone(),
                // The binding is kept as is; it is never edited by hand.
                cloud: old_binding.cloned(),
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
        if dialog.location_missing {
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                dialog.message = "The selected remote was removed. Choose a location explicitly before creating.".into();
            }
            outcome.repaint = true;
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
            LocationDialogKind::New => {
                let cwd = dialog.fields[2].trim().to_owned();
                let label = dialog.fields[0].trim().to_owned();
                let ready = profile.as_ref().is_none_or(|p| {
                    p.enabled && self.endpoint_is_online(&ClientEndpointId::Ssh(p.id.clone()))
                });
                let intent = NewLocationWorkspace {
                    profile,
                    options,
                    cwd,
                    label,
                };
                if ready {
                    self.submit_location_workspace(intent);
                } else {
                    self.location_job(move || {
                        // New workspace never starts a hangar machine; a stopped one
                        // reports "use Start remote".
                        if let Some(p) = &intent.profile {
                            backend::start_session(p, &intent.options)?;
                        }
                        Ok(JobResult::Ready(intent))
                    });
                }
                Ok(())
            }
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
            LocationDialogKind::SaveImage(request) => {
                let request = (**request).clone();
                self.submit_save_image(request);
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
                    match &request.remote {
                        Some((profile, options)) => {
                            backend::delete_remote(profile, options, &mut |_| {})
                        }
                        None => backend::delete_unbound_machine(&request.binding, &mut |_| {}),
                    }
                    .map(JobResult::Message)
                });
                Ok(())
            }
            LocationDialogKind::Manage => {
                let selected = dialog.selected;
                match selected {
                    0 => {
                        if let Some(ClientShellOverlay::Locations(d)) = self.overlay.as_mut() {
                            d.cycle_location(1);
                        }
                        Ok(())
                    }
                    1 => {
                        self.open_add_remote();
                        Ok(())
                    }
                    4 => {
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
                            if let Some(ClientShellOverlay::Locations(d)) = self.overlay.as_mut() {
                                d.message =
                                    format!("New workspace defaults to {}", d.location_label());
                            }
                        }
                        result
                    }
                    _ => {
                        if let Some(profile) = profile {
                            self.remote_location_action(selected, profile, options)
                        } else {
                            Err("Select a remote location first".into())
                        }
                    }
                }
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
        action: usize,
        profile: SavedSshEndpoint,
        options: RemoteOptions,
    ) -> Result<(), String> {
        backend::validate_binding(&profile, options.cloud.as_ref())?;
        match action {
            2 => self.edit_location(Some(profile.id)),
            3 => {
                if !profile.enabled {
                    return Err("Remote is disabled. Use Start remote before testing SSH.".into());
                }
                self.location_job(move || {
                    crate::remote::check_saved_ssh(&profile.target, &profile.session)
                        .map(|()| JobResult::Message("SSH and Herdr session are ready".into()))
                        .map_err(|error| error.to_string())
                });
            }
            5 => {
                if options.cloud.is_none() {
                    return Err("This remote has no hangar machine".into());
                }
                self.location_job(move || {
                    backend::machine_status(&options).map(JobResult::Message)
                });
            }
            START_ROW => {
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
            7 => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Suspend remote requires a hangar machine.".into());
                };
                let name = cloud.hangar().machine_name.clone();
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.kind = LocationDialogKind::Suspend;
                    dialog.selected = 0;
                    dialog.message = format!("Suspend hangar machine {name}? Its memory is saved to a snapshot, so running programs and Herdr sessions continue after Resume remote. (Stop machine… shuts everything down instead; only files on disk remain.) Automatic connection is disabled until Resume remote.");
                }
            }
            8 => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Stop machine requires a hangar machine. Manage SSH-only server lifetime on its host.".into());
                };
                let name = cloud.hangar().machine_name.clone();
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.kind = LocationDialogKind::Stop;
                    dialog.selected = 0;
                    dialog.message = format!("Stop hangar machine {name}? All sessions and jobs on this machine stop. Files on its persistent disk remain; running processes do not survive (Suspend remote… keeps them). Automatic connection is disabled until Start remote.");
                }
            }
            SAVE_IMAGE_ROW => {
                let Some(cloud) = options.cloud.as_ref() else {
                    return Err("Save as image requires a hangar machine.".into());
                };
                let machine_name = cloud.hangar().machine_name.clone();
                self.show_save_image(SaveImageRequest {
                    machine_name,
                    profile,
                    options,
                    check: None,
                    error: None,
                });
                self.check_save_image();
            }
            10 | 11 => match options.cloud.clone() {
                Some(cloud) => self.confirm_delete(DeleteRequest {
                    binding: cloud.hangar().clone(),
                    remote: Some((profile, options)),
                }),
                None if action == 10 => {
                    return Err(
                        "This SSH remote has no hangar machine to delete. Use Remove profile."
                            .into(),
                    )
                }
                None => {
                    backend::remove_remote(&profile, &options)?;
                    self.open_locations();
                }
            },
            _ => {}
        }
        Ok(())
    }

    /// Switches the dialog to Save as image… with empty name and description.
    fn show_save_image(&mut self, request: SaveImageRequest) {
        if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
            dialog.message = request.message();
            dialog.kind = LocationDialogKind::SaveImage(Box::new(request));
            dialog.fields = vec![TextEditor::new("", false), TextEditor::new("", false)];
            dialog.selected = 0;
        }
    }

    /// Reads the machine's state and template on a worker; the result decides whether
    /// the dialog saves directly, stops first, or explains why it cannot save.
    fn check_save_image(&mut self) {
        let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::SaveImage(request),
            ..
        })) = self.overlay.as_ref()
        else {
            return;
        };
        let Some(cloud) = request.options.cloud.clone() else {
            return;
        };
        let checker = self
            .locations
            .save_checker
            .unwrap_or(backend::hangar::check_save);
        let (send, receive) = mpsc::channel();
        self.locations.image_check = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let result = checker(cloud.hangar()).map_err(|error| {
                format!(
                    "Could not check {}: {error} Close and reopen Save as image… to retry.",
                    cloud.hangar().machine_name
                )
            });
            let _ = send.send(result);
        });
    }

    fn submit_save_image(&mut self, request: SaveImageRequest) {
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
            dialog.message = SaveImageRequest {
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

    /// Shows what a confirmed delete destroys: the machine, its disks and snapshots,
    /// and every Herdr remote bound to it.
    pub(super) fn confirm_delete(&mut self, request: DeleteRequest) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        let cloud = backend::CloudBinding::Hangar(request.binding.clone());
        let labels = if request.remote.is_some() {
            backend::machine_profiles(&dialog.profiles, &dialog.prefs, &cloud)
                .iter()
                .map(|profile| profile.label.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
        dialog.message = backend::delete_confirmation(&request.binding.machine_name, &labels);
        dialog.kind = LocationDialogKind::Delete(Box::new(request));
        dialog.selected = 0;
    }

    pub(super) fn close_location(&mut self) {
        self.locations.epoch = self.locations.epoch.wrapping_add(1);
        self.locations.add.cancel_sign_in();
        self.locations.created = None;
        self.locations.prepared = None;
        self.overlay = None;
    }

    pub(super) fn route_location_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.route_add_remote_key(key, outcome) {
            return true;
        }
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return false;
        };
        outcome.repaint = true;
        if key.code == KeyCode::Esc {
            self.close_location();
            return true;
        }
        if dialog.busy {
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
            KeyCode::Left | KeyCode::Right if dialog.choice_field(dialog.selected) => {
                dialog.cycle_location(if key.code == KeyCode::Left { -1 } else { 1 })
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

    fn submit_location_workspace(&mut self, intent: NewLocationWorkspace) {
        let endpoint = intent.endpoint();
        let Some(stamp) = self.location_stamp(&endpoint) else {
            self.locations.prepared = Some((
                self.locations.epoch,
                intent,
                Instant::now() + Duration::from_secs(30),
            ));
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                dialog.busy = true;
                dialog.message = "Waiting for the destination session to connect…".into();
            }
            return;
        };
        self.location_job(move || {
            let workspace = backend::create_workspace(
                intent.profile.as_ref(),
                &intent.options,
                intent.cwd,
                intent.label,
            )?;
            Ok(JobResult::Created {
                endpoint,
                workspace,
                profile: intent.profile,
                stamp,
            })
        });
    }

    pub(crate) fn tick_locations(&mut self, outcome: &mut ClientShellInput) {
        self.tick_add_remote(outcome);
        let states = self
            .locations
            .states
            .as_ref()
            .and_then(|(epoch, receiver)| match receiver.try_recv() {
                Ok(states) => Some((*epoch, Some(states))),
                Err(mpsc::TryRecvError::Disconnected) => Some((*epoch, None)),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some((epoch, states)) = states {
            self.locations.states = None;
            if let (Some(states), Some(ClientShellOverlay::Locations(dialog))) =
                (states, self.overlay.as_mut())
            {
                if epoch == self.locations.epoch {
                    dialog.machine_states = states.into_iter().collect();
                    outcome.repaint = true;
                }
            }
        }
        let checked = self
            .locations
            .image_check
            .as_ref()
            .and_then(|(epoch, receiver)| match receiver.try_recv() {
                Ok(result) => Some((*epoch, result)),
                Err(mpsc::TryRecvError::Disconnected) => Some((
                    *epoch,
                    Err("The machine check stopped unexpectedly.".into()),
                )),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some((epoch, result)) = checked {
            self.locations.image_check = None;
            if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                if let LocationDialogKind::SaveImage(request) = &mut dialog.kind {
                    if epoch == self.locations.epoch {
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
        if let Some((epoch, result)) = received {
            self.locations.job = None;
            let succeeded = result.is_ok();
            let current = epoch == self.locations.epoch
                && matches!(self.overlay, Some(ClientShellOverlay::Locations(_)));
            let message = match result {
                Ok(JobResult::Message(message)) => message,
                Ok(JobResult::Ready(intent)) => {
                    if current {
                        self.locations.prepared =
                            Some((epoch, intent, Instant::now() + Duration::from_secs(30)));
                    }
                    "Remote started. Waiting for its session snapshot…".into()
                }
                Ok(JobResult::Created {
                    endpoint,
                    workspace,
                    profile,
                    stamp,
                }) => {
                    if current {
                        self.locations.created = Some(CreatedLocationWorkspace {
                            epoch,
                            endpoint,
                            workspace: workspace.clone(),
                            profile,
                            stamp,
                            deadline: Instant::now() + Duration::from_secs(20),
                        });
                        format!("Workspace {workspace} created. Waiting for its machine snapshot…")
                    } else {
                        format!("Workspace {workspace} created on its selected machine.")
                    }
                }
                Err(error) => error,
            };
            if current {
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.busy =
                        self.locations.created.is_some() || self.locations.prepared.is_some();
                    dialog.message = message.clone();
                    let saved =
                        succeeded && matches!(dialog.kind, LocationDialogKind::SaveImage(_));
                    if let (LocationDialogKind::SaveImage(request), false) =
                        (&mut dialog.kind, succeeded)
                    {
                        // The attempt may have stopped the machine: check it again and
                        // keep the error visible meanwhile.
                        request.error = Some(message);
                        request.check = None;
                        dialog.message = request.message();
                        recheck_image = true;
                    }
                    if saved
                        || matches!(
                            dialog.kind,
                            LocationDialogKind::Delete(_) | LocationDialogKind::DeleteImage(_)
                        )
                    {
                        // A finished delete or save never stays armed for a second Enter.
                        dialog.kind = LocationDialogKind::Manage;
                        dialog.selected = 0;
                    }
                    if let Ok(prefs) = LocationPreferences::load() {
                        dialog.prefs = prefs;
                    }
                    if let Ok(profiles) = EndpointCatalog::load_profiles() {
                        dialog.replace_profiles(profiles);
                    }
                    refresh_states = true;
                }
            } else {
                self.set_endpoint_error(message);
            }
            outcome.repaint = true;
        }
        if refresh_states {
            self.refresh_machine_states();
        }
        if recheck_image {
            self.check_save_image();
        }
        let owns_new_dialog = matches!(
            self.overlay,
            Some(ClientShellOverlay::Locations(LocationDialog {
                kind: LocationDialogKind::New,
                ..
            }))
        );
        if let Some((epoch, intent, deadline)) = self.locations.prepared.take() {
            if epoch != self.locations.epoch || !owns_new_dialog || Instant::now() > deadline {
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.busy = false;
                    dialog.message =
                        "Remote connection was cancelled or timed out; no workspace was created."
                            .into();
                }
                outcome.repaint = true;
            } else if self.location_stamp(&intent.endpoint()).is_some() {
                self.submit_location_workspace(intent);
                outcome.repaint = true;
            } else {
                self.locations.prepared = Some((epoch, intent, deadline));
            }
        }
        if let Some(created) = self.locations.created.as_ref() {
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
            if created.epoch != self.locations.epoch
                || !owns_new_dialog
                || stale
                || Instant::now() > created.deadline
            {
                self.locations.created = None;
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.busy = false;
                    dialog.message = "Workspace created; automatic focus cancelled because its connection or dialog changed. Select it from the sidebar.".into();
                }
                outcome.repaint = true;
            } else if visible {
                let unchanged = created.profile.as_ref().is_none_or(|profile| {
                    EndpointCatalog::load_profiles().is_ok_and(|profiles| {
                        profiles
                            .iter()
                            .any(|p| backend::same_destination(p, profile) && p.enabled)
                    })
                });
                let endpoint = created.endpoint.clone();
                let workspace = created.workspace.clone();
                self.locations.created = None;
                if unchanged {
                    self.overlay = None;
                    self.focus_or_activate(
                        endpoint,
                        ClientEndpointFocusTarget::Workspace(workspace),
                        outcome,
                    );
                } else if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.busy = false;
                    dialog.message = "Workspace created, but the remote profile changed. Automatic focus cancelled.".into();
                }
                outcome.repaint = true;
            }
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
            kind: LocationDialogKind::New,
            fields: vec![
                TextEditor::default(),
                TextEditor::default(),
                TextEditor::new("/local/project", false),
            ],
            selected: 1,
            location: 0,
            location_missing: false,
            profiles: vec![profile],
            prefs,
            message: String::new(),
            busy: false,
            machine_states: BTreeMap::new(),
        }
    }

    pub(super) fn shell() -> ClientShellState {
        ClientShellState::new(ClientShellConfig::from_config(&Config::default()))
    }

    const MACHINE: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";

    /// A Manage dialog whose first two remotes share one hangar machine.
    fn hangar_dialog() -> LocationDialog {
        let mut dialog = dialog();
        let binding = HangarBinding::new("https://hangar.test", MACHINE, "box").unwrap();
        let alias = binding.alias.clone();
        let first = SavedSshEndpoint::new("box", &alias, "herdr-remote").unwrap();
        let second = SavedSshEndpoint::new("box agents", &alias, "agents").unwrap();
        for profile in [&first, &second] {
            dialog.prefs.remotes.insert(
                profile.id.clone(),
                RemoteOptions {
                    cwd: String::new(),
                    cloud: Some(backend::CloudBinding::Hangar(binding.clone())),
                },
            );
        }
        dialog.profiles.insert(0, second);
        dialog.profiles.insert(0, first);
        dialog.kind = LocationDialogKind::Manage;
        dialog.location = 1;
        dialog.selected = 0;
        dialog
    }

    #[test]
    fn remove_remote_confirmation_names_machine_and_every_bound_remote() {
        let mut state = shell();
        let dialog = hangar_dialog();
        let profile = dialog.profiles[0].clone();
        let options = dialog.prefs.remotes[&profile.id].clone();
        let binding = options.cloud.as_ref().unwrap().hangar().clone();
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.confirm_delete(DeleteRequest {
            binding,
            remote: Some((profile, options)),
        });
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert!(matches!(dialog.kind, LocationDialogKind::Delete(_)));
        assert!(dialog.message.contains("'box'"), "{}", dialog.message);
        assert!(dialog
            .message
            .contains("disks and snapshots are permanently deleted"));
        assert!(
            dialog.message.contains("'box', 'box agents'"),
            "{}",
            dialog.message
        );
        assert!(!dialog.message.contains("Remote A"));
        assert!(state.locations.job.is_none(), "nothing runs before Enter");
        state.compose(110, 35).unwrap();
    }

    #[test]
    fn a_finished_delete_returns_to_the_remote_list_instead_of_staying_armed() {
        let mut state = shell();
        let mut dialog = hangar_dialog();
        let binding = HangarBinding::new("https://hangar.test", MACHINE, "box").unwrap();
        dialog.kind = LocationDialogKind::Delete(Box::new(DeleteRequest {
            binding,
            remote: None,
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

    fn save_image_shell(check: Option<SaveCheck>) -> ClientShellState {
        let mut state = shell();
        state.locations.save_checker = Some(|_| {
            Err(crate::hangar::api::HangarError::Invalid(
                "offline in tests".into(),
            ))
        });
        let dialog = hangar_dialog();
        let profile = dialog.profiles[0].clone();
        let options = dialog.prefs.remotes[&profile.id].clone();
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.show_save_image(SaveImageRequest {
            profile,
            options,
            machine_name: "box".into(),
            check,
            error: None,
        });
        state
    }

    fn save_request(state: &ClientShellState) -> (&LocationDialog, &SaveImageRequest) {
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        let LocationDialogKind::SaveImage(request) = &dialog.kind else {
            panic!("save as image");
        };
        (dialog, request)
    }

    fn deliver_check(state: &mut ClientShellState, epoch: u64, check: SaveCheck) {
        let (send, receive) = mpsc::channel();
        state.locations.image_check = Some((epoch, receive));
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
    fn save_as_image_sits_next_to_the_machine_actions_and_states_what_is_saved() {
        let dialog = hangar_dialog();
        assert_eq!(dialog.row_label(SAVE_IMAGE_ROW - 1), "Stop machine…");
        assert_eq!(dialog.row_label(SAVE_IMAGE_ROW), "Save as image…");
        assert_eq!(dialog.row_label(SAVE_IMAGE_ROW + 1), "Remove remote…");
        let mut state = save_image_shell(None);
        let (dialog, request) = save_request(&state);
        assert_eq!(dialog.labels(), ["Image name", "Description"]);
        assert!(dialog.message.contains("Checking box…"));
        for part in [
            "root disk only",
            "Installed packages and system configuration are included",
            "home directory files, logins and /data/workspace are not",
            "`sudo gh auth`",
            "/etc/environment",
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
        assert_eq!(save_request(&state).0.fields[0].as_str(), "basex");
        state.compose(110, 35).unwrap();
    }

    #[test]
    fn a_running_machine_offers_stop_machine_and_save_and_old_templates_refuse() {
        let mut state = save_image_shell(None);
        let epoch = state.locations.epoch;
        let running = SaveCheck {
            plan: Some(SavePlan::Stop),
            note: "box is running. Only a stopped machine can be saved.".into(),
        };
        deliver_check(&mut state, epoch, running);
        let (dialog, request) = save_request(&state);
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
        assert_eq!(save_request(&state).1.plan(), Some(SavePlan::Stop));

        let mut state = save_image_shell(Some(SaveCheck {
            plan: None,
            note: "box was created from template herdr@old, which is too old to save images from."
                .into(),
        }));
        type_name(&mut state, "base");
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none(), "an old template never saves");
        assert_eq!(save_request(&state).1.primary_label(), " ↵ save ");
    }

    #[test]
    fn an_invalid_image_name_stops_nothing() {
        let mut state = save_image_shell(Some(SaveCheck {
            plan: Some(SavePlan::Stop),
            note: String::new(),
        }));
        type_name(&mut state, "Not Valid");
        state.accept_location(&mut ClientShellInput::default());
        assert!(state.locations.job.is_none());
        let (dialog, _) = save_request(&state);
        assert!(dialog.message.starts_with("Image names use"));
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn a_failed_save_stays_open_and_rechecks_while_a_saved_one_returns_to_the_list() {
        let mut state = save_image_shell(Some(SaveCheck {
            plan: Some(SavePlan::Save),
            note: String::new(),
        }));
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        send.send(Err(
            "Could not save image base: an image named \"base\" already exists".into(),
        ))
        .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let (dialog, request) = save_request(&state);
        assert!(dialog.message.contains("already exists"));
        assert!(dialog.message.contains("Checking box…"));
        assert!(request.check.is_none());
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
    fn a_suspended_machine_offers_resume_and_is_labelled_suspended() {
        let mut state = shell();
        let mut dialog = hangar_dialog();
        dialog.profiles[0].enabled = false;
        assert_eq!(dialog.row_label(START_ROW), "Start remote");
        assert!(dialog.location_label().ends_with("(stopped / disabled)"));
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        let (send, receive) = mpsc::channel();
        state.locations.states = Some((state.locations.epoch, receive));
        send.send(vec![(
            ("https://hangar.test".to_owned(), MACHINE.to_owned()),
            MachineState::Suspended,
        )])
        .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert_eq!(dialog.row_label(START_ROW), "Resume remote");
        assert!(dialog.location_label().ends_with("(suspended)"));
        assert_eq!(dialog.row_label(START_ROW + 1), "Suspend remote…");
        // A state result for an older dialog is ignored.
        let (send, receive) = mpsc::channel();
        state.locations.states = Some((state.locations.epoch.wrapping_sub(1), receive));
        send.send(Vec::new()).unwrap();
        state.tick_locations(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        assert_eq!(dialog.machine_state(), Some(MachineState::Suspended));
    }

    #[test]
    fn changing_location_resets_directory_to_that_machines_default() {
        let mut dialog = dialog();
        dialog.cycle_location(1);
        assert_eq!(dialog.fields[2].as_str(), "/remote/project");
        dialog.fields[2] = TextEditor::new("/remote/edited", false);
        dialog.cycle_location(-1);
        assert!(dialog.fields[2].is_empty());
    }

    #[test]
    fn mouse_and_keyboard_location_selection_have_the_same_directory_policy() {
        let mut state = shell();
        state.overlay = Some(ClientShellOverlay::Locations(dialog()));
        let mut keyboard = shell();
        keyboard.overlay = Some(ClientShellOverlay::Locations(dialog()));
        keyboard.route_location_key(
            &crate::input::TerminalKey::from(crossterm::event::KeyEvent::new(
                KeyCode::Right,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut ClientShellInput::default(),
        );
        state.compose(110, 35).unwrap();
        let (rect, _) = state
            .hits
            .settings_choices
            .iter()
            .find(|(_, i)| *i == 1)
            .unwrap();
        state.handle_mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
        let Some(ClientShellOverlay::Locations(mouse)) = state.overlay.as_ref() else {
            panic!("dialog");
        };
        let Some(ClientShellOverlay::Locations(keys)) = keyboard.overlay.as_ref() else {
            panic!("dialog");
        };
        assert_eq!(mouse.location, keys.location);
        assert_eq!(mouse.fields[2], keys.fields[2]);
    }

    #[test]
    fn cancelled_create_result_never_steals_focus() {
        let mut state = shell();
        state.overlay = Some(ClientShellOverlay::Locations(dialog()));
        let (send, receive) = mpsc::channel();
        state.locations.job = Some((state.locations.epoch, receive));
        state.close_location();
        send.send(Ok(JobResult::Created {
            endpoint: ClientEndpointId::Local,
            workspace: "ws_1".into(),
            profile: None,
            stamp: LocationStamp {
                generation: None,
                boot_id: "boot-1".into(),
            },
        }))
        .unwrap();
        let mut outcome = ClientShellInput::default();
        state.tick_locations(&mut outcome);
        assert!(state.locations.created.is_none());
        assert!(outcome.actions.is_empty());
        assert!(state.overlay.is_none());
    }

    #[test]
    fn create_focus_waits_for_destination_snapshot_despite_colliding_local_ids() {
        let mut state = shell();
        let dialog = dialog();
        let endpoint = ClientEndpointId::Ssh(dialog.profiles[0].id.clone());
        state.set_endpoint_catalog(&dialog.profiles);
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state.locations.created = Some(CreatedLocationWorkspace {
            epoch: state.locations.epoch,
            endpoint: endpoint.clone(),
            workspace: "ws_1".into(),
            profile: None,
            stamp: LocationStamp {
                generation: None,
                boot_id: "boot-1".into(),
            },
            deadline: Instant::now() + Duration::from_secs(5),
        });
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
    fn remote_management_survives_disconnection_and_consumes_paste_locally() {
        let mut state = shell();
        let mut dialog = dialog();
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
        assert!(dialog.location_missing);
        dialog.cycle_location(1);
        assert!(!dialog.location_missing);
        assert!(dialog.fields[2].is_empty());
    }

    #[test]
    fn popup_takeover_or_server_reboot_cancels_delayed_create_focus() {
        for popup in [true, false] {
            let mut state = shell();
            state.set_snapshot(Box::new(super::super::tests::snapshot()));
            state.overlay = Some(ClientShellOverlay::Locations(dialog()));
            state.locations.created = Some(CreatedLocationWorkspace {
                epoch: state.locations.epoch,
                endpoint: ClientEndpointId::Local,
                workspace: "ws_1".into(),
                profile: None,
                stamp: LocationStamp {
                    generation: None,
                    boot_id: if popup { "boot-1" } else { "prior-boot" }.into(),
                },
                deadline: Instant::now() + Duration::from_secs(10),
            });
            if popup {
                state.overlay = None;
            }
            let mut outcome = ClientShellInput::default();
            state.tick_locations(&mut outcome);
            assert!(outcome.actions.is_empty());
            assert!(state.locations.created.is_none());
        }
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
        assert!(!state.hits.settings_choices.is_empty());
    }
}
