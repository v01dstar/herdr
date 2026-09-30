//! Instacloud discovery and setup UI. Worker events are separate from pane state.
use super::*;
use crate::client::locations::instacloud::{inventory, provisioning};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

#[derive(Clone, Debug)]
pub(in crate::client::shell) struct AddRemoteForm {
    projects: Vec<inventory::Project>,
    project_id: Option<String>,
    computes: Option<inventory::Computes>,
    pending: Vec<provisioning::Operation>,
    pub(in crate::client::shell) choice: provisioning::Selection,
    pub(in crate::client::shell) dropdown: Option<(usize, usize)>,
    loading: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> AddRemoteForm {
        AddRemoteForm {
            projects: vec![inventory::Project {
                id: "p".into(),
                name: "herdr-remote".into(),
                org_id: "o".into(),
                org_name: "Org".into(),
            }],
            project_id: Some("p".into()),
            computes: Some(inventory::Computes {
                project: "p".into(),
                branch: "main".into(),
                items: Vec::new(),
            }),
            loading: false,
            ..AddRemoteForm::default()
        }
    }

    fn shell() -> ClientShellState {
        let mut state = super::super::tests::shell();
        let mut dialog = super::super::tests::dialog();
        dialog.kind = LocationDialogKind::Add(Box::new(form()));
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

    #[test]
    fn dropdown_enter_selects_only_and_escape_closes_dropdown_before_dialog() {
        let mut state = shell();
        key(&mut state, KeyCode::Enter);
        key(&mut state, KeyCode::Enter);
        assert!(!state.locations.add.running());
        key(&mut state, KeyCode::Enter);
        key(&mut state, KeyCode::Esc);
        assert!(state.overlay.is_some());
        key(&mut state, KeyCode::Esc);
        assert!(state.overlay.is_none());
    }

    #[test]
    fn mouse_dropdown_and_keyboard_share_the_same_choice_state() {
        let mut state = shell();
        state.compose(110, 35).unwrap();
        let rect = state
            .hits
            .settings_choices
            .iter()
            .find(|(_, i)| *i == 0)
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
        let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::Add(form),
            ..
        })) = &state.overlay
        else {
            panic!("form");
        };
        assert_eq!(form.dropdown, Some((0, 1)));
        assert!(!state.locations.add.running());
    }

    #[test]
    fn late_inventory_cannot_replace_the_newly_selected_project() {
        let mut state = shell();
        let (send, receive) = mpsc::channel();
        state.locations.add.discovery = Some((state.locations.epoch, receive));
        send.send(Ok(Discovery::Computes(
            inventory::Computes {
                project: "old".into(),
                branch: "old-branch".into(),
                items: Vec::new(),
            },
            Vec::new(),
        )))
        .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::Add(form),
            ..
        })) = &state.overlay
        else {
            panic!("form");
        };
        assert_eq!(form.computes.as_ref().unwrap().project, "p");
        assert_eq!(form.computes.as_ref().unwrap().branch, "main");
    }

    #[test]
    fn reopening_dropdown_keeps_the_current_project_and_compute() {
        let mut form = form();
        let mut other = form.projects[0].clone();
        other.id = "other".into();
        form.projects.insert(0, other);
        form.open_dropdown(1);
        assert_eq!(form.dropdown, Some((1, 1)));
        form.computes
            .as_mut()
            .unwrap()
            .items
            .push(inventory::Compute {
                id: "service".into(),
                name: "existing".into(),
                kind: "compute".into(),
                status: "running".into(),
            });
        form.choice = provisioning::Selection::Existing {
            id: "service".into(),
            name: "existing".into(),
        };
        form.open_dropdown(2);
        assert_eq!(form.dropdown, Some((2, 1)));
    }

    #[test]
    fn mouse_scroll_reaches_choices_beyond_the_visible_dropdown() {
        let mut state = shell();
        if let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::Add(form),
            ..
        })) = state.overlay.as_mut()
        {
            let project = form.projects[0].clone();
            for index in 1..12 {
                let mut p = project.clone();
                p.id = index.to_string();
                form.projects.push(p);
            }
            form.open_dropdown(1);
        }
        for _ in 0..11 {
            state.route_add_remote_mouse(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 40,
                    row: 12,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &mut ClientShellInput::default(),
            );
        }
        let Some(ClientShellOverlay::Locations(LocationDialog {
            kind: LocationDialogKind::Add(form),
            ..
        })) = &state.overlay
        else {
            panic!("form");
        };
        assert_eq!(form.dropdown, Some((1, 11)));
        state.compose(110, 35).unwrap();
        assert!(state.hits.settings_choices.iter().any(|(_, i)| *i == 1011));
    }

    #[test]
    fn cloud_setup_does_not_disable_new_workspace_on_other_locations() {
        let mut state = shell();
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

    #[test]
    fn progress_does_not_release_the_worker_or_allow_duplicate_submission() {
        let mut state = shell();
        let (send, receive) = mpsc::channel();
        state.locations.add.job = Some(receive);
        state.locations.add.running_form = Some(form());
        send.send(SetupEvent::Progress("Creating…".into())).unwrap();
        send.send(SetupEvent::Progress("Deploying…".into()))
            .unwrap();
        state.tick_add_remote(&mut ClientShellInput::default());
        assert!(state.locations.add.running());
        state.accept_add_remote(&mut ClientShellInput::default());
        assert_eq!(state.locations.add.status, "Deploying…");
        state.close_location();
        state.open_locations();
        assert!(
            matches!(&state.overlay, Some(ClientShellOverlay::Locations(LocationDialog { kind: LocationDialogKind::Add(_), busy: true, message, .. })) if message == "Deploying…")
        );
        state.close_location();
        send.send(SetupEvent::Finished(Err("interrupted".into())))
            .unwrap();
        let mut outcome = ClientShellInput::default();
        state.tick_add_remote(&mut outcome);
        assert!(!state.locations.add.running());
        assert!(state.overlay.is_none());
        assert!(outcome.actions.is_empty());
    }
}

