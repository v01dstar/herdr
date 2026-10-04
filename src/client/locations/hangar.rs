//! hangar machine actions for the remotes UI and CLI. These block on HTTP and SSH and
//! run only on worker threads. Accepting a mutation is not readiness: herdr polls the
//! operation and the machine, and never repeats an accepted mutation. Nothing here
//! starts a machine implicitly; only explicit Start remote and Create do.
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::hangar::api::{
    Client, CreateImageRequest, CreateMachineRequest, CurlHttp, ErrorCode, ForkMachineRequest,
    HangarError, Image, Machine, MachineSpec, MachineState, Operation, OperationState, Template,
    TemplateCapability, Usage,
};
use crate::hangar::auth::{system_clock, AccountStatus, CredentialStore, SignOut};
use crate::hangar::binding::HangarBinding;
use crate::hangar::login::{browser_unavailable, Browser, SignInStep};

/// The template that ships Herdr; sizes come from its defaults.
const TEMPLATE: &str = "herdr";
/// Session name on hangar machines (the template's Herdr server session).
pub(crate) const SESSION: &str = "herdr-remote";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const READY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(crate) type Progress<'a> = &'a mut dyn FnMut(String);

fn operation_error(operation: &Operation) -> HangarError {
    match &operation.error {
        Some(error) if !error.code.is_empty() => HangarError::Api(crate::hangar::api::ApiError {
            status: 0,
            code: ErrorCode::parse(&error.code),
            message: error.message.clone(),
            operation_id: Some(operation.id.clone()),
        }),
        _ => HangarError::Invalid(format!(
            "{} operation {} failed",
            operation.kind, operation.id
        )),
    }
}

/// Polls `operation` until it finishes. Never re-sends the mutation that created it.
pub(crate) fn wait_operation(
    client: &Client,
    mut operation: Operation,
    progress: Progress<'_>,
) -> Result<Operation, HangarError> {
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    loop {
        match operation.state {
            OperationState::Succeeded => return Ok(operation),
            OperationState::Failed => return Err(operation_error(&operation)),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(HangarError::Invalid(format!(
                "{} was accepted but has not finished; check Machine status later",
                operation.kind
            )));
        }
        if let Some(phase) = operation.phase.as_deref().filter(|phase| !phase.is_empty()) {
            progress(format!("hangar: {} {phase}…", operation.kind));
        }
        client.sleep(POLL_INTERVAL);
        operation = client.operation(&operation.id)?;
    }
}

/// Sends one mutation. On `operation_conflict` it waits for the reported operation and
/// then sends the mutation once more with a new key: the first one was rejected, not
/// accepted.
fn mutate(
    client: &Client,
    send: impl Fn(&str) -> Result<Operation, HangarError>,
    progress: Progress<'_>,
) -> Result<Operation, HangarError> {
    match send(&crate::hangar::new_idempotency_key()) {
        Ok(operation) => wait_operation(client, operation, progress),
        Err(HangarError::Api(error)) if error.code == ErrorCode::OperationConflict => {
            let Some(other) = error.operation_id.clone() else {
                return Err(HangarError::Api(error));
            };
            progress("Waiting for another operation on this machine…".into());
            let other = client.operation(&other)?;
            if let Err(error) = wait_operation(client, other, progress) {
                tracing::debug!(%error, "conflicting hangar operation did not succeed");
            }
            let operation = send(&crate::hangar::new_idempotency_key())?;
            wait_operation(client, operation, progress)
        }
        Err(error) => Err(error),
    }
}

/// Waits until the guest reports ready.
pub(crate) fn wait_ready(
    client: &Client,
    machine_id: &str,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let machine = client.machine(machine_id)?;
        match machine.state {
            MachineState::Running if machine.runtime.ready => return Ok(machine),
            MachineState::Stopped
            | MachineState::Suspended
            | MachineState::Error
            | MachineState::Deleted => {
                return Err(HangarError::MachineNotRunning {
                    machine: machine.name,
                    state: machine.state.as_str().to_owned(),
                })
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(HangarError::Invalid(format!(
                "{} is {} but not ready yet; check Machine status later",
                machine.name,
                machine.state.as_str()
            )));
        }
        progress(format!("Waiting for {} to be ready…", machine.name));
        client.sleep(POLL_INTERVAL);
    }
}

pub(crate) fn start_with(
    client: &Client,
    machine_id: &str,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    progress("Starting the hangar machine…".into());
    mutate(
        client,
        |key| client.start_machine(key, machine_id),
        progress,
    )?;
    wait_ready(client, machine_id, progress)
}

pub(crate) fn stop_with(
    client: &Client,
    machine_id: &str,
    progress: Progress<'_>,
) -> Result<(), HangarError> {
    progress("Stopping the hangar machine…".into());
    mutate(client, |key| client.stop_machine(key, machine_id), progress).map(|_| ())
}

