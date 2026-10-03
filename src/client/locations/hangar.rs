//! hangar machine actions for the remotes UI and CLI. These block on HTTP and SSH and
//! run only on worker threads. Accepting a mutation is not readiness: herdr polls the
//! operation and the machine, and never repeats an accepted mutation. Nothing here
//! starts a machine implicitly; only explicit Start remote and Create do.
use std::time::{Duration, Instant};

use super::{operation_lock, CloudBinding, LocationPreferences, RemoteOptions};
use crate::client::endpoint::{EndpointCatalog, SavedSshEndpoint, MAX_LABEL_BYTES};
use crate::hangar::api::{
    Client, CreateImageRequest, CreateMachineRequest, ErrorCode, HangarError, Image, Machine,
    MachineState, Operation, OperationState, Template, TemplateCapability,
};
use crate::hangar::binding::HangarBinding;

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

/// What Save as image… stores, shown before saving.
pub(crate) const IMAGE_CONTENTS: &str = "Saves the machine's root disk only. Installed packages and system configuration are included. Your home directory files, logins and /data/workspace are not. Anything written to the root disk is included, such as credentials from `sudo gh auth` or tokens in /etc/environment. The image is private to your hangar account.";

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

/// `templates` is `None` when the catalog could not be read; the server then decides.
pub(crate) fn save_check(machine: &Machine, templates: Option<&[Template]>) -> SaveCheck {
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
                    "{name} was created from template {}, which is too old to save images from. Create a new machine from the latest template (Add remote → Create new machine), set it up there, and save that one.",
                    template.label()
                ),
            };
        }
    }
    let synced = machine
        .storage
        .as_ref()
        .is_none_or(|storage| storage.synced);
    let (plan, note) = match machine.state {
        MachineState::Stopped if synced => (Some(SavePlan::Save), String::new()),
        MachineState::Stopped => (
            Some(SavePlan::Stop),
            format!("{name} has changes that are not uploaded yet. Stop machine and save stops it again to upload them, then saves the image."),
        ),
        MachineState::Running | MachineState::Error => (
            Some(SavePlan::Stop),
            format!("{name} is {}. Only a stopped machine can be saved. Stop machine and save stops all sessions and jobs on it (as Stop machine… does), waits until it is stopped, then saves the image.", machine.state.as_str()),
        ),
        MachineState::Suspended => (
            Some(SavePlan::ResumeThenStop),
            format!("{name} is suspended. Only a stopped machine can be saved, and a suspended machine keeps its memory. Stop machine and save resumes it, stops all sessions and jobs on it, waits until it is stopped, then saves the image."),
        ),
        state => (
            None,
            format!("{name} is {}. Wait until it is stopped or running, then open Save as image… again.", state.as_str()),
        ),
    };
    SaveCheck { plan, note }
}

pub(crate) fn check_save_with(client: &Client, machine_id: &str) -> Result<SaveCheck, HangarError> {
    let machine = client.machine(machine_id)?;
    let templates = client
        .templates()
        .map_err(|error| tracing::debug!(%error, "could not read hangar templates"))
        .ok();
    Ok(save_check(&machine, templates.as_deref()))
}

/// Image names follow hangar's rule; checked before any machine is stopped.
pub(crate) fn validate_image_name(name: &str) -> Result<(), String> {
    let valid = (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-');
    if valid {
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
    if plan == SavePlan::ResumeThenStop {
        start_with(client, machine_id, progress)?;
    }
    if plan != SavePlan::Save {
        quiesce().map_err(HangarError::Invalid)?;
        stop_with(client, machine_id, progress)?;
    }
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

/// Newest first, as the source picker shows them.
pub(crate) fn sort_images(images: &mut [Image]) {
    images.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.name.cmp(&right.name))
    });
}

pub(crate) fn describe(machine: &Machine) -> String {
    let ready = match (machine.state, machine.runtime.ready) {
        (MachineState::Running, true) => ", ready",
        (MachineState::Running, false) => ", not ready yet",
        _ => "",
    };
    let error = machine
        .last_error
        .as_ref()
        .filter(|error| !error.message.is_empty())
        .map(|error| format!(" (last error: {})", error.message))
        .unwrap_or_default();
    let origin = match (&machine.image, &machine.forked_from) {
        (Some(image), _) if !image.id.is_empty() => format!(", from image {}", image.id),
        (_, Some(fork)) if !fork.machine_id.is_empty() => {
            format!(", forked from {}", fork.machine_id)
        }
        _ => String::new(),
    };
    format!(
        "{}: {}{ready}{origin}{error}",
        machine.name,
        machine.state.as_str()
    )
}