impl Default for AddRemoteForm {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            project_id: None,
            computes: None,
            pending: Vec::new(),
            choice: provisioning::Selection::New,
            dropdown: None,
            loading: true,
        }
    }
}

impl AddRemoteForm {
    fn project(&self) -> Option<&inventory::Project> {
        self.projects
            .iter()
            .find(|p| Some(&p.id) == self.project_id.as_ref())
    }
    fn selections(&self) -> Vec<(String, provisioning::Selection)> {
        let mut choices = vec![("＋ Create new compute".into(), provisioning::Selection::New)];
        for op in &self.pending {
            choices.push((
                format!("Resume {} (setup incomplete)", op.target.service),
                provisioning::Selection::Resume(op.id().clone()),
            ));
        }
        if let Some(computes) = &self.computes {
            for c in &computes.items {
                if self
                    .pending
                    .iter()
                    .any(|p| p.target.service_id.as_ref() == Some(&c.id))
                {
                    continue;
                }
                choices.push((
                    format!("{} · {} (connect only)", c.name, c.status),
                    provisioning::Selection::Existing {
                        id: c.id.clone(),
                        name: c.name.clone(),
                    },
                ));
            }
        }
        choices
    }
    pub(in crate::client::shell) fn options(&self, field: usize) -> Vec<String> {
        match field {
            0 => vec!["Instacloud".into(), "SSH".into()],
            1 => self
                .projects
                .iter()
                .map(|p| format!("{} · {}", p.name, p.org_name))
                .collect(),
            2 => self
                .selections()
                .into_iter()
                .map(|(label, _)| label)
                .collect(),
            _ => Vec::new(),
        }
    }
    pub(in crate::client::shell) fn label(&self, field: usize) -> String {
        match field {
            0 => "Instacloud".into(),
            1 => self
                .project()
                .map(|p| format!("{} · {}", p.name, p.org_name))
                .unwrap_or_else(|| {
                    if self.loading {
                        "Loading…".into()
                    } else {
                        "Choose a project".into()
                    }
                }),
            2 => match &self.choice {
                provisioning::Selection::New => "＋ Create new compute".into(),
                provisioning::Selection::Existing { name, .. } => name.clone(),
                provisioning::Selection::Resume(id) => self
                    .pending
                    .iter()
                    .find(|p| p.id() == id)
                    .map(|p| format!("Resume {}", p.target.service))
                    .unwrap_or_else(|| "Setup no longer available".into()),
            },
            _ => String::new(),
        }
    }
    pub(in crate::client::shell) fn can_submit(&self) -> bool {
        !self.loading
            && self.project().is_some()
            && self
                .computes
                .as_ref()
                .is_some_and(|c| Some(&c.project) == self.project_id.as_ref())
    }
    fn open_dropdown(&mut self, field: usize) {
        if field < 3 && (field == 0 || !self.loading) && !self.options(field).is_empty() {
            let selected = match field {
                1 => self
                    .projects
                    .iter()
                    .position(|p| Some(&p.id) == self.project_id.as_ref())
                    .unwrap_or(0),
                2 => self
                    .selections()
                    .iter()
                    .position(|(_, value)| value == &self.choice)
                    .unwrap_or(0),
                _ => 0,
            };
            self.dropdown = Some((field, selected));
        }
    }
}