/// Keeps memory (running programs) in a snapshot; Start remote resumes it.
pub(crate) fn suspend_with(
    client: &Client,
    machine_id: &str,
    progress: Progress<'_>,
) -> Result<(), HangarError> {
    progress("Suspending the hangar machine…".into());
    mutate(
        client,
        |key| client.suspend_machine(key, machine_id),
        progress,
    )
    .map(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deletion {
    Deleted,
    /// hangar no longer has the machine (deleted elsewhere or never visible).
    AlreadyGone,
}

/// Deletes the machine, its disks and snapshots, and waits for the operation. A
/// `not_found` anywhere counts as already deleted only when the machine itself is
/// confirmed missing, so a vanished operation record is not mistaken for success.
pub(crate) fn delete_with(
    client: &Client,
    machine_id: &str,
    progress: Progress<'_>,
) -> Result<Deletion, HangarError> {
    progress("Deleting the hangar machine…".into());
    match mutate(
        client,
        |key| client.delete_machine(key, machine_id),
        progress,
    ) {
        Ok(_) => Ok(Deletion::Deleted),
        Err(error) if error.code() == Some(&ErrorCode::NotFound) => {
            match client.machine(machine_id) {
                Err(missing) if missing.code() == Some(&ErrorCode::NotFound) => {
                    Ok(Deletion::AlreadyGone)
                }
                Ok(machine) if machine.state == MachineState::Deleted => Ok(Deletion::AlreadyGone),
                Ok(_) => Err(error),
                Err(other) => Err(other),
            }
        }
        Err(error) => Err(error),
    }
}

/// What a new machine starts from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum MachineSource {
    /// The latest version of the herdr template.
    #[default]
    Template,
    /// One of the caller's images: its root disk and a new, empty /data.
    Image { id: String, name: String },
}

pub(crate) fn create_with(
    client: &Client,
    name: &str,
    source: &MachineSource,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    let request = match source {
        MachineSource::Template => {
            progress(format!("Creating hangar machine {name}…"));
            CreateMachineRequest {
                name,
                template_id: Some(TEMPLATE),
                image_id: None,
            }
        }
        MachineSource::Image { id, name: image } => {
            progress(format!(
                "Creating hangar machine {name} from image {image}…"
            ));
            CreateMachineRequest {
                name,
                template_id: None,
                image_id: Some(id),
            }
        }
    };
    let operation = mutate(client, |key| client.create_machine(key, &request), progress).map_err(
        |error| match (source, error.code()) {
            (MachineSource::Image { name, .. }, Some(ErrorCode::NotFound)) => {
                HangarError::Invalid(format!("image {name} was deleted; choose another source"))
            }
            _ => error,
        },
    )?;
    if operation.machine_id.is_empty() {
        return Err(HangarError::Invalid("create returned no machine".into()));
    }
    wait_ready(client, &operation.machine_id, progress)
}

/// What Copy machine… → Save as image stores, shown before saving.
pub(crate) const IMAGE_CONTENTS: &str = "Saves the machine's root disk only: installed software and system settings. Repositories, home directory files and logins on /data are not included. Anything written to the root disk is included, such as credentials from `sudo gh auth` or tokens in /etc/environment. The machine is stopped first and stays stopped. The image appears on the images tab; New machine from image… starts machines from it. Images are private to your hangar account.";

/// How a machine becomes saveable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SavePlan {
    /// Stopped and uploaded: save directly.
    Save,
    /// Running (or not uploaded): stop it like Stop machine, then save.
    Stop,
    /// Suspended: its snapshot holds memory, so resume and stop it first.
    ResumeThenStop,
}

/// Whether the machine can be saved, and what the dialog says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SaveCheck {
    pub plan: Option<SavePlan>,
    pub note: String,
}

/// What a stopped machine's snapshot is used for: Copy machine… → Save as image or
/// Clone now (hangar's fork).
/// Both need the same stopped, uploaded machine and identity-reset template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotUse {
    Image,
    Fork,
}

impl SnapshotUse {
    /// "Only a stopped machine can be {passive}."
    fn passive(self) -> &'static str {
        match self {
            SnapshotUse::Image => "saved",
            SnapshotUse::Fork => "cloned",
        }
    }

    /// The primary button for a machine that must be stopped first.
    fn stop_action(self) -> &'static str {
        match self {
            SnapshotUse::Image => "Stop machine and save",
            SnapshotUse::Fork => "Stop machine and clone",
        }
    }

    /// What happens once the machine is stopped.
    fn then(self) -> &'static str {
        match self {
            SnapshotUse::Image => "saves the image",
            SnapshotUse::Fork => "clones it",
        }
    }

    fn too_old(self) -> &'static str {
        match self {
            SnapshotUse::Image => "save images from",
            SnapshotUse::Fork => "clone",
        }
    }

    fn redo(self) -> &'static str {
        match self {
            SnapshotUse::Image => "save that one",
            SnapshotUse::Fork => "clone that one",
        }
    }

    fn dialog(self) -> &'static str {
        "Copy machine…"
    }
}

/// Whether the machine can be used for `usage`. `templates` is `None` when the catalog
/// could not be read; the server then decides.
pub(crate) fn snapshot_check(
    machine: &Machine,
    templates: Option<&[Template]>,
    usage: SnapshotUse,
) -> SaveCheck {
    let name = &machine.name;
    if let (Some(templates), Some(template)) = (templates, machine.template.as_ref()) {
        let supported = templates.iter().any(|candidate| {
            candidate.id == template.id
                && candidate.version == template.version
                && candidate.has(TemplateCapability::IdentityReset)
        });
        if !supported {
            return SaveCheck {
                plan: None,
                note: format!(
                    "{name} was created from template {}, which is too old to {}. Create a new machine from the latest template (Add remote → Create new machine), set it up there, and {}.",
                    template.label(),
                    usage.too_old(),
                    usage.redo(),
                ),
            };
        }
    }
    let synced = machine
        .storage
        .as_ref()
        .is_none_or(|storage| storage.synced);
    let (passive, action, then) = (usage.passive(), usage.stop_action(), usage.then());
    let (plan, note) = match machine.state {
        MachineState::Stopped if synced => (Some(SavePlan::Save), String::new()),
        MachineState::Stopped => (
            Some(SavePlan::Stop),
            format!("{name} has changes that are not uploaded yet. {action} stops it again to upload them, then {then}."),
        ),
        MachineState::Running | MachineState::Error => (
            Some(SavePlan::Stop),
            format!("{name} is {}. Only a stopped machine can be {passive}. {action} stops all sessions and jobs on it (as Stop machine… does), waits until it is stopped, then {then}.", machine.state.as_str()),
        ),
        MachineState::Suspended => (
            Some(SavePlan::ResumeThenStop),
            format!("{name} is suspended. Only a stopped machine can be {passive}, and a suspended machine keeps its memory. {action} resumes it, stops all sessions and jobs on it, waits until it is stopped, then {then}."),
        ),
        state => (
            None,
            format!("{name} is {}. Wait until it is stopped or running, then open {} again.", state.as_str(), usage.dialog()),
        ),
    };
    SaveCheck { plan, note }
}

pub(crate) fn check_save_with(client: &Client, machine_id: &str) -> Result<SaveCheck, HangarError> {
    check_snapshot_with(client, machine_id, SnapshotUse::Image)
}

pub(crate) fn check_snapshot_with(
    client: &Client,
    machine_id: &str,
    usage: SnapshotUse,
) -> Result<SaveCheck, HangarError> {
    let machine = client.machine(machine_id)?;
    let templates = client
        .templates()
        .map_err(|error| tracing::debug!(%error, "could not read hangar templates"))
        .ok();
    Ok(snapshot_check(&machine, templates.as_deref(), usage))
}

