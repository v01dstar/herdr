//! Add remote: a hangar machine (sign in, then pick an existing machine or create one)
//! or a manual SSH profile. HTTP and SSH run in workers; results carry the dialog
//! epoch so a late result never overrides a newer dialog or selection.
use super::*;
use crate::client::locations::hangar::{self as machines, MachineSource};
use crate::hangar::api::{HangarError, Image, Machine, MachineState};
use crate::hangar::login::SignInStep;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(in crate::client::shell) const NAME_FIELD: usize = 2;
/// What a new machine is created from: the latest template or one of the images.
pub(in crate::client::shell) const SOURCE_FIELD: usize = 3;
/// Delete image… for the image chosen as source.
pub(in crate::client::shell) const IMAGE_ACTION_FIELD: usize = 4;
const FIELDS: usize = 5;
const TEMPLATE_SOURCE: &str = "Latest herdr template";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum MachineChoice {
    New,
    Existing(String),
}

#[derive(Clone, Debug)]
pub(in crate::client::shell) struct AddRemoteForm {
    server: String,
    machines: Vec<Machine>,
    /// Machine IDs that already have a saved remote on this server.
    bound: Vec<String>,
    /// The caller's images, newest first; empty when hangar could not list them.
    images: Vec<Image>,
    pub(in crate::client::shell) choice: MachineChoice,
    pub(in crate::client::shell) source: MachineSource,
    pub(in crate::client::shell) dropdown: Option<(usize, usize)>,
    loading: bool,
    signed_in: bool,
    /// The sign-in step in progress (browser or device code).
    sign_in: Option<SignInStep>,
}

impl Default for AddRemoteForm {
    fn default() -> Self {
        Self {
            server: String::new(),
            machines: Vec::new(),
            bound: Vec::new(),
            images: Vec::new(),
            choice: MachineChoice::New,
            source: MachineSource::Template,
            dropdown: None,
            loading: true,
            signed_in: false,
            sign_in: None,
        }
    }
}

fn machine_label(machine: &Machine, bound: bool) -> String {
    let state = match machine.state {
        MachineState::Running => String::new(),
        state => format!(" · {}", state.as_str()),
    };
    let added = if bound { " (added)" } else { "" };
    format!("{}{state}{added}", machine.name)
}

/// `2026-10-03T08:00:00Z` → `2026-10-03`.
fn created_date(created_at: &str) -> &str {
    created_at.get(..10).unwrap_or(created_at)
}

impl AddRemoteForm {
    fn sources(&self) -> Vec<MachineSource> {
        std::iter::once(MachineSource::Template)
            .chain(self.images.iter().map(|image| MachineSource::Image {
                id: image.id.clone(),
                name: image.name.clone(),
            }))
            .collect()
    }

    fn image(&self, id: &str) -> Option<&Image> {
        self.images.iter().find(|image| image.id == id)
    }

    /// Name, creation date and what it was saved from (the machine while it exists).
    fn image_label(&self, image: &Image) -> String {
        let source = self
            .machine(&image.source_machine_id)
            .map(|machine| machine.name.clone())
            .unwrap_or_else(|| image.template.label());
        format!(
            "{} · {} · from {source}",
            image.name,
            created_date(&image.created_at)
        )
    }

    fn source_label(&self, source: &MachineSource) -> String {
        match source {
            MachineSource::Template => TEMPLATE_SOURCE.into(),
            MachineSource::Image { id, .. } => self
                .image(id)
                .map(|image| self.image_label(image))
                .unwrap_or_else(|| "Image no longer listed".into()),
        }
    }

    /// The image chosen as source, offered for deletion.
    pub(in crate::client::shell) fn deletable_image(&self) -> Option<&Image> {
        match &self.source {
            MachineSource::Image { id, .. } if self.signed_in && self.creating() => self.image(id),
            _ => None,
        }
    }

    fn selections(&self) -> Vec<MachineChoice> {
        std::iter::once(MachineChoice::New)
            .chain(
                self.machines
                    .iter()
                    .map(|machine| MachineChoice::Existing(machine.id.clone())),
            )
            .collect()
    }

    fn machine(&self, id: &str) -> Option<&Machine> {
        self.machines.iter().find(|machine| machine.id == id)
    }

    /// The selected machine when no Herdr remote uses it, so it can be deleted here.
    pub(in crate::client::shell) fn deletable(&self) -> Option<&Machine> {
        match &self.choice {
            MachineChoice::Existing(id) if self.signed_in && !self.bound.contains(id) => {
                self.machine(id)
            }
            _ => None,
        }
    }

    pub(in crate::client::shell) fn creating(&self) -> bool {
        self.choice == MachineChoice::New
    }

    pub(in crate::client::shell) fn edits_name(&self) -> bool {
        self.signed_in && self.creating() && self.dropdown.is_none()
    }