enum Discovery {
    Projects(inventory::Projects),
    Computes(inventory::Computes, Vec<provisioning::Operation>),
}
enum SetupEvent {
    Progress(String),
    Finished(Result<SavedSshEndpoint, String>),
}

#[derive(Default)]
pub(super) struct AddRemoteController {
    discovery: Option<(u64, mpsc::Receiver<Result<Discovery, String>>)>,
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
}

impl ClientShellState {
    pub(super) fn open_add_remote(&mut self) {
        self.locations.epoch = self.locations.epoch.wrapping_add(1);
        let form = self.locations.add.running_form.clone().unwrap_or_default();
        match self.location_dialog(LocationDialogKind::Add(Box::new(form))) {
            Ok(mut dialog) => {
                dialog.busy = self.locations.add.running();
                dialog.message = if dialog.busy {
                    self.locations.add.status.clone()
                } else {
                    "Loading your Instacloud projects…".into()
                };
                self.overlay = Some(ClientShellOverlay::Locations(dialog));
                if !self.locations.add.running() {
                    self.discover_cloud(None);
                }
            }
            Err(e) => self.set_endpoint_error(e),
        }
    }

    fn discover_cloud(&mut self, project: Option<String>) {
        let (send, receive) = mpsc::channel();
        self.locations.add.discovery = Some((self.locations.epoch, receive));
        std::thread::spawn(move || {
            let result = match project {
                Some(id) => inventory::computes(&id)
                    .and_then(|c| provisioning::pending(&id).map(|p| Discovery::Computes(c, p))),
                None => inventory::projects().map(Discovery::Projects),
            };
            let _ = send.send(result);
        });
    }