fn valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
}

/// Image names follow hangar's rule; checked before any machine is stopped.
pub(crate) fn validate_image_name(name: &str) -> Result<(), String> {
    if valid_name(name) {
        Ok(())
    } else {
        Err("Image names use 1–63 lowercase letters, digits or '-', starting with a letter or digit.".into())
    }
}

/// Text for a failed image request: hangar's own explanation for refusals.
pub(crate) fn image_error(error: &HangarError) -> String {
    match error {
        HangarError::Api(api)
            if matches!(
                api.code,
                ErrorCode::OperationConflict | ErrorCode::BadRequest
            ) && !api.message.is_empty() =>
        {
            api.message.clone()
        }
        HangarError::Api(api) if api.code == ErrorCode::NotFound => {
            "the hangar machine or image no longer exists".into()
        }
        other => other.to_string(),
    }
}

/// Brings the machine to a stopped, uploaded state per `plan`. `quiesce` runs right
/// before the stop: it disables automatic connection and stops the Herdr sessions, as
/// Stop machine does.
fn prepare_source(
    client: &Client,
    machine_id: &str,
    plan: SavePlan,
    quiesce: &mut dyn FnMut() -> Result<(), String>,
    progress: Progress<'_>,
) -> Result<(), HangarError> {
    if plan == SavePlan::ResumeThenStop {
        start_with(client, machine_id, progress)?;
    }
    if plan != SavePlan::Save {
        quiesce().map_err(HangarError::Invalid)?;
        stop_with(client, machine_id, progress)?;
    }
    Ok(())
}

/// Brings the machine to a stopped, uploaded state per `plan`, then saves the image.
/// `quiesce` runs right before the stop: it disables automatic connection and stops
/// the Herdr sessions, as Stop machine does.
pub(crate) fn save_image_with(
    client: &Client,
    machine_id: &str,
    plan: SavePlan,
    request: &CreateImageRequest<'_>,
    quiesce: &mut dyn FnMut() -> Result<(), String>,
    progress: Progress<'_>,
) -> Result<Image, HangarError> {
    prepare_source(client, machine_id, plan, quiesce, progress)?;
    progress(format!("Saving image {}…", request.name));
    match client.create_image(&crate::hangar::new_idempotency_key(), machine_id, request) {
        Err(HangarError::Api(error)) if error.code == ErrorCode::OperationConflict => {
            // Another operation holds the machine: wait for it, then ask once more.
            let Some(other) = error.operation_id.clone() else {
                return Err(HangarError::Api(error));
            };
            progress("Waiting for another operation on this machine…".into());
            if let Err(error) = wait_operation(client, client.operation(&other)?, progress) {
                tracing::debug!(%error, "conflicting hangar operation did not succeed");
            }
            client.create_image(&crate::hangar::new_idempotency_key(), machine_id, request)
        }
        result => result,
    }
}

/// Deletes an image. Machines created from it keep their disks.
pub(crate) fn delete_image_with(client: &Client, id: &str) -> Result<Deletion, HangarError> {
    match client.delete_image(&crate::hangar::new_idempotency_key(), id) {
        Ok(()) => Ok(Deletion::Deleted),
        Err(error) if error.code() == Some(&ErrorCode::NotFound) => Ok(Deletion::AlreadyGone),
        Err(error) => Err(error),
    }
}

/// What Copy machine… → Clone now copies, shown before cloning.
pub(crate) const CLONE_CONTENTS: &str = "The clone is a new machine with a copy of this machine's root disk and its /data disk: installed software, system settings, repositories, your home directory, and signed-in credentials (gh, Claude, Codex, SSH keys). It gets its own SSH host keys, machine ID and hostname, starts as a new machine, and appears on the remotes tab. This machine is stopped first and stays stopped. Clones count toward your machine limit.";

/// The suggested clone name: `<source>-clone`, shortened to hangar's 63 bytes.
pub(crate) fn default_clone_name(source: &str) -> String {
    const SUFFIX: &str = "-clone";
    let mut base = source.to_owned();
    while base.len() + SUFFIX.len() > 63 {
        base.pop();
    }
    format!("{}{SUFFIX}", base.trim_end_matches('-'))
}

/// Clone names follow hangar's machine name rule; checked before anything is stopped.
pub(crate) fn validate_fork_name(name: &str) -> Result<(), String> {
    if valid_name(name) {
        Ok(())
    } else {
        Err("Machine names use 1–63 lowercase letters, digits or '-', starting with a letter or digit.".into())
    }
}

/// Text for a failed clone: hangar's own explanation for refusals and quota limits.
pub(crate) fn fork_error(error: &HangarError) -> String {
    match error {
        HangarError::Api(api)
            if matches!(
                api.code,
                ErrorCode::OperationConflict | ErrorCode::BadRequest | ErrorCode::QuotaExceeded
            ) && !api.message.is_empty() =>
        {
            api.message.clone()
        }
        HangarError::Api(api) if api.code == ErrorCode::NotFound => {
            "the hangar machine no longer exists".into()
        }
        other => other.to_string(),
    }
}

/// Brings the source to a stopped, uploaded state per `plan` (see [`save_image_with`]),
/// forks it, and waits until the fork is running and ready. The source stays stopped.
pub(crate) fn fork_with(
    client: &Client,
    source_id: &str,
    plan: SavePlan,
    name: &str,
    quiesce: &mut dyn FnMut() -> Result<(), String>,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    prepare_source(client, source_id, plan, quiesce, progress)?;
    progress(format!("Cloning into {name}…"));
    let request = ForkMachineRequest {
        name,
        desired_state: "running",
    };
    let operation = mutate(
        client,
        |key| client.fork_machine(key, source_id, &request),
        progress,
    )?;
    if operation.machine_id.is_empty() {
        return Err(HangarError::Invalid("clone returned no machine".into()));
    }
    wait_ready(client, &operation.machine_id, progress)
}