fn hangar_client(server: &str) -> Result<Client, HangarError> {
    crate::hangar::auth::shared_client(server)
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

/// Current states of the given machines on `server`, for labels such as Resume remote.
pub(crate) fn machine_states(server: &str) -> Result<Vec<(String, MachineState)>, HangarError> {
    Ok(list_machines(server)?
        .into_iter()
        .map(|machine| (machine.id, machine.state))
        .collect())
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

pub(crate) fn machine_status(binding: &HangarBinding) -> Result<String, HangarError> {
    Ok(describe(
        &hangar_client(&binding.server)?.machine(&binding.machine_id)?,
    ))
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

fn unique_label(catalog: &EndpointCatalog, wanted: &str) -> String {
    let base: String = wanted.chars().take(MAX_LABEL_BYTES / 4).collect();
    let base = if base.trim().is_empty() {
        "hangar".to_owned()
    } else {
        base
    };
    let taken = |label: &str| catalog.ssh.iter().any(|profile| profile.label == label);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|suffix| format!("{base} ({suffix})"))
        .find(|label| !taken(label))
        .unwrap_or(base)
}

/// Saves a profile for `machine` whose target is its hangar alias. Rejects a
/// machine that is already bound to another profile.
pub(crate) fn save_binding(
    server: &str,
    machine: &Machine,
    label: Option<&str>,
    session: &str,
    enabled: bool,
) -> Result<(SavedSshEndpoint, RemoteOptions), String> {
    let binding = HangarBinding::new(server, &machine.id, &machine.name)?;
    let _guard = operation_lock()?;
    let mut prefs = LocationPreferences::load()?;
    if prefs
        .remotes
        .values()
        .filter_map(|options| options.cloud.as_ref())
        .any(|cloud| {
            cloud.hangar().machine_id == binding.machine_id
                && cloud.hangar().server == binding.server
        })
    {
        return Err(format!(
            "{} is already added. Select it in Settings → remotes.",
            machine.name
        ));
    }
    let mut catalog = EndpointCatalog::load()?;
    if catalog.ssh.len() >= 64 {
        return Err("At most 64 remotes can be saved".into());
    }
    let label = match label {
        Some(label) => label.to_owned(),
        None => unique_label(&catalog, &machine.name),
    };
    let mut profile = SavedSshEndpoint::new(label, &binding.alias, session)?;
    profile.enabled = enabled;
    let options = RemoteOptions {
        cwd: String::new(),
        cloud: Some(CloudBinding::Hangar(binding)),
    };
    prefs.remotes.insert(profile.id.clone(), options.clone());
    // Metadata first: the profile must not appear without its binding.
    prefs.store()?;
    catalog.ssh.push(profile.clone());
    catalog.store_profiles()?;
    Ok((profile, options))
}

/// Binds a machine and, when it is running, starts its Herdr session and enables
/// automatic connection. A stopped machine is saved disabled and left stopped.
pub(crate) fn add_machine(
    server: &str,
    machine: &Machine,
    progress: Progress<'_>,
) -> Result<String, String> {
    let (profile, options) = save_binding(server, machine, None, SESSION, false)?;
    if machine.state != MachineState::Running {
        return Ok(format!(
            "{} was saved. It is {}; use Start remote to start it.",
            profile.label,
            machine.state.as_str()
        ));
    }
    progress(format!("Connecting to {}…", machine.name));
    super::start_session(&profile, &options).map_err(|error| {
        format!(
            "{} was saved but not connected: {error} Use Start remote to retry.",
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
        assert_eq!(save_check(&machine, None).plan, Some(SavePlan::Save));
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

    #[test]
    fn a_machine_that_stops_while_starting_is_not_waited_on() {
        let http = FakeHttp::new();
        http.reply(200, machine(ID, "stopped", false));
        let error = wait_ready(&client(&http), ID, &mut |_| {}).unwrap_err();
        assert!(matches!(error, HangarError::MachineNotRunning { .. }));
    }
}