    pub(in crate::client::shell) fn options(&self, field: usize) -> Vec<String> {
        match field {
            0 => vec!["hangar".into(), "SSH".into()],
            1 if self.signed_in => self
                .selections()
                .iter()
                .map(|choice| match choice {
                    MachineChoice::New => "＋ Create new machine".into(),
                    MachineChoice::Existing(id) => self
                        .machine(id)
                        .map(|machine| machine_label(machine, self.bound.contains(id)))
                        .unwrap_or_default(),
                })
                .collect(),
            SOURCE_FIELD if self.signed_in && self.creating() => self
                .sources()
                .iter()
                .map(|source| self.source_label(source))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(in crate::client::shell) fn label(&self, field: usize) -> String {
        match field {
            0 => "hangar".into(),
            1 if self.loading => "Loading…".into(),
            1 if !self.signed_in => "Sign in to list machines".into(),
            1 => match &self.choice {
                MachineChoice::New => "＋ Create new machine".into(),
                MachineChoice::Existing(id) => self
                    .machine(id)
                    .map(|machine| machine_label(machine, self.bound.contains(id)))
                    .unwrap_or_else(|| "Machine no longer listed".into()),
            },
            SOURCE_FIELD => self.source_label(&self.source),
            _ => String::new(),
        }
    }

    pub(in crate::client::shell) fn primary_label(&self) -> &'static str {
        if !self.signed_in {
            " sign in "
        } else if self.creating() {
            " create and connect "
        } else {
            " connect "
        }
    }

    pub(in crate::client::shell) fn can_submit(&self) -> bool {
        !self.loading
            && (!self.signed_in
                || match &self.choice {
                    MachineChoice::New => match &self.source {
                        MachineSource::Template => true,
                        MachineSource::Image { id, .. } => self.image(id).is_some(),
                    },
                    MachineChoice::Existing(id) => {
                        self.machine(id).is_some() && !self.bound.contains(id)
                    }
                })
    }

    fn open_dropdown(&mut self, field: usize) {
        let available = field == 0
            || ((field == 1 || field == SOURCE_FIELD) && !self.loading && self.signed_in);
        if available && !self.options(field).is_empty() {
            let selected = match field {
                1 => self
                    .selections()
                    .iter()
                    .position(|choice| choice == &self.choice)
                    .unwrap_or(0),
                SOURCE_FIELD => self
                    .sources()
                    .iter()
                    .position(|source| source == &self.source)
                    .unwrap_or(0),
                _ => 0,
            };
            self.dropdown = Some((field, selected));
        }
    }

    /// A refreshed list keeps the user's choices while that machine and image are still
    /// listed.
    fn apply_machines(
        &mut self,
        server: String,
        machines: Vec<Machine>,
        bound: Vec<String>,
        images: Vec<Image>,
    ) {
        if let MachineChoice::Existing(id) = &self.choice {
            if !machines.iter().any(|machine| &machine.id == id) {
                self.choice = MachineChoice::New;
            }
        }
        if let MachineSource::Image { id, .. } = &self.source {
            if !images.iter().any(|image| &image.id == id) {
                self.source = MachineSource::Template;
            }
        }
        self.server = server;
        self.machines = machines;
        self.bound = bound;
        self.images = images;
        self.loading = false;
        self.signed_in = true;
        self.sign_in = None;
    }
}

enum Discovery {
    Machines {
        server: String,
        result: Result<Vec<Machine>, HangarError>,
        bound: Vec<String>,
        images: Vec<Image>,
    },
    SignIn(SignInStep),
    SignInFailed(String),
}

enum SetupEvent {
    Progress(String),
    Finished(Result<String, String>),
}

#[derive(Default)]
pub(super) struct AddRemoteController {
    discovery: Option<(u64, mpsc::Receiver<Discovery>)>,
    sign_in_cancel: Option<Arc<AtomicBool>>,
    job: Option<mpsc::Receiver<SetupEvent>>,
    running_form: Option<AddRemoteForm>,
    status: String,
}

impl std::fmt::Debug for AddRemoteController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddRemoteController")
            .field("running", &self.running())
            .finish()
    }
}

impl AddRemoteController {
    pub fn running(&self) -> bool {
        self.job.is_some()
    }

    pub fn cancel_sign_in(&mut self) {
        if let Some(cancel) = self.sign_in_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
    }
}

const UNBOUND_HINT: &str = "No Herdr remote uses it; Delete machine… deletes it instead.";
const CREATE_HINT: &str = "Creates a hangar machine from the herdr template with its default size and a persistent disk. Closing Herdr keeps it running; stop it from Settings → remotes.";