/// Newest first, as the source picker shows them.
pub(crate) fn sort_images(images: &mut [Image]) {
    images.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// What Settings → Remotes shows about a hangar machine besides its state; read from
/// the machine list on the settings sync worker, kept only in memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MachineDetails {
    /// `herdr@2026-10-03.2`, when the server reports it.
    pub template: Option<String>,
    pub spec: Option<MachineSpec>,
    /// The image it was created from.
    pub image_id: Option<String>,
    /// The machine it was forked from.
    pub forked_from: Option<String>,
    /// Whether its template allows images and forks; `None` when unknown.
    pub snapshots: Option<bool>,
    /// The message of the machine's last failed operation, if any.
    pub last_error: Option<String>,
}

impl MachineDetails {
    /// `templates` is `None` when the catalog could not be read.
    pub(crate) fn of(machine: &Machine, templates: Option<&[Template]>) -> Self {
        let snapshots = match (templates, machine.template.as_ref()) {
            (Some(templates), Some(template)) => Some(templates.iter().any(|candidate| {
                candidate.id == template.id
                    && candidate.version == template.version
                    && candidate.has(TemplateCapability::IdentityReset)
            })),
            _ => None,
        };
        Self {
            template: machine
                .template
                .as_ref()
                .filter(|template| !template.id.is_empty())
                .map(|template| template.label()),
            spec: machine.spec.clone(),
            image_id: machine
                .image
                .as_ref()
                .map(|image| image.id.clone())
                .filter(|id| !id.is_empty()),
            forked_from: machine
                .forked_from
                .as_ref()
                .map(|fork| fork.machine_id.clone())
                .filter(|id| !id.is_empty()),
            snapshots,
            last_error: machine
                .last_error
                .as_ref()
                .map(|error| error.message.clone())
                .filter(|message| !message.is_empty()),
        }
    }
}

fn hangar_client(server: &str) -> Result<Client, HangarError> {
    crate::hangar::auth::shared_client(server)
}

/// Opens `url` in the user's browser. A launcher that fails right away (such as
/// `xdg-open` without a browser) counts as no browser, so sign-in uses a code instead.
fn open_in_browser(url: &str) -> Result<(), String> {
    let mut child = match crate::platform::open_url(url) {
        Ok(Some(child)) => child,
        Ok(None) => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("the browser launcher exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                // Still running (some launchers wait for the browser): reap it later.
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Signs in to the server new remotes use (`HANGAR_SERVER`, the signed-in server, or
/// the default): browser sign-in when possible, else a device code. Stores the
/// sign-in shared with the hangar CLI.
pub(crate) fn sign_in(
    notify: &mut dyn FnMut(SignInStep),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), HangarError> {
    let server = crate::hangar::default_server();
    let store = CredentialStore::shared().ok_or_else(|| {
        HangarError::Invalid("cannot find a home directory for hangar credentials".into())
    })?;
    let client = Client::new(&server, Arc::new(CurlHttp), None);
    let browser = Browser {
        unavailable: browser_unavailable(
            |name| std::env::var(name).ok(),
            crate::platform::OPEN_URL_NEEDS_DISPLAY,
        ),
        open: &open_in_browser,
    };
    crate::hangar::login::sign_in(
        &client,
        &store,
        &browser,
        notify,
        cancelled,
        &system_clock(),
    )
}

/// Who is signed in, checked with hangar.
pub(crate) fn account_status() -> AccountStatus {
    let server = crate::hangar::default_server();
    match CredentialStore::shared() {
        Some(store) => {
            crate::hangar::auth::account_status(&store, Arc::new(CurlHttp), system_clock(), &server)
        }
        None => AccountStatus::SignedOut { server },
    }
}

/// Signs out of hangar (Herdr and the hangar CLI share the sign-in).
pub(crate) fn sign_out() -> Result<String, String> {
    let store =
        CredentialStore::shared().ok_or("Cannot find a home directory for hangar credentials")?;
    match crate::hangar::auth::sign_out(&store, Arc::new(CurlHttp)) {
        Ok(SignOut::NotSignedIn) => Ok("Not signed in to hangar.".into()),
        Ok(SignOut::SignedOut {
            server,
            warning: None,
        }) => Ok(format!(
            "Signed out of hangar on {server}. The hangar CLI is signed out too."
        )),
        Ok(SignOut::SignedOut {
            server,
            warning: Some(warning),
        }) => Ok(format!(
            "Signed out of hangar on {server} on this computer (the hangar CLI too), but the server did not confirm the revocation: {warning}"
        )),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn list_machines(server: &str) -> Result<Vec<Machine>, HangarError> {
    let mut machines = hangar_client(server)?.machines()?;
    machines.retain(|machine| machine.state != MachineState::Deleted);
    machines.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(machines)
}

pub(crate) fn start_machine(
    binding: &HangarBinding,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    start_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        progress,
    )
}

pub(crate) fn stop_machine(
    binding: &HangarBinding,
    progress: Progress<'_>,
) -> Result<(), HangarError> {
    stop_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        progress,
    )
}

pub(crate) fn suspend_machine(
    binding: &HangarBinding,
    progress: Progress<'_>,
) -> Result<(), HangarError> {
    suspend_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        progress,
    )
}

pub(crate) fn delete_machine(
    binding: &HangarBinding,
    progress: Progress<'_>,
) -> Result<Deletion, HangarError> {
    delete_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        progress,
    )
}

pub(crate) fn create_machine(
    server: &str,
    name: &str,
    source: &MachineSource,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    create_with(&hangar_client(server)?, name, source, progress)
}

pub(crate) fn list_images(server: &str) -> Result<Vec<Image>, HangarError> {
    let mut images = hangar_client(server)?.images()?;
    sort_images(&mut images);
    Ok(images)
}

// Unit tests replace the settings workers that call these with fakes.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn list_templates(server: &str) -> Result<Vec<Template>, HangarError> {
    hangar_client(server)?.templates()
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn usage(server: &str) -> Result<Usage, HangarError> {
    hangar_client(server)?.usage()
}

pub(crate) fn check_save(binding: &HangarBinding) -> Result<SaveCheck, HangarError> {
    check_save_with(&hangar_client(&binding.server)?, &binding.machine_id)
}

pub(crate) fn save_image(
    binding: &HangarBinding,
    plan: SavePlan,
    request: &CreateImageRequest<'_>,
    quiesce: &mut dyn FnMut() -> Result<(), String>,
    progress: Progress<'_>,
) -> Result<Image, HangarError> {
    save_image_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        plan,
        request,
        quiesce,
        progress,
    )
}

pub(crate) fn check_fork(binding: &HangarBinding) -> Result<SaveCheck, HangarError> {
    check_snapshot_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        SnapshotUse::Fork,
    )
}