    fn select_cloud_option(&mut self, field: usize, index: usize) {
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
            0 if index == 1 => self.edit_location(None),
            1 => {
                if let Some(project) = form.projects.get(index) {
                    let id = project.id.clone();
                    form.project_id = Some(id.clone());
                    form.computes = None;
                    form.pending.clear();
                    form.choice = provisioning::Selection::New;
                    form.loading = true;
                    dialog.message = "Loading computes…".into();
                    self.discover_cloud(Some(id));
                }
            }
            2 => {
                if let Some((_, choice)) = form.selections().get(index) {
                    form.choice = choice.clone();
                    dialog.message = match &form.choice {
                        provisioning::Selection::New => "Creates an always-on compute with a 10 GiB persistent disk. Instacloud usage is billed to the selected project. Closing Herdr keeps it running.".into(),
                        provisioning::Selection::Existing { .. } => "Connects to an existing Herdr environment. Its image and files will be kept.".into(),
                        provisioning::Selection::Resume(id) => form.pending.iter().find(|p| p.id() == id)
                            .and_then(|p| p.error.clone()).unwrap_or_else(|| "Continue setting up the same compute.".into()),
                    };
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
            self.select_cloud_option(field, index);
            return;
        }
        if !form.can_submit() {
            dialog.message = "Select an available project and wait for its computes to load. Reopen Add remote to refresh after a CLI error.".into();
            return;
        }
        let Some(project) = form.project().cloned() else {
            return;
        };
        let Some(computes) = &form.computes else {
            return;
        };
        let branch = computes.branch.clone();
        let choice = form.choice.clone();
        self.locations.add.running_form = Some((**form).clone());
        let (send, receive) = mpsc::channel();
        self.locations.add.job = Some(receive);
        self.locations.add.status = "Preparing your remote…".into();
        dialog.message = self.locations.add.status.clone();
        dialog.busy = true;
        std::thread::spawn(move || {
            let result = provisioning::provision(&project, &branch, choice, |message| {
                let _ = send.send(SetupEvent::Progress(message));
            });
            let _ = send.send(SetupEvent::Finished(result));
        });
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
                KeyCode::Enter => self.select_cloud_option(field, selected),
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => dialog.selected = (dialog.selected + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => dialog.selected = (dialog.selected + 3) % 4,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') if dialog.selected < 3 => {
                form.open_dropdown(dialog.selected)
            }
            KeyCode::Enter => self.accept_add_remote(outcome),
            _ => {}
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
                    self.select_cloud_option(field, index - 1000);
                }
            }
            Some(field) => {
                dialog.selected = field;
                form.open_dropdown(field);
            }
            None => form.dropdown = None,
        }
        true
    }

    pub(super) fn tick_add_remote(&mut self, outcome: &mut ClientShellInput) {
        let discovered = self
            .locations
            .add
            .discovery
            .as_ref()
            .and_then(|(epoch, receiver)| match receiver.try_recv() {
                Ok(result) => Some((*epoch, result)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some((*epoch, Err("Instacloud discovery worker exited".into())))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some((epoch, result)) = discovered {
            self.locations.add.discovery = None;
            let mut load = None;
            if epoch == self.locations.epoch {
                if let Some(ClientShellOverlay::Locations(dialog)) = self.overlay.as_mut() {
                    if let LocationDialogKind::Add(form) = &mut dialog.kind {
                        match result {
                            Ok(Discovery::Projects(projects)) => {
                                form.projects = projects.items;
                                form.project_id = projects.preferred;
                                form.loading = false;
                                load = form.project_id.clone();
                                dialog.message = if form.projects.is_empty() {
                                    "No Instacloud projects found. Create a project in Instacloud, then reopen this dialog.".into()
                                } else {
                                    "Select a project to continue.".into()
                                };
                                if load.is_some() {
                                    form.loading = true;
                                    dialog.message = "Loading computes…".into();
                                }
                            }
                            Ok(Discovery::Computes(computes, pending))
                                if Some(&computes.project) == form.project_id.as_ref() =>
                            {
                                form.computes = Some(computes);
                                form.pending = pending;
                                form.loading = false;
                                dialog.message = "Creates an always-on compute with a 10 GiB persistent disk. Instacloud usage is billed to the selected project. Closing Herdr keeps it running.".into();
                            }
                            Ok(_) => {}
                            Err(error) => {
                                form.loading = false;
                                dialog.message = error;
                            }
                        }
                        outcome.repaint = true;
                    }
                }
            }
            if let Some(project) = load {
                self.discover_cloud(Some(project));
            }
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
                        "Setup worker exited. Reopen Add remote to resume.".into(),
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
                        Ok(profile) => format!(
                            "{} is ready. Select it in the sidebar or New → Location.",
                            profile.label
                        ),
                        Err(error) => {
                            format!("{error} Reopen Add remote and choose Resume to continue.")
                        }
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