fn image_hint(name: &str) -> String {
    format!("Creates a hangar machine from image {name}: the packages and system configuration saved in it, with a new, empty /data (no home directory files, logins or workspace). Delete image… removes the image.")
}

fn bound_machines(server: &str) -> Vec<String> {
    LocationPreferences::load()
        .map(|prefs| {
            prefs
                .remotes
                .values()
                .filter_map(|options| options.cloud.as_ref())
                .map(|cloud| cloud.hangar())
                .filter(|binding| binding.server == server)
                .map(|binding| binding.machine_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn list_worker(send: &mpsc::Sender<Discovery>) {
    let server = crate::hangar::default_server();
    let result = machines::list_machines(&server);
    // Images are optional: a server without them, or a failed listing, offers only the
    // template.
    let images = if result.is_ok() {
        machines::list_images(&server).unwrap_or_else(|error| {
            tracing::debug!(%error, "could not list hangar images");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let bound = bound_machines(&server);
    let _ = send.send(Discovery::Machines {
        server,
        result,
        bound,
        images,
    });
}

impl ClientShellState {
    pub(super) fn open_add_remote(&mut self) {
        self.locations.epoch = self.locations.epoch.wrapping_add(1);
        let form = self.locations.add.running_form.clone().unwrap_or_default();
        match self.location_dialog(LocationDialogKind::Add(Box::new(form))) {
            Ok(mut dialog) => {
                dialog.fields = vec![TextEditor::new("", false)];
                dialog.busy = self.locations.add.running();
                dialog.message = if dialog.busy {
                    self.locations.add.status.clone()
                } else {
                    "Loading your hangar machines…".into()
                };
                self.overlay = Some(ClientShellOverlay::Locations(dialog));
                if !self.locations.add.running() {
                    self.discover_machines();
                }
            }
            Err(e) => self.set_endpoint_error(e),
        }
    }

    fn discover_machines(&mut self) {
        let (send, receive) = mpsc::channel();
        self.locations.add.discovery = Some((self.locations.epoch, receive));
        std::thread::spawn(move || list_worker(&send));
    }

    fn start_sign_in(&mut self) {
        self.locations.add.cancel_sign_in();
        let cancel = Arc::new(AtomicBool::new(false));
        self.locations.add.sign_in_cancel = Some(cancel.clone());
        let (send, receive) = mpsc::channel();
        self.locations.add.discovery = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let result = machines::sign_in(
                &mut |step| {
                    let _ = send.send(Discovery::SignIn(step));
                },
                &|| cancel.load(Ordering::Acquire),
            );
            match result {
                Ok(()) => list_worker(&send),
                Err(error) => {
                    let _ = send.send(Discovery::SignInFailed(error.to_string()));
                }
            }
        });
    }

    fn select_add_option(&mut self, field: usize, index: usize) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        if dialog.busy {
            return;
        }
        let LocationDialogKind::Add(form) = &mut dialog.kind else {
            return;
        };
        form.dropdown = None;
        match field {
            0 if index == 1 => {
                self.locations.add.cancel_sign_in();
                self.edit_location(None);
            }
            1 => {
                if let Some(choice) = form.selections().get(index).cloned() {
                    let message = match &choice {
                        MachineChoice::New => CREATE_HINT.to_owned(),
                        MachineChoice::Existing(id) if form.bound.contains(id) => {
                            "This machine is already added. Select it in Settings → remotes.".into()
                        }
                        MachineChoice::Existing(id) => match form.machine(id) {
                            Some(machine) if machine.state == MachineState::Running => {
                                format!("Connects to the machine's Herdr session. Its files are kept. {UNBOUND_HINT}")
                            }
                            Some(machine) => format!(
                                "{} is {}. It is saved without starting it; use Start remote when you need it. {UNBOUND_HINT}",
                                machine.name,
                                machine.state.as_str()
                            ),
                            None => String::new(),
                        },
                    };
                    if choice == MachineChoice::New {
                        dialog.selected = NAME_FIELD;
                    }
                    form.choice = choice;
                    dialog.message = message;
                }
            }
            SOURCE_FIELD => {
                if let Some(source) = form.sources().get(index).cloned() {
                    dialog.message = match &source {
                        MachineSource::Template => CREATE_HINT.to_owned(),
                        MachineSource::Image { name, .. } => image_hint(name),
                    };
                    form.source = source;
                }
            }
            _ => {}
        }
    }

    pub(super) fn accept_add_remote(&mut self, outcome: &mut ClientShellInput) {
        outcome.repaint = true;
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        if dialog.busy || self.locations.add.running() {
            return;
        }
        let LocationDialogKind::Add(form) = &mut dialog.kind else {
            return;
        };
        if let Some((field, index)) = form.dropdown {
            self.select_add_option(field, index);
            return;
        }
        if form.loading {
            return;
        }
        if !form.signed_in {
            if form.sign_in.is_none() {
                form.loading = true;
                dialog.message = "Starting sign-in…".into();
                self.start_sign_in();
            }
            return;
        }
        if !form.can_submit() {
            dialog.message = if form.creating() {
                "The chosen image is no longer listed. Choose another source.".into()
            } else {
                "Choose a machine that is not added yet, or create a new one.".into()
            };
            return;
        }
        let server = form.server.clone();
        let source = form.source.clone();
        let existing = match &form.choice {
            MachineChoice::New => None,
            MachineChoice::Existing(id) => form.machine(id).cloned(),
        };
        let name = dialog
            .fields
            .first()
            .map(|field| field.trim().to_owned())
            .unwrap_or_default();
        if existing.is_none() && name.is_empty() {
            dialog.selected = NAME_FIELD;
            dialog.message = "Enter a name for the new machine.".into();
            return;
        }
        if let LocationDialogKind::Add(form) = &dialog.kind {
            self.locations.add.running_form = Some((**form).clone());
        }
        let (send, receive) = mpsc::channel();
        self.locations.add.job = Some(receive);
        self.locations.add.status = "Preparing your remote…".into();
        dialog.message = self.locations.add.status.clone();
        dialog.busy = true;
        std::thread::spawn(move || {
            let mut progress = |message: String| {
                let _ = send.send(SetupEvent::Progress(message));
            };
            let result = (|| {
                let machine = match existing {
                    Some(machine) => machine,
                    None => machines::create_machine(&server, &name, &source, &mut progress)
                        .map_err(|error| error.to_string())?,
                };
                machines::add_machine(&server, &machine, &mut progress)
            })();
            let _ = send.send(SetupEvent::Finished(result));
        });
    }

    /// Delete machine… for a hangar machine that no Herdr remote uses.
    fn confirm_delete_unbound(&mut self) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        let LocationDialogKind::Add(form) = &dialog.kind else {
            return;
        };
        let Some(machine) = form.deletable() else {
            return;
        };
        match HangarBinding::new(&form.server, &machine.id, &machine.name) {
            Ok(binding) => {
                self.locations.add.cancel_sign_in();
                self.confirm_delete(DeleteRequest {
                    binding,
                    remote: None,
                });
            }
            Err(error) => dialog.message = error,
        }
    }

    /// Delete image… for the image chosen as source.
    fn confirm_delete_image(&mut self) {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return;
        };
        let LocationDialogKind::Add(form) = &dialog.kind else {
            return;
        };
        let Some(image) = form.deletable_image() else {
            return;
        };
        let request = ImageDeleteRequest {
            server: form.server.clone(),
            id: image.id.clone(),
            name: image.name.clone(),
        };
        self.locations.add.cancel_sign_in();
        dialog.message = backend::image_delete_confirmation(&request.name);
        dialog.kind = LocationDialogKind::DeleteImage(Box::new(request));
        dialog.selected = 0;
    }

    pub(super) fn route_add_remote_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return false;
        };
        let LocationDialogKind::Add(form) = &mut dialog.kind else {
            return false;
        };
        outcome.repaint = true;
        if key.code == KeyCode::Esc {
            if form.dropdown.take().is_none() {
                self.close_location();
            }
            return true;
        }
        if dialog.busy {
            return true;
        }
        if let Some((field, selected)) = form.dropdown {
            let count = form.options(field).len();
            match key.code {
                KeyCode::Down if count > 0 => form.dropdown = Some((field, (selected + 1) % count)),
                KeyCode::Up if count > 0 => {
                    form.dropdown = Some((field, (selected + count - 1) % count))
                }
                KeyCode::Enter => self.select_add_option(field, selected),
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => dialog.selected = (dialog.selected + 1) % (FIELDS + 1),
            KeyCode::BackTab | KeyCode::Up => {
                dialog.selected = (dialog.selected + FIELDS) % (FIELDS + 1)
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ')
                if dialog.selected < NAME_FIELD
                    || (dialog.selected == SOURCE_FIELD && form.creating()) =>
            {
                form.open_dropdown(dialog.selected)
            }
            KeyCode::Enter if dialog.selected == NAME_FIELD && form.deletable().is_some() => {
                self.confirm_delete_unbound()
            }
            KeyCode::Enter
                if dialog.selected == IMAGE_ACTION_FIELD && form.deletable_image().is_some() =>
            {
                self.confirm_delete_image()
            }
            KeyCode::Enter => self.accept_add_remote(outcome),
            _ => {
                if let Some(editor) = dialog.editor_mut() {
                    editor.handle_key(key);
                }
            }
        }
        true
    }

    pub(in crate::client::shell) fn route_add_remote_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
            return false;
        };
        let LocationDialogKind::Add(form) = &mut dialog.kind else {
            return false;
        };
        if !dialog.busy
            && matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            if let Some((field, selected)) = form.dropdown {
                let count = form.options(field).len();
                let next = if mouse.kind == MouseEventKind::ScrollUp {
                    selected.saturating_sub(1)
                } else {
                    (selected + 1).min(count.saturating_sub(1))
                };
                form.dropdown = Some((field, next));
                outcome.repaint = true;
            }
            return true;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return true;
        }
        outcome.repaint = true;
        let point = (mouse.column, mouse.row);
        if contains(self.hits.overlay_cancel, point) {
            self.close_location();
            return true;
        }
        if dialog.busy {
            return true;
        }
        if contains(self.hits.overlay_primary, point) {
            if form.dropdown.take().is_none() {
                self.accept_add_remote(outcome);
            }
            return true;
        }
        let hit = self
            .hits
            .settings_choices
            .iter()
            .rev()
            .find(|(r, _)| contains(*r, point))
            .map(|(_, i)| *i);
        match hit {
            Some(index) if index >= 1000 => {
                if let Some((field, _)) = form.dropdown {
                    self.select_add_option(field, index - 1000);
                }
            }
            Some(NAME_FIELD) if form.deletable().is_some() => self.confirm_delete_unbound(),
            Some(IMAGE_ACTION_FIELD) => self.confirm_delete_image(),
            Some(field) => {
                dialog.selected = field;
                form.dropdown = None;
                form.open_dropdown(field);
            }
            None => form.dropdown = None,
        }
        true
    }

    pub(super) fn tick_add_remote(&mut self, outcome: &mut ClientShellInput) {
        loop {
            let received = self
                .locations
                .add
                .discovery
                .as_ref()
                .and_then(|(epoch, receiver)| match receiver.try_recv() {
                    Ok(result) => Some((*epoch, Some(result))),
                    Err(mpsc::TryRecvError::Disconnected) => Some((*epoch, None)),
                    Err(mpsc::TryRecvError::Empty) => None,
                });
            let Some((epoch, event)) = received else {
                break;
            };
            let Some(event) = event else {
                self.locations.add.discovery = None;
                break;
            };
            if !matches!(event, Discovery::SignIn(_)) {
                self.locations.add.discovery = None;
                self.locations.add.sign_in_cancel = None;
            }
            if epoch != self.locations.epoch {
                continue;
            }
            let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() else {
                continue;
            };
            let LocationDialogKind::Add(form) = &mut dialog.kind else {
                continue;
            };
            outcome.repaint = true;
            match event {
                Discovery::Machines {
                    server,
                    result: Ok(list),
                    bound,
                    images,
                } => {
                    form.apply_machines(server, list, bound, images);
                    dialog.message = if form.machines.is_empty() {
                        format!("No machines yet. {CREATE_HINT}")
                    } else {
                        "Choose a machine, or create a new one.".into()
                    };
                }
                Discovery::Machines {
                    server,
                    result: Err(error),
                    ..
                } => {
                    form.loading = false;
                    form.server = server;
                    form.signed_in = !error.needs_sign_in();
                    dialog.message = if error.needs_sign_in() {
                        format!("{error} Press sign in to continue in your browser.")
                    } else {
                        format!("{error} Reopen Add remote to retry.")
                    };
                }
                Discovery::SignIn(step) => {
                    dialog.message = step.message();
                    form.sign_in = Some(step);
                }
                Discovery::SignInFailed(error) => {
                    form.loading = false;
                    form.sign_in = None;
                    dialog.message = error;
                }
            }
            break;
        }
        loop {
            let event = self
                .locations
                .add
                .job
                .as_ref()
                .and_then(|r| match r.try_recv() {
                    Ok(event) => Some(event),
                    Err(mpsc::TryRecvError::Disconnected) => Some(SetupEvent::Finished(Err(
                        "Setup worker exited. Check Settings → remotes before retrying.".into(),
                    ))),
                    Err(mpsc::TryRecvError::Empty) => None,
                });
            let Some(event) = event else {
                break;
            };
            let done = matches!(&event, SetupEvent::Finished(_));
            let message = match event {
                SetupEvent::Progress(message) => message,
                SetupEvent::Finished(result) => {
                    self.locations.add.job = None;
                    self.locations.add.running_form = None;
                    match result {
                        Ok(message) => message,
                        Err(error) => error,
                    }
                }
            };
            self.locations.add.status = message.clone();
            let visible = matches!(
                self.overlay,
                Some(ClientShellOverlay::Locations(LocationDialog {
                    kind: LocationDialogKind::Add(_),
                    ..
                }))
            );
            if visible {
                if done {
                    self.open_locations();
                }
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    dialog.message = message;
                    dialog.busy = !done;
                }
            } else if done {
                self.set_endpoint_error(message);
            }
            outcome.repaint = true;
            if done {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";

    fn listed(id: &str, name: &str, state: &str) -> Machine {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "state": state, "runtime": {"ready": state == "running"}
        }))
        .unwrap()
    }

    fn form() -> AddRemoteForm {
        let mut form = AddRemoteForm::default();
        form.apply_machines(
            "https://hangar.test".into(),
            vec![
                listed(ID, "box", "running"),
                listed("m_bbbbbbbbbbbbbbbbbbbbbbbbbb", "cold", "stopped"),
            ],
            Vec::new(),
            images(),
        );
        form
    }

    fn saved(id: &str, name: &str, source: &str, created_at: &str) -> Image {
        let mut value = crate::hangar::api::fake::image(id, name, source);
        value["createdAt"] = created_at.into();
        serde_json::from_value(value).unwrap()
    }

    /// Newest first: one saved from `box`, one whose source machine is gone.
    fn images() -> Vec<Image> {
        vec![
            saved("im_new", "agents", ID, "2026-10-03T09:00:00Z"),
            saved(
                "im_old",
                "base",
                "m_gonegonegonegonegonegonego",
                "2026-10-01T09:00:00Z",
            ),
        ]
    }

    fn image_source(id: &str, name: &str) -> MachineSource {
        MachineSource::Image {
            id: id.into(),
            name: name.into(),
        }
    }

    fn shell_with(form: AddRemoteForm) -> ClientShellState {
        let mut state = super::super::tests::shell();
        let mut dialog = super::super::tests::dialog();
        dialog.kind = LocationDialogKind::Add(Box::new(form));
        dialog.fields = vec![TextEditor::new("", false)];
        dialog.selected = 0;
        state.overlay = Some(ClientShellOverlay::Locations(dialog));
        state
    }

    fn key(state: &mut ClientShellState, code: KeyCode) {
        assert!(state.route_add_remote_key(
            &crate::input::TerminalKey::from(crossterm::event::KeyEvent::new(
                code,
                crossterm::event::KeyModifiers::NONE
            )),
            &mut ClientShellInput::default()
        ));
    }

    fn current_form(state: &ClientShellState) -> &AddRemoteForm {
        let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::Add(form),
            ..
        })) = &state.overlay
        else {
            panic!("add remote form");
        };
        form
    }

    fn machines_event(server: &str, result: Result<Vec<Machine>, HangarError>) -> Discovery {
        Discovery::Machines {
            server: server.into(),
            result,
            bound: Vec::new(),
            images: Vec::new(),
        }
    }

    #[test]
    fn source_lists_the_template_then_images_with_date_and_origin() {
        let mut form = form();
        let options = form.options(SOURCE_FIELD);
        assert_eq!(
            options,
            [
                TEMPLATE_SOURCE.to_owned(),
                "agents · 2026-10-03 · from box".to_owned(),
                "base · 2026-10-01 · from herdr@2026-10-03.2".to_owned(),
            ]
        );
        assert_eq!(form.label(SOURCE_FIELD), TEMPLATE_SOURCE);
        assert!(form.deletable_image().is_none());
        form.source = image_source("im_old", "base");
        assert_eq!(
            form.deletable_image().map(|image| image.id.as_str()),
            Some("im_old")
        );
        assert!(form.can_submit());
        // Only creating a machine has a source.
        form.choice = MachineChoice::Existing(ID.into());
        assert!(form.options(SOURCE_FIELD).is_empty());
        assert!(form.deletable_image().is_none());
    }

    #[test]
    fn keyboard_source_dropdown_selects_an_image() {
        let mut state = shell_with(form());
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.selected = SOURCE_FIELD;
        }
        key(&mut state, KeyCode::Enter);
        assert_eq!(current_form(&state).dropdown, Some((SOURCE_FIELD, 0)));
        key(&mut state, KeyCode::Down);
        key(&mut state, KeyCode::Enter);
        assert_eq!(
            current_form(&state).source,
            image_source("im_new", "agents")
        );
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        assert!(dialog.message.contains("from image agents"));
        assert!(dialog.message.contains("new, empty /data"));
        state.compose(110, 35).unwrap();
        assert!(state
            .hits
            .settings_choices
            .iter()
            .any(|(_, index)| *index == IMAGE_ACTION_FIELD));
        assert!(!state.locations.add.running());
    }

    #[test]
    fn late_image_list_keeps_the_chosen_source_while_it_is_listed() {
        let mut state = shell_with(form());
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            if let LocationDialogKind::Add(form) = &mut dialog.kind {
                form.source = image_source("im_old", "base");
            }
        }
        // A result from an earlier dialog is dropped.
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch.wrapping_sub(1), receive));
        send.send(machines_event("https://hangar.test", Ok(Vec::new())))
            .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert_eq!(current_form(&state).images.len(), 2);
        assert_eq!(current_form(&state).source, image_source("im_old", "base"));
        // A current refresh that still lists the image keeps it.
        let refresh = |images: Vec<Image>| Discovery::Machines {
            server: "https://hangar.test".into(),
            result: Ok(vec![listed(ID, "box", "running")]),
            bound: Vec::new(),
            images,
        };
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch, receive));
        send.send(refresh(images())).unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert_eq!(current_form(&state).source, image_source("im_old", "base"));
        // Once the image is gone the source falls back to the template.
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch, receive));
        send.send(refresh(images()[..1].to_vec())).unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert_eq!(current_form(&state).source, MachineSource::Template);
    }

    #[test]
    fn delete_image_asks_first_and_says_machines_are_not_affected() {
        let mut form = form();
        form.source = image_source("im_new", "agents");
        let mut state = shell_with(form);
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.selected = IMAGE_ACTION_FIELD;
        }
        key(&mut state, KeyCode::Enter);
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        let LocationDialogKind::DeleteImage(request) = &dialog.kind else {
            panic!("delete image confirmation");
        };
        assert_eq!(request.id, "im_new");
        assert_eq!(request.server, "https://hangar.test");
        assert!(dialog.message.contains("Delete image 'agents'?"));
        assert!(dialog
            .message
            .contains("Machines already created from it are not affected"));
        assert!(state.locations.job.is_none(), "nothing runs before Enter");
        state.compose(110, 35).unwrap();
    }

    #[test]
    fn stopped_machines_are_marked_and_added_ones_cannot_be_submitted() {
        let mut form = form();
        let options = form.options(1);
        assert_eq!(options[0], "＋ Create new machine");
        assert_eq!(options[1], "box");
        assert_eq!(options[2], "cold · stopped");
        form.bound = vec![ID.into()];
        form.choice = MachineChoice::Existing(ID.into());
        assert!(form.options(1)[1].ends_with("(added)"));
        assert!(!form.can_submit());
        form.choice = MachineChoice::New;
        assert!(form.can_submit());
        assert_eq!(form.primary_label(), " create and connect ");
        let signed_out = AddRemoteForm {
            loading: false,
            ..AddRemoteForm::default()
        };
        assert_eq!(signed_out.primary_label(), " sign in ");
        assert!(signed_out.options(1).is_empty());
    }

    #[test]
    fn an_unbound_machine_can_be_deleted_after_confirmation() {
        let mut state = shell_with(form());
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            if let LocationDialogKind::Add(form) = &mut dialog.kind {
                form.choice = MachineChoice::Existing(ID.into());
                form.bound = vec![ID.into()];
                assert!(
                    form.deletable().is_none(),
                    "added machines are removed from remotes"
                );
                form.bound.clear();
            }
            dialog.selected = NAME_FIELD;
        }
        key(&mut state, KeyCode::Enter);
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        let LocationDialogKind::Delete(request) = &dialog.kind else {
            panic!("delete confirmation");
        };
        assert_eq!(request.binding.machine_id, ID);
        assert!(request.remote.is_none());
        assert!(dialog.message.contains("'box'"));
        assert!(dialog.message.contains("permanently deleted"));
        assert!(!state.locations.add.running());
        assert!(
            state.locations.job.is_none(),
            "nothing runs before confirmation"
        );
    }

    #[test]
    fn keyboard_dropdown_selects_a_machine_and_escape_closes_dropdown_first() {
        let mut state = shell_with(form());
        key(&mut state, KeyCode::Down);
        key(&mut state, KeyCode::Enter);
        key(&mut state, KeyCode::Down);
        key(&mut state, KeyCode::Enter);
        assert_eq!(
            current_form(&state).choice,
            MachineChoice::Existing(ID.into())
        );
        assert!(!state.locations.add.running());
        key(&mut state, KeyCode::Enter);
        key(&mut state, KeyCode::Esc);
        assert!(state.overlay.is_some());
        key(&mut state, KeyCode::Esc);
        assert!(state.overlay.is_none());
    }

    #[test]
    fn name_field_takes_typing_only_when_creating() {
        let mut state = shell_with(form());
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            dialog.selected = NAME_FIELD;
        }
        key(&mut state, KeyCode::Char('a'));
        assert!(state.insert_overlay_text("bc"));
        let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() else {
            panic!("dialog");
        };
        assert_eq!(dialog.fields[0].as_str(), "abc");
        if let LocationDialogKind::Add(form) = &mut dialog.kind {
            form.choice = MachineChoice::Existing(ID.into());
        }
        assert!(dialog.editor_mut().is_none());
    }

    #[test]
    fn empty_name_is_not_submitted() {
        let mut state = shell_with(form());
        state.accept_add_remote(&mut ClientShellInput::default());
        assert!(!state.locations.add.running());
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        assert_eq!(dialog.selected, NAME_FIELD);
        assert!(dialog.message.contains("name"));
    }

    #[test]
    fn mouse_dropdown_and_keyboard_share_the_same_choice_state() {
        let mut state = shell_with(form());
        state.compose(110, 35).unwrap();
        let rect = state
            .hits
            .settings_choices
            .iter()
            .find(|(_, i)| *i == 1)
            .unwrap()
            .0;
        assert!(state.route_add_remote_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE
            },
            &mut ClientShellInput::default()
        ));
        key(&mut state, KeyCode::Down);
        assert_eq!(current_form(&state).dropdown, Some((1, 1)));
        state.compose(110, 35).unwrap();
        assert!(state.hits.settings_choices.iter().any(|(_, i)| *i == 1001));
        assert!(!state.locations.add.running());
    }

    #[test]
    fn stale_machine_list_does_not_override_a_newer_dialog_or_selection() {
        let mut state = shell_with(form());
        if let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() {
            if let LocationDialogKind::Add(form) = &mut dialog.kind {
                form.choice = MachineChoice::Existing(ID.into());
            }
        }
        // A result from an earlier dialog is dropped.
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch.wrapping_sub(1), receive));
        send.send(machines_event("https://old.test", Ok(Vec::new())))
            .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert_eq!(current_form(&state).server, "https://hangar.test");
        assert_eq!(current_form(&state).machines.len(), 2);
        // A current refresh keeps the selected machine while it is still listed.
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch, receive));
        send.send(machines_event(
            "https://hangar.test",
            Ok(vec![listed(ID, "box", "running")]),
        ))
        .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert_eq!(
            current_form(&state).choice,
            MachineChoice::Existing(ID.into())
        );
    }

    #[test]
    fn signed_out_listing_offers_sign_in_and_shows_each_sign_in_step() {
        let mut state = shell_with(AddRemoteForm::default());
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch, receive));
        send.send(machines_event(
            "https://hangar.test",
            Err(HangarError::NotSignedIn),
        ))
        .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert!(!current_form(&state).signed_in);
        assert_eq!(current_form(&state).primary_label(), " sign in ");
        let cancel = Arc::new(AtomicBool::new(false));
        state.locations.add.sign_in_cancel = Some(cancel.clone());
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch, receive));
        send.send(Discovery::SignIn(SignInStep::Device {
            device: crate::hangar::api::DeviceStart {
                device_code: "dc".into(),
                user_code: "ABCD-EFGH".into(),
                verification_uri: "https://github.com/login/device".into(),
                interval: 5,
                expires_in: 900,
            },
            reason: None,
        }))
        .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        assert!(dialog.message.contains("ABCD-EFGH"));
        // The sign-in channel stays open for the result that follows the code.
        assert!(state.locations.add.discovery.is_some());
        send.send(Discovery::SignIn(SignInStep::Browser {
            url: "https://hangar.test/auth/cli/start?state=s".into(),
        }))
        .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
            panic!("dialog");
        };
        assert!(dialog.message.starts_with("Continue in your browser"));
        assert!(dialog
            .message
            .contains("https://hangar.test/auth/cli/start"));
        assert!(state.locations.add.discovery.is_some());
        state.close_location();
        assert!(cancel.load(Ordering::Acquire), "closing cancels polling");
    }

    #[test]
    fn progress_does_not_release_the_worker_or_allow_duplicate_submission() {
        let mut state = shell_with(form());
        let (send, receive) = mpsc::channel();
        state.locations.add.job = Some(receive);
        state.locations.add.running_form = Some(form());
        send.send(SetupEvent::Progress("Creating…".into())).unwrap();
        send.send(SetupEvent::Progress("Waiting…".into())).unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert!(state.locations.add.running());
        state.accept_add_remote(&mut ClientShellInput::default());
        assert_eq!(state.locations.add.status, "Waiting…");
        state.close_location();
        send.send(SetupEvent::Finished(Err("interrupted".into())))
            .unwrap();
        let mut outcome = ClientShellInput::default();
        state.tick_add_remote(&mut outcome);
        assert!(!state.locations.add.running());
        assert!(state.overlay.is_none());
        assert!(outcome.actions.is_empty());
    }

    #[test]
    fn setup_does_not_disable_new_workspace_on_other_locations() {
        let mut state = shell_with(form());
        let (_send, receive) = mpsc::channel();
        state.locations.add.job = Some(receive);
        state.close_location();
        state.open_location_workspace();
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::Locations(LocationDialog {
                kind: LocationDialogKind::New,
                ..
            }))
        ));
    }
}