pub(crate) fn fork_machine(
    binding: &HangarBinding,
    plan: SavePlan,
    name: &str,
    quiesce: &mut dyn FnMut() -> Result<(), String>,
    progress: Progress<'_>,
) -> Result<Machine, HangarError> {
    fork_with(
        &hangar_client(&binding.server)?,
        &binding.machine_id,
        plan,
        name,
        quiesce,
        progress,
    )
}

pub(crate) fn delete_image(server: &str, id: &str) -> Result<Deletion, HangarError> {
    delete_image_with(&hangar_client(server)?, id)
}

/// Resolves a machine by ID or unique name.
pub(crate) fn resolve_machine(server: &str, selector: &str) -> Result<Machine, String> {
    let machines = list_machines(server).map_err(|error| error.to_string())?;
    if let Some(machine) = machines.iter().find(|machine| machine.id == selector) {
        return Ok(machine.clone());
    }
    let mut named = machines.iter().filter(|machine| machine.name == selector);
    match (named.next(), named.next()) {
        (Some(machine), None) => Ok(machine.clone()),
        (Some(_), Some(_)) => Err(format!(
            "hangar machine name '{selector}' is ambiguous; use its ID"
        )),
        _ => Err(format!("no hangar machine named '{selector}'")),
    }
}

/// Lists a machine hangar just created or forked (before the next sync) and, when it is
/// running, starts its Herdr session so clients connect.
pub(crate) fn adopt_machine(
    server: &str,
    machine: &Machine,
    progress: Progress<'_>,
) -> Result<String, String> {
    super::sync::record_listed(server, machine)?;
    let remotes = super::Remotes::load()?;
    let remote = remotes
        .machine(&crate::hangar::normalize_server(server), &machine.id)
        .ok_or("hangar machine is not listed")?;
    let (profile, options) = (remote.profile.clone(), remote.options());
    if machine.state != MachineState::Running {
        return Ok(format!(
            "{} is listed. It is {}; use Start remote to start it.",
            profile.label,
            machine.state.as_str()
        ));
    }
    progress(format!("Connecting to {}…", machine.name));
    super::start_session(&profile, &options).map_err(|error| {
        format!(
            "{} is listed but not connected: {error} Use Start remote to retry.",
            profile.label
        )
    })?;
    Ok(format!(
        "{} is ready. Select it in the sidebar or New → Location.",
        profile.label
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hangar::api::fake::*;

    const ID: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";

    #[test]
    fn machine_details_name_the_template_origin_and_snapshot_support() {
        let mut value = machine(ID, "stopped", false);
        value["template"] = serde_json::json!({"id": "herdr", "version": "v1", "digest": "d"});
        value["forkedFrom"] = serde_json::json!({"machineId": "m_src", "snapshotSeq": 1});
        value["spec"] = serde_json::json!({"vcpus": 2, "memMiB": 2048, "persistentDiskGiB": 5});
        let listed: Machine = serde_json::from_value(value).unwrap();
        let catalog = |capabilities: &[&str]| -> Vec<Template> {
            vec![serde_json::from_value(template("v1", capabilities)).unwrap()]
        };
        let details = MachineDetails::of(&listed, Some(&catalog(&["identity-reset"])));
        assert_eq!(details.template.as_deref(), Some("herdr@v1"));
        assert_eq!(details.forked_from.as_deref(), Some("m_src"));
        assert_eq!(details.image_id, None);
        assert_eq!(details.spec.as_ref().map(|spec| spec.vcpus), Some(2));
        assert_eq!(details.snapshots, Some(true));
        assert_eq!(
            MachineDetails::of(&listed, Some(&catalog(&[]))).snapshots,
            Some(false)
        );
        assert_eq!(MachineDetails::of(&listed, None).snapshots, None);
    }

    #[test]
    fn accepted_start_is_polled_not_resent() {
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "start", "queued"))
            .reply(200, operation("op_1", "start", "running"))
            .reply(200, operation("op_1", "start", "succeeded"))
            .reply(200, machine(ID, "running", false))
            .reply(200, machine(ID, "running", true));
        let client = client(&http);
        let machine = start_with(&client, ID, &mut |_| {}).unwrap();
        assert!(machine.runtime.ready);
        assert_eq!(
            http.paths(),
            [
                format!("POST /v1/machines/{ID}/start"),
                "GET /v1/operations/op_1".to_owned(),
                "GET /v1/operations/op_1".to_owned(),
                format!("GET /v1/machines/{ID}"),
                format!("GET /v1/machines/{ID}"),
            ]
        );
    }

    #[test]
    fn status_failure_after_stop_does_not_resend_stop() {
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "stop", "running"))
            .push(Err(crate::hangar::api::TransportError::Failed(
                "reset".into(),
            )));
        let client = client(&http);
        assert!(stop_with(&client, ID, &mut |_| {}).is_err());
        assert_eq!(
            http.paths()
                .iter()
                .filter(|path| path.ends_with("/stop"))
                .count(),
            1
        );
    }

    #[test]
    fn operation_conflict_waits_for_the_given_operation_then_sends_once_more() {
        let http = FakeHttp::new();
        http.reply(
            409,
            serde_json::json!({"error": {"code": "operation_conflict", "message": "busy", "requestId": "r", "retryable": false, "operationId": "op_other"}}),
        )
        .reply(200, operation("op_other", "stop", "succeeded"))
        .reply(202, operation("op_2", "start", "succeeded"))
        .reply(200, machine(ID, "running", true));
        let client = client(&http);
        start_with(&client, ID, &mut |_| {}).unwrap();
        let sent = http.sent();
        assert_eq!(
            http.paths()[..3],
            [
                format!("POST /v1/machines/{ID}/start"),
                "GET /v1/operations/op_other".to_owned(),
                format!("POST /v1/machines/{ID}/start"),
            ]
        );
        assert_ne!(sent[0].idempotency_key, sent[2].idempotency_key);
    }

    #[test]
    fn failed_operation_reports_its_error() {
        let http = FakeHttp::new();
        let mut failed = operation("op_1", "create", "failed");
        failed["error"] = serde_json::json!({"code": "no_capacity", "message": "full", "requestId": "r", "retryable": true, "operationId": "op_1"});
        http.reply(202, failed);
        let error =
            create_with(&client(&http), "box", &MachineSource::Template, &mut |_| {}).unwrap_err();
        assert_eq!(error.code(), Some(&ErrorCode::NoCapacity));
        let body: serde_json::Value =
            serde_json::from_str(http.sent()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name": "box", "templateId": "herdr"})
        );
    }

    #[test]
    fn create_waits_for_the_new_machine_to_be_ready() {
        let http = FakeHttp::new();
        let mut created = operation("op_1", "create", "succeeded");
        created["machineId"] = ID.into();
        http.reply(202, created)
            .reply(200, machine(ID, "starting", false))
            .reply(200, machine(ID, "running", true));
        let mut steps = Vec::new();
        let machine = create_with(
            &client(&http),
            "box",
            &MachineSource::Template,
            &mut |step| steps.push(step),
        )
        .unwrap();
        assert_eq!(machine.id, ID);
        assert!(steps.iter().any(|step| step.contains("ready")));
    }

    #[test]
    fn suspend_is_sent_once_and_polled_and_start_resumes_a_suspended_machine() {
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "suspend", "running"))
            .reply(200, operation("op_1", "suspend", "succeeded"));
        suspend_with(&client(&http), ID, &mut |_| {}).unwrap();
        assert_eq!(
            http.paths(),
            [
                format!("POST /v1/machines/{ID}/suspend"),
                "GET /v1/operations/op_1".to_owned()
            ]
        );
        assert!(http.sent()[0].idempotency_key.is_some());
        // Resume is an explicit start; it waits until the guest is ready again.
        let http = FakeHttp::new();
        http.reply(202, operation("op_2", "start", "succeeded"))
            .reply(200, machine(ID, "resuming", false))
            .reply(200, machine(ID, "running", true));
        assert!(
            start_with(&client(&http), ID, &mut |_| {})
                .unwrap()
                .runtime
                .ready
        );
        assert_eq!(http.paths()[0], format!("POST /v1/machines/{ID}/start"));
    }

    #[test]
    fn delete_is_sent_once_with_a_key_and_polled_to_completion() {
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "delete", "queued"))
            .reply(200, operation("op_1", "delete", "succeeded"));
        assert_eq!(
            delete_with(&client(&http), ID, &mut |_| {}).unwrap(),
            Deletion::Deleted
        );
        assert_eq!(
            http.paths(),
            [
                format!("DELETE /v1/machines/{ID}"),
                "GET /v1/operations/op_1".to_owned()
            ]
        );
        assert!(http.sent()[0].idempotency_key.is_some());
    }

    #[test]
    fn not_found_counts_as_deleted_only_when_the_machine_is_missing() {
        let http = FakeHttp::new();
        http.error(404, "not_found").error(404, "not_found");
        assert_eq!(
            delete_with(&client(&http), ID, &mut |_| {}).unwrap(),
            Deletion::AlreadyGone
        );
        // The operation record vanished, but the machine still exists.
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "delete", "running"))
            .error(404, "not_found")
            .reply(200, machine(ID, "deleting", false));
        let error = delete_with(&client(&http), ID, &mut |_| {}).unwrap_err();
        assert_eq!(error.code(), Some(&ErrorCode::NotFound));
        // Other failures are reported, never treated as deleted.
        let http = FakeHttp::new();
        http.error(403, "permission_denied");
        assert!(delete_with(&client(&http), ID, &mut |_| {}).is_err());
    }

    fn with_template(
        mut machine: serde_json::Value,
        version: &str,
        synced: bool,
    ) -> serde_json::Value {
        machine["template"] = serde_json::json!({"id": "herdr", "version": version, "digest": "d"});
        machine["storage"] = serde_json::json!({"sizeGiB": 5, "mountPath": "/data", "persistent": true, "synced": synced});
        machine
    }

    fn templates() -> serde_json::Value {
        serde_json::json!({"templates": [
            template("2026-10-02.1", &[]),
            template("2026-10-03.2", &["identity-reset", "root-grow"]),
        ]})
    }

    fn check(state: &str, version: &str, synced: bool) -> SaveCheck {
        let http = FakeHttp::new();
        http.reply(
            200,
            with_template(machine(ID, state, false), version, synced),
        )
        .reply(200, templates());
        let check = check_save_with(&client(&http), ID).unwrap();
        assert_eq!(
            http.paths(),
            [
                format!("GET /v1/machines/{ID}"),
                "GET /v1/templates".to_owned()
            ]
        );
        check
    }

    const NEW: &str = "2026-10-03.2";

    fn request() -> CreateImageRequest<'static> {
        CreateImageRequest {
            name: "base",
            description: "node and gh",
        }
    }

    #[test]
    fn a_stopped_synced_machine_is_saved_directly_with_a_key() {
        let check = check("stopped", NEW, true);
        assert_eq!(check.plan, Some(SavePlan::Save));
        assert!(check.note.is_empty());
        let http = FakeHttp::new();
        http.reply(201, image("im_a", "base", ID));
        let mut quiesced = false;
        let saved = save_image_with(
            &client(&http),
            ID,
            SavePlan::Save,
            &request(),
            &mut || {
                quiesced = true;
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(saved.name, "base");
        assert!(!quiesced, "a stopped machine is not touched");
        assert_eq!(http.paths(), [format!("POST /v1/machines/{ID}/images")]);
        let sent = &http.sent()[0];
        assert!(sent.idempotency_key.is_some());
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name": "base", "description": "node and gh"})
        );
    }

    #[test]
    fn running_suspended_unsynced_and_old_template_machines_explain_themselves() {
        let running = check("running", NEW, true);
        assert_eq!(running.plan, Some(SavePlan::Stop));
        assert!(running
            .note
            .contains("is running. Only a stopped machine can be saved"));
        assert!(running
            .note
            .contains("Stop machine and save stops all sessions and jobs"));
        let suspended = check("suspended", NEW, true);
        assert_eq!(suspended.plan, Some(SavePlan::ResumeThenStop));
        assert!(suspended.note.contains("resumes it"));
        let unsynced = check("stopped", NEW, false);
        assert_eq!(unsynced.plan, Some(SavePlan::Stop));
        assert!(unsynced.note.contains("not uploaded"));
        let old = check("stopped", "2026-10-02.1", true);
        assert_eq!(old.plan, None);
        assert!(old
            .note
            .contains("template herdr@2026-10-02.1, which is too old to save images from"));
        assert!(old
            .note
            .contains("Create a new machine from the latest template"));
        let busy = check("stopping", NEW, true);
        assert_eq!(busy.plan, None);
        assert!(busy.note.contains("is stopping"));
        // Without the template catalog the server decides.
        let machine: Machine =
            serde_json::from_value(with_template(machine(ID, "stopped", false), "x", true))
                .unwrap();
        assert_eq!(
            snapshot_check(&machine, None, SnapshotUse::Image).plan,
            Some(SavePlan::Save)
        );
    }

    #[test]
    fn stop_and_save_quiesces_then_stops_then_saves() {
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "stop", "running"))
            .reply(200, operation("op_1", "stop", "succeeded"))
            .reply(201, image("im_a", "base", ID));
        let sent_before_quiesce = std::cell::Cell::new(None);
        save_image_with(
            &client(&http),
            ID,
            SavePlan::Stop,
            &request(),
            &mut || {
                sent_before_quiesce.set(Some(http.sent().len()));
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(sent_before_quiesce.get(), Some(0));
        assert_eq!(
            http.paths(),
            [
                format!("POST /v1/machines/{ID}/stop"),
                "GET /v1/operations/op_1".to_owned(),
                format!("POST /v1/machines/{ID}/images"),
            ]
        );
        // A suspended machine is resumed (and ready) before its sessions stop.
        let http = FakeHttp::new();
        http.reply(202, operation("op_1", "start", "succeeded"))
            .reply(200, machine(ID, "running", true))
            .reply(202, operation("op_2", "stop", "succeeded"))
            .reply(201, image("im_a", "base", ID));
        let sent_before_quiesce = std::cell::Cell::new(None);
        save_image_with(
            &client(&http),
            ID,
            SavePlan::ResumeThenStop,
            &request(),
            &mut || {
                sent_before_quiesce.set(Some(http.sent().len()));
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(sent_before_quiesce.get(), Some(2));
        assert_eq!(
            http.paths(),
            [
                format!("POST /v1/machines/{ID}/start"),
                format!("GET /v1/machines/{ID}"),
                format!("POST /v1/machines/{ID}/stop"),
                format!("POST /v1/machines/{ID}/images"),
            ]
        );
    }

    #[test]
    fn a_failed_stop_never_saves_and_refusals_show_hangars_reason() {
        let http = FakeHttp::new();
        http.error(409, "operation_conflict");
        let error = save_image_with(
            &client(&http),
            ID,
            SavePlan::Stop,
            &request(),
            &mut || Ok(()),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error.code(), Some(&ErrorCode::OperationConflict));
        assert!(!http.paths().iter().any(|path| path.ends_with("/images")));
        let http = FakeHttp::new();
        http.reply(
            409,
            serde_json::json!({"error": {"code": "operation_conflict", "message": "an image named \"base\" already exists", "requestId": "r", "retryable": false, "operationId": null}}),
        );
        let error = save_image_with(
            &client(&http),
            ID,
            SavePlan::Save,
            &request(),
            &mut || Ok(()),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(
            image_error(&error),
            "an image named \"base\" already exists"
        );
        assert!(validate_image_name("base-1").is_ok());
        for bad in ["", "-x", "Base", "a b", &"a".repeat(64)] {
            assert!(validate_image_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_from_an_image_sends_its_id_and_reports_a_deleted_image() {
        let http = FakeHttp::new();
        let mut created = operation("op_1", "create", "succeeded");
        created["machineId"] = ID.into();
        http.reply(202, created)
            .reply(200, machine(ID, "running", true));
        let source = MachineSource::Image {
            id: "im_a".into(),
            name: "base".into(),
        };
        create_with(&client(&http), "box", &source, &mut |_| {}).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(http.sent()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({"name": "box", "imageId": "im_a"}));
        let http = FakeHttp::new();
        http.error(404, "not_found");
        let error = create_with(&client(&http), "box", &source, &mut |_| {}).unwrap_err();
        assert!(
            error.to_string().contains("image base was deleted"),
            "{error}"
        );
    }

    #[test]
    fn deleting_a_missing_image_counts_as_deleted_and_images_sort_newest_first() {
        let http = FakeHttp::new();
        http.error(404, "not_found");
        assert_eq!(
            delete_image_with(&client(&http), "im_a").unwrap(),
            Deletion::AlreadyGone
        );
        assert!(http.sent()[0].idempotency_key.is_some());
        let mut older: Image = serde_json::from_value(image("im_a", "old", ID)).unwrap();
        older.created_at = "2026-10-01T00:00:00Z".into();
        let newer: Image = serde_json::from_value(image("im_b", "new", ID)).unwrap();
        let mut images = vec![older, newer];
        sort_images(&mut images);
        assert_eq!(images[0].name, "new");
    }

    fn fork_check(state: &str, version: &str, synced: bool) -> SaveCheck {
        let http = FakeHttp::new();
        http.reply(
            200,
            with_template(machine(ID, state, false), version, synced),
        )
        .reply(200, templates());
        check_snapshot_with(&client(&http), ID, SnapshotUse::Fork).unwrap()
    }

    #[test]
    fn clone_checks_plan_like_images_with_clone_wording() {
        assert_eq!(fork_check("stopped", NEW, true).plan, Some(SavePlan::Save));
        let running = fork_check("running", NEW, true);
        assert_eq!(running.plan, Some(SavePlan::Stop));
        assert!(running.note.contains(
            "Only a stopped machine can be cloned. Stop machine and clone stops all sessions"
        ));
        assert!(running.note.ends_with("then clones it."));
        let errored = fork_check("error", NEW, true);
        assert_eq!(errored.plan, Some(SavePlan::Stop));
        let unsynced = fork_check("stopped", NEW, false);
        assert_eq!(unsynced.plan, Some(SavePlan::Stop));
        assert!(unsynced
            .note
            .contains("Stop machine and clone stops it again"));
        let suspended = fork_check("suspended", NEW, true);
        assert_eq!(suspended.plan, Some(SavePlan::ResumeThenStop));
        assert!(suspended.note.contains("resumes it"));
        let old = fork_check("stopped", "2026-10-02.1", true);
        assert_eq!(old.plan, None);
        assert!(old.note.contains("which is too old to clone."));
        assert!(old.note.contains("clone that one"));
        let busy = fork_check("starting", NEW, true);
        assert_eq!(busy.plan, None);
        assert!(busy.note.contains("then open Copy machine… again"));
    }

    #[test]
    fn a_stopped_machine_is_forked_with_a_key_and_the_fork_is_waited_ready() {
        const FORK: &str = "m_forkforkforkforkforkforkfo";
        let http = FakeHttp::new();
        let mut accepted = operation("op_1", "create", "queued");
        accepted["machineId"] = FORK.into();
        let mut done = operation("op_1", "create", "succeeded");
        done["machineId"] = FORK.into();
        http.reply(202, accepted)
            .reply(200, done)
            .reply(200, machine(FORK, "starting", false))
            .reply(200, machine(FORK, "running", true));
        let mut quiesced = false;
        let fork = fork_with(
            &client(&http),
            ID,
            SavePlan::Save,
            "box-fork",
            &mut || {
                quiesced = true;
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fork.id, FORK);
        assert!(!quiesced, "a stopped source is not touched");
        assert_eq!(
            http.paths(),
            [
                format!("POST /v1/machines/{ID}/fork"),
                "GET /v1/operations/op_1".to_owned(),
                format!("GET /v1/machines/{FORK}"),
                format!("GET /v1/machines/{FORK}"),
            ]
        );
        let sent = &http.sent()[0];
        assert!(sent.idempotency_key.is_some());
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name": "box-fork", "desiredState": "running"})
        );
    }

    #[test]
    fn stop_and_fork_quiesces_and_stops_before_forking_and_a_failed_stop_never_forks() {
        let http = FakeHttp::new();
        let mut accepted = operation("op_2", "create", "succeeded");
        accepted["machineId"] = "m_new".into();
        http.reply(202, operation("op_1", "stop", "succeeded"))
            .reply(202, accepted)
            .reply(200, machine("m_new", "running", true));
        let sent_before_quiesce = std::cell::Cell::new(None);
        fork_with(
            &client(&http),
            ID,
            SavePlan::Stop,
            "box-fork",
            &mut || {
                sent_before_quiesce.set(Some(http.sent().len()));
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(sent_before_quiesce.get(), Some(0));
        assert_eq!(
            http.paths(),
            [
                format!("POST /v1/machines/{ID}/stop"),
                format!("POST /v1/machines/{ID}/fork"),
                "GET /v1/machines/m_new".to_owned(),
            ]
        );
        let http = FakeHttp::new();
        http.error(409, "operation_conflict");
        assert!(fork_with(
            &client(&http),
            ID,
            SavePlan::Stop,
            "box-fork",
            &mut || Ok(()),
            &mut |_| {},
        )
        .is_err());
        assert!(!http.paths().iter().any(|path| path.ends_with("/fork")));
        // A quiesce failure stops nothing.
        let http = FakeHttp::new();
        assert!(fork_with(
            &client(&http),
            ID,
            SavePlan::Stop,
            "box-fork",
            &mut || Err("locked".into()),
            &mut |_| {},
        )
        .is_err());
        assert!(http.paths().is_empty());
    }

    #[test]
    fn fork_refusals_show_hangars_reason_and_names_are_checked() {
        let refusal = |status: u16, code: &str, message: &str| {
            let http = FakeHttp::new();
            http.reply(
                status,
                serde_json::json!({"error": {"code": code, "message": message, "requestId": "r", "retryable": false, "operationId": null}}),
            );
            let error = fork_with(
                &client(&http),
                ID,
                SavePlan::Save,
                "box-fork",
                &mut || Ok(()),
                &mut |_| {},
            )
            .unwrap_err();
            assert_eq!(http.paths().len(), 1, "a refusal is not retried");
            fork_error(&error)
        };
        assert_eq!(
            refusal(
                409,
                "operation_conflict",
                "a machine named \"box-fork\" already exists"
            ),
            "a machine named \"box-fork\" already exists"
        );
        assert_eq!(
            refusal(
                400,
                "bad_request",
                "template herdr@x does not support images and forks"
            ),
            "template herdr@x does not support images and forks"
        );
        assert_eq!(
            refusal(429, "quota_exceeded", "machine limit reached (5)"),
            "machine limit reached (5)"
        );
        assert_eq!(
            refusal(404, "not_found", "machine not found"),
            "the hangar machine no longer exists"
        );
        // A failed create operation is reported, not retried.
        let http = FakeHttp::new();
        let mut failed = operation("op_1", "create", "failed");
        failed["machineId"] = "m_new".into();
        failed["error"] = serde_json::json!({"code": "no_capacity", "message": "full", "requestId": "r", "retryable": true, "operationId": "op_1"});
        http.reply(202, failed);
        let error = fork_with(
            &client(&http),
            ID,
            SavePlan::Save,
            "box-fork",
            &mut || Ok(()),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(error.code(), Some(&ErrorCode::NoCapacity));
        assert_eq!(default_clone_name("box"), "box-clone");
        let long = default_clone_name(&"a".repeat(63));
        assert_eq!(long.len(), 63);
        assert!(validate_fork_name(&long).is_ok());
        assert_eq!(
            default_clone_name(&format!("{}-b", "a".repeat(57))),
            format!("{}-clone", "a".repeat(57))
        );
        assert!(validate_fork_name("box-fork").is_ok());
        for bad in ["", "-x", "Box", "a b", &"a".repeat(64)] {
            assert!(validate_fork_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_machine_that_stops_while_starting_is_not_waited_on() {
        let http = FakeHttp::new();
        http.reply(200, machine(ID, "stopped", false));
        let error = wait_ready(&client(&http), ID, &mut |_| {}).unwrap_err();
        assert!(matches!(error, HangarError::MachineNotRunning { .. }));
    }
}
