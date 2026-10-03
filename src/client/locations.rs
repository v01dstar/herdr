//! Client-owned defaults and provider bindings. Keep these out of the v1 SSH catalog
//! and the frozen runtime codecs: a server does not own another machine's locations.
use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::endpoint::{EndpointCatalog, ProfileId, SavedSshEndpoint};
use crate::hangar::binding::HangarBinding;

pub(crate) mod hangar;

const VERSION: u32 = 2;
const MAX_REMOTES: usize = 64;
const LOCK_WAIT: Duration = Duration::from_secs(3);

/// A remote whose machine lifecycle belongs to a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub(crate) enum CloudBinding {
    Hangar(HangarBinding),
}

impl CloudBinding {
    pub fn hangar(&self) -> &HangarBinding {
        match self {
            Self::Hangar(binding) => binding,
        }
    }

    /// Profiles bound to the same machine share its lifecycle.
    fn same_machine(&self, other: &Self) -> bool {
        let (left, right) = (self.hangar(), other.hangar());
        left.machine_id == right.machine_id && left.server == right.server
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteOptions {
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub cloud: Option<CloudBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocationPreferences {
    version: u32,
    pub default_profile: Option<ProfileId>,
    pub remotes: BTreeMap<ProfileId, RemoteOptions>,
    /// Shown once in the remotes dialog, then cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

impl Default for LocationPreferences {
    fn default() -> Self {
        Self {
            version: VERSION,
            default_profile: None,
            remotes: BTreeMap::new(),
            notice: None,
        }
    }
}

/// Version 1 stored Instacloud bindings (`project`, `branch`, `service`, …).
#[derive(Deserialize)]
struct V1Preferences {
    #[serde(default)]
    default_profile: Option<ProfileId>,
    #[serde(default)]
    remotes: BTreeMap<ProfileId, V1RemoteOptions>,
}

#[derive(Deserialize)]
struct V1RemoteOptions {
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    cloud: Option<serde_json::Value>,
}

const INSTACLOUD_NOTICE: &str = "Instacloud remotes are no longer supported and were disabled. Their profiles and default directories were kept; remove them, or add the machine again from hangar.";

/// Keeps every profile and directory; Instacloud bindings are dropped and their
/// profiles are returned so the caller can disable them.
fn migrate_v1(bytes: &[u8]) -> Result<(LocationPreferences, Vec<ProfileId>), String> {
    let old: V1Preferences = serde_json::from_slice(bytes)
        .map_err(|error| format!("Invalid remote locations: {error}"))?;
    let mut disabled = Vec::new();
    let remotes = old
        .remotes
        .into_iter()
        .map(|(id, options)| {
            if options.cloud.is_some_and(|cloud| !cloud.is_null()) {
                disabled.push(id.clone());
            }
            (
                id,
                RemoteOptions {
                    cwd: options.cwd,
                    cloud: None,
                },
            )
        })
        .collect();
    let prefs = LocationPreferences {
        version: VERSION,
        default_profile: old.default_profile,
        remotes,
        notice: (!disabled.is_empty()).then(|| INSTACLOUD_NOTICE.to_owned()),
    };
    prefs.validate()?;
    Ok((prefs, disabled))
}

impl LocationPreferences {
    fn path() -> std::path::PathBuf {
        crate::config::state_dir()
            .join("client")
            .join("locations.json")
    }

    pub fn load() -> Result<Self, String> {
        let file = match std::fs::File::open(Self::path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(format!("Cannot read remote locations: {error}")),
        };
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 65536 {
            return Err("Remote locations file is too large".into());
        }
        let version = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| format!("Invalid remote locations: {error}"))?
            .get("version")
            .and_then(serde_json::Value::as_u64);
        if version == Some(1) {
            return Self::migrate(&bytes);
        }
        let prefs: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid remote locations: {error}"))?;
        prefs.validate()?;
        Ok(prefs)
    }

    /// One-time upgrade. Profiles are disabled before the new file is written, so an
    /// interrupted migration runs again rather than leaving a stale binding enabled.
    /// Idempotent, so it needs no lock (callers may already hold `operation_lock`).
    fn migrate(bytes: &[u8]) -> Result<Self, String> {
        let (prefs, disabled) = migrate_v1(bytes)?;
        if !disabled.is_empty() {
            let mut catalog = EndpointCatalog::load()?;
            for id in &disabled {
                catalog.set_enabled(id, false);
            }
            catalog.store_profiles()?;
        }
        prefs.store()?;
        tracing::info!(
            disabled = disabled.len(),
            "migrated remote locations to version 2"
        );
        Ok(prefs)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION || self.remotes.len() > MAX_REMOTES {
            return Err("Unsupported remote locations file".into());
        }
        for (id, options) in &self.remotes {
            ProfileId::parse(id.to_string())?;
            options.validate()?;
        }
        if let Some(id) = &self.default_profile {
            ProfileId::parse(id.to_string())?;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.len() > 1024)
        {
            return Err("Remote locations notice is too long".into());
        }
        Ok(())
    }

    pub fn store(&self) -> Result<(), String> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        super::endpoint::store_private_json(&Self::path(), &bytes, "remote locations")
    }

    pub fn default_index(&self, profiles: &[SavedSshEndpoint]) -> usize {
        self.default_profile
            .as_ref()
            .and_then(|id| profiles.iter().position(|p| &p.id == id))
            .map_or(0, |index| index + 1)
    }

    pub fn binding(&self, id: &ProfileId) -> Option<&CloudBinding> {
        self.remotes
            .get(id)
            .and_then(|options| options.cloud.as_ref())
    }

    /// Returns the migration notice once.
    pub fn take_notice() -> Option<String> {
        let _guard = operation_lock().ok()?;
        let mut prefs = Self::load().ok()?;
        let notice = prefs.notice.take()?;
        if let Err(error) = prefs.store() {
            tracing::warn!(%error, "could not clear the remote locations notice");
        }
        Some(notice)
    }
}

impl RemoteOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.cwd.len() > 4096 || self.cwd.chars().any(char::is_control) {
            return Err("Directory must be at most 4096 bytes with no control characters".into());
        }
        if let Some(CloudBinding::Hangar(binding)) = &self.cloud {
            binding.validate()?;
        }
        Ok(())
    }
}

/// The saved hangar binding whose alias is `target`.
pub(crate) fn hangar_binding_for_target(target: &str) -> Result<Option<HangarBinding>, String> {
    Ok(LocationPreferences::load()?
        .remotes
        .values()
        .filter_map(|options| options.cloud.as_ref())
        .map(CloudBinding::hangar)
        .find(|binding| binding.alias == target)
        .cloned())
}

/// Serializes short read-modify-write updates of the catalog and location files
/// across clients sharing this state directory. Never held across network calls.
/// Keeping the open file holds the OS lock; process exit releases it.
pub(crate) fn operation_lock() -> Result<std::fs::File, String> {
    let directory = crate::config::state_dir().join("client");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let path = directory.join("location-operation.lock");
    let file = match crate::platform::create_private_state_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(&path)
                .map_err(|error| error.to_string())?
                .file_type()
                .is_symlink()
            {
                return Err("Remote operation lock must not be a symlink".into());
            }
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    // Bounded wait: holders only edit small files, and a nested attempt in the same
    // thread fails instead of deadlocking.
    let deadline = Instant::now() + LOCK_WAIT;
    while file.try_lock().is_err() {
        if Instant::now() >= deadline {
            return Err(
                "Another client is changing remotes. Wait a moment and try again.".to_owned(),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(file)
}

pub(super) fn same_destination(left: &SavedSshEndpoint, right: &SavedSshEndpoint) -> bool {
    left.id == right.id && left.target == right.target && left.session == right.session
}

pub(super) fn validate_binding(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudBinding>,
) -> Result<(), String> {
    let profiles = EndpointCatalog::load_profiles()?;
    let prefs = LocationPreferences::load()?;
    validate_binding_in(profile, cloud, &profiles, &prefs)
}

fn validate_binding_in(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudBinding>,
    profiles: &[SavedSshEndpoint],
    prefs: &LocationPreferences,
) -> Result<(), String> {
    if !profiles.iter().any(|p| same_destination(p, profile)) || prefs.binding(&profile.id) != cloud
    {
        return Err("Remote was removed or changed in another client. Close and reopen this dialog before continuing.".into());
    }
    Ok(())
}

/// Enables only `profile`; disabling covers every profile bound to the same machine.
pub(super) fn set_service_enabled(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudBinding>,
    enabled: bool,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    validate_binding(profile, cloud)?;
    let mut catalog = EndpointCatalog::load()?;
    let prefs = LocationPreferences::load()?;
    let ids = catalog
        .ssh
        .iter()
        .filter(|p| {
            p.id == profile.id
                || (!enabled
                    && cloud.is_some_and(|target| {
                        prefs
                            .binding(&p.id)
                            .is_some_and(|other| other.same_machine(target))
                    }))
        })
        .map(|p| p.id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        catalog.set_enabled(&id, enabled);
    }
    catalog.store_profiles()
}

/// Starts the installed Herdr session (never installs or replaces a remote binary) and
/// enables automatic connection. A hangar machine must already be running.
pub(super) fn start_session(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<(), String> {
    validate_binding(profile, options.cloud.as_ref())?;
    crate::remote::start_saved_ssh(&profile.target, &profile.session)
        .map_err(|error| error.to_string())?;
    set_service_enabled(profile, options.cloud.as_ref(), true)
}

/// Explicit Start remote (Resume remote when suspended): starts or resumes the hangar
/// machine first when the remote has one.
pub(super) fn start_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<(), String> {
    validate_binding(profile, options.cloud.as_ref())?;
    if let Some(cloud) = &options.cloud {
        hangar::start_machine(cloud.hangar(), &mut |_| {}).map_err(|error| error.to_string())?;
    }
    start_session(profile, options)
}

pub(super) fn machine_status(options: &RemoteOptions) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("This remote has no hangar machine")?;
    hangar::machine_status(cloud.hangar()).map_err(|error| error.to_string())
}

pub(super) fn stop_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("Stop machine requires a hangar machine")?;
    // Persist the fence before SSH or provider I/O: every client observes the same
    // disabled profiles, and reconnect never starts the machine again.
    set_service_enabled(profile, Some(cloud), false)?;
    let catalog = EndpointCatalog::load()?;
    let prefs = LocationPreferences::load()?;
    let mut warnings = Vec::new();
    for other in &catalog.ssh {
        let bound = other.id == profile.id
            || prefs
                .binding(&other.id)
                .is_some_and(|binding| binding.same_machine(cloud));
        if bound {
            if let Err(error) = crate::remote::stop_saved_ssh(&other.target, &other.session) {
                warnings.push(format!("{}: {error}", other.label));
            }
        }
    }
    hangar::stop_machine(cloud.hangar(), &mut |_| {}).map_err(|error| error.to_string())?;
    let message = format!("{}: stopped", cloud.hangar().machine_name);
    if warnings.is_empty() {
        Ok(message)
    } else {
        Ok(format!(
            "{message}. Graceful shutdown warnings: {}",
            warnings.join("; ")
        ))
    }
}

/// Suspend remote: disables automatic connection (so no client fights the gateway
/// dropping its connections), then snapshots the machine's memory. Unlike Stop machine
/// the Herdr server is not shut down; its sessions and programs continue after Resume.
pub(super) fn suspend_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<String, String> {
    suspend_remote_with(profile, options, hangar::suspend_machine)
}

fn suspend_remote_with(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
    suspend: impl FnOnce(
        &HangarBinding,
        hangar::Progress<'_>,
    ) -> Result<(), crate::hangar::api::HangarError>,
) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("Suspend remote requires a hangar machine")?;
    set_service_enabled(profile, Some(cloud), false)?;
    suspend(cloud.hangar(), &mut |_| {}).map_err(|error| error.to_string())?;
    Ok(format!(
        "{}: suspended. Running programs resume with Resume remote.",
        cloud.hangar().machine_name
    ))
}

/// Removes an SSH-only profile and its location metadata. A hangar remote is removed
/// only together with its machine (`delete_remote`), so this never orphans one.
pub(super) fn remove_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    validate_binding(profile, options.cloud.as_ref())?;
    if let Some(cloud) = &options.cloud {
        return Err(format!(
            "{} is a hangar remote. Removing it deletes the machine; use Remove remote… to confirm.",
            cloud.hangar().machine_name
        ));
    }
    let mut catalog = EndpointCatalog::load()?;
    catalog.remove_ssh(&profile.id);
    catalog.store_profiles()?;
    remove_binding(&profile.id)
}

/// Drops location metadata for a removed profile. Callers hold `operation_lock`.
pub(crate) fn remove_binding(id: &ProfileId) -> Result<(), String> {
    let mut prefs = LocationPreferences::load()?;
    let removed = prefs.remotes.remove(id).is_some();
    let was_default = prefs.default_profile.as_ref() == Some(id);
    if was_default {
        prefs.default_profile = None;
    }
    if removed || was_default {
        prefs.store()?;
    }
    Ok(())
}

/// Profiles bound to the same hangar machine as `cloud`.
pub(crate) fn machine_profiles<'a>(
    profiles: &'a [SavedSshEndpoint],
    prefs: &LocationPreferences,
    cloud: &CloudBinding,
) -> Vec<&'a SavedSshEndpoint> {
    profiles
        .iter()
        .filter(|profile| {
            prefs
                .binding(&profile.id)
                .is_some_and(|binding| binding.same_machine(cloud))
        })
        .collect()
}

fn quoted_list(labels: &[&str]) -> String {
    labels
        .iter()
        .map(|label| format!("'{label}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What deleting a hangar machine destroys. `labels` are the Herdr remotes that go
/// away with it.
pub(crate) fn delete_consequences(labels: &[&str]) -> String {
    let entries = match labels {
        [] => "No Herdr remote uses it.".to_owned(),
        [_] => format!("Herdr also removes the remote {}.", quoted_list(labels)),
        _ => format!("Herdr also removes the remotes {}.", quoted_list(labels)),
    };
    format!(
        "The machine, its disks and snapshots are permanently deleted, with every file and process on it. This cannot be undone. {entries}"
    )
}

/// Confirmation shown before a hangar machine is deleted.
pub(crate) fn delete_confirmation(machine_name: &str, labels: &[&str]) -> String {
    format!(
        "Delete hangar machine '{machine_name}'? {}",
        delete_consequences(labels)
    )
}

/// Deletes the hangar machine behind `profile`, then removes every profile bound to it.
/// Local entries are removed only after hangar confirms the machine is gone.
pub(crate) fn delete_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
    progress: hangar::Progress<'_>,
) -> Result<String, String> {
    delete_remote_with(
        profile,
        options,
        |other| {
            crate::remote::stop_saved_ssh(&other.target, &other.session)
                .map_err(|error| error.to_string())
        },
        hangar::delete_machine,
        progress,
    )
}

fn delete_remote_with(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
    stop_session: impl Fn(&SavedSshEndpoint) -> Result<(), String>,
    delete: impl FnOnce(
        &HangarBinding,
        hangar::Progress<'_>,
    ) -> Result<hangar::Deletion, crate::hangar::api::HangarError>,
    progress: hangar::Progress<'_>,
) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("Remove remote requires a hangar machine; use Remove profile")?;
    // Same fence as Stop machine: no client reconnects to a machine being deleted.
    set_service_enabled(profile, Some(cloud), false)?;
    let catalog = EndpointCatalog::load()?;
    let prefs = LocationPreferences::load()?;
    for other in machine_profiles(&catalog.ssh, &prefs, cloud) {
        progress(format!("Stopping Herdr on {}…", other.label));
        // The machine is deleted next; a session that cannot stop cleanly goes with it.
        if let Err(error) = stop_session(other) {
            tracing::debug!(%error, label = %other.label, "graceful stop before delete failed");
        }
    }
    let name = &cloud.hangar().machine_name;
    let deletion = delete(cloud.hangar(), progress).map_err(|error| {
        let error = error.to_string();
        let error = error.trim_end_matches('.');
        format!(
            "Could not delete '{name}': {error}. Nothing was removed from Herdr; remotes for '{name}' stay disabled. Retry Remove remote…, or use Start remote to keep using it."
        )
    })?;
    let removed = remove_machine_profiles(cloud).map_err(|error| {
        format!("hangar machine '{name}' was deleted, but its Herdr remotes were not removed: {error}. Remove them with Remove remote… again.")
    })?;
    forget_machine_files(cloud.hangar());
    let removed = removed.iter().map(String::as_str).collect::<Vec<_>>();
    let entries = if removed.is_empty() {
        String::new()
    } else {
        format!(" Removed {} from Herdr.", quoted_list(&removed))
    };
    Ok(match deletion {
        hangar::Deletion::Deleted => format!("Deleted hangar machine '{name}'.{entries}"),
        hangar::Deletion::AlreadyGone => {
            format!("hangar machine '{name}' was already deleted.{entries}")
        }
    })
}

/// Removes every profile and location entry bound to `cloud`'s machine. Returns the
/// removed profile labels.
fn remove_machine_profiles(cloud: &CloudBinding) -> Result<Vec<String>, String> {
    let _guard = operation_lock()?;
    let mut catalog = EndpointCatalog::load()?;
    let mut prefs = LocationPreferences::load()?;
    let removed = machine_profiles(&catalog.ssh, &prefs, cloud)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let selection = catalog.selected_profile.clone();
    for profile in &removed {
        catalog.remove_ssh(&profile.id);
    }
    // Profiles first: a profile never appears without its binding.
    catalog.store_profiles()?;
    if catalog.selected_profile != selection {
        if let Err(error) = catalog.store_selection() {
            tracing::warn!(%error, "could not clear the selection of a removed remote");
        }
    }
    let stale = prefs
        .remotes
        .iter()
        .filter(|(_, options)| {
            options
                .cloud
                .as_ref()
                .is_some_and(|binding| binding.same_machine(cloud))
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in &stale {
        prefs.remotes.remove(id);
    }
    if prefs
        .default_profile
        .as_ref()
        .is_some_and(|id| stale.contains(id))
    {
        prefs.default_profile = None;
    }
    prefs.store()?;
    Ok(removed.into_iter().map(|profile| profile.label).collect())
}

/// Local files that exist only for a machine hangar no longer has: its certificate,
/// cached SSH metadata for its alias, and leftover shared control sockets. The shared
/// key and `known_hosts` stay. Best effort: a leftover file is harmless.
pub(crate) fn forget_machine_files(binding: &HangarBinding) {
    if let Err(error) = crate::hangar::certs::forget_machine(
        &crate::hangar::certs::SshPaths::herdr(),
        &binding.machine_id,
    ) {
        tracing::warn!(%error, "could not remove the certificate of a deleted hangar machine");
    }
    super::endpoint::invalidate_ssh_metadata_target(&binding.alias);
    if let Err(error) = crate::platform::remove_shared_ssh_control_sockets(
        &crate::config::config_path(),
        &binding.alias,
    ) {
        tracing::debug!(%error, "could not remove control sockets of a deleted hangar machine");
    }
}

/// Deletes a hangar machine that no Herdr remote uses (an orphan left by an older
/// Herdr or another client).
pub(crate) fn delete_unbound_machine(
    binding: &HangarBinding,
    progress: hangar::Progress<'_>,
) -> Result<String, String> {
    let cloud = CloudBinding::Hangar(binding.clone());
    {
        let _guard = operation_lock()?;
        let prefs = LocationPreferences::load()?;
        if prefs
            .remotes
            .values()
            .filter_map(|options| options.cloud.as_ref())
            .any(|other| other.same_machine(&cloud))
        {
            return Err(format!(
                "'{}' is now used by a Herdr remote. Remove it from Settings → remotes → Remove remote….",
                binding.machine_name
            ));
        }
    }
    let name = &binding.machine_name;
    let deletion = hangar::delete_machine(binding, progress).map_err(|error| error.to_string())?;
    forget_machine_files(binding);
    match deletion {
        hangar::Deletion::Deleted => Ok(format!("Deleted hangar machine '{name}'.")),
        hangar::Deletion::AlreadyGone => {
            Ok(format!("hangar machine '{name}' was already deleted."))
        }
    }
}

pub(super) fn create_workspace(
    profile: Option<&SavedSshEndpoint>,
    options: &RemoteOptions,
    cwd: String,
    label: String,
) -> Result<String, String> {
    if let Some(profile) = profile {
        validate_binding(profile, options.cloud.as_ref())?;
        if !EndpointCatalog::load_profiles()?
            .iter()
            .any(|p| p.id == profile.id && p.enabled)
        {
            return Err("Remote was disabled before workspace creation".into());
        }
    }
    use crate::api::client::{ApiClient, ConnectionTarget};
    use crate::api::schema::{Method, Request, ResponseResult, WorkspaceCreateParams};
    let bridge = profile
        .map(|profile| {
            crate::remote::SavedSshApiBridge::start(
                profile.id.as_str(),
                &profile.target,
                &profile.session,
                false,
            )
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let path = bridge
        .as_ref()
        .map(|bridge| bridge.socket_path().to_owned())
        .unwrap_or_else(crate::api::socket_path);
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(path));
    let response = client
        .request_value_with_timeout(
            &Request {
                id: "client-new-workspace".into(),
                method: Method::WorkspaceCreate(WorkspaceCreateParams {
                    // The destination server resolves its own default directory. Never send
                    // an ID or cwd copied from a different endpoint.
                    source_workspace_id: None,
                    cwd: (!cwd.trim().is_empty()).then(|| cwd.trim().to_owned()),
                    label: (!label.trim().is_empty()).then(|| label.trim().to_owned()),
                    focus: false,
                    env: Default::default(),
                }),
            },
            std::time::Duration::from_secs(20),
        )
        .map_err(|error| {
            format!("Create outcome unknown: {error}. Check the destination before retrying.")
        })?;
    match crate::api::client::parse_response_value(response)
        .map_err(|error| error.to_string())?
        .result
    {
        ResponseResult::WorkspaceCreated { workspace, .. } => Ok(workspace.workspace_id),
        _ => Err("Remote returned an unexpected workspace result".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";

    fn hangar() -> CloudBinding {
        CloudBinding::Hangar(HangarBinding::new("https://hangar.test", ID, "box").unwrap())
    }

    #[test]
    fn stale_target_session_or_cloud_binding_cannot_authorize_an_operation() {
        let profile = SavedSshEndpoint::new("remote", "host-a", "demo").unwrap();
        let prefs = LocationPreferences::default();
        let mut edited = profile.clone();
        edited.target = "host-b".into();
        assert!(validate_binding_in(&profile, None, &[edited], &prefs).is_err());
        let mut edited = profile.clone();
        edited.session = "other".into();
        assert!(validate_binding_in(&profile, None, &[edited], &prefs).is_err());
        assert!(validate_binding_in(&profile, None, &[], &prefs).is_err());
        assert!(validate_binding_in(
            &profile,
            Some(&hangar()),
            std::slice::from_ref(&profile),
            &prefs
        )
        .is_err());
        let mut renamed = profile.clone();
        renamed.label = "new label".into();
        assert!(validate_binding_in(&profile, None, &[renamed], &prefs).is_ok());
    }

    #[test]
    fn removed_default_falls_back_to_local_without_using_last_active_machine() {
        let profile = SavedSshEndpoint::new("remote", "host", "demo").unwrap();
        let prefs = LocationPreferences {
            default_profile: Some(profile.id.clone()),
            ..Default::default()
        };
        assert_eq!(prefs.default_index(std::slice::from_ref(&profile)), 1);
        assert_eq!(prefs.default_index(&[]), 0);
    }

    #[test]
    fn remote_metadata_is_separate_from_the_strict_generation_one_catalog() {
        let profile = SavedSshEndpoint::new("remote", "host", "demo").unwrap();
        let mut prefs = LocationPreferences::default();
        prefs.remotes.insert(
            profile.id.clone(),
            RemoteOptions {
                cwd: "/data/project".into(),
                cloud: Some(hangar()),
            },
        );
        let encoded = serde_json::to_value(&profile).unwrap();
        assert!(encoded.get("cwd").is_none());
        assert!(encoded.get("cloud").is_none());
        assert!(prefs.validate().is_ok());
    }

    #[test]
    fn hangar_binding_round_trips_as_a_tagged_provider() {
        let profile = ProfileId::generate();
        let mut prefs = LocationPreferences::default();
        prefs.remotes.insert(
            profile.clone(),
            RemoteOptions {
                cwd: String::new(),
                cloud: Some(hangar()),
            },
        );
        let value = serde_json::to_value(&prefs).unwrap();
        assert_eq!(value["version"], 2);
        let cloud = &value["remotes"][profile.as_str()]["cloud"];
        assert_eq!(cloud["provider"], "hangar");
        assert_eq!(cloud["machineId"], ID);
        assert_eq!(cloud["alias"], format!("hangar-{ID}"));
        assert!(value.get("notice").is_none());
        let decoded: LocationPreferences = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.binding(&profile), Some(&hangar()));
        let mut unknown = serde_json::to_value(&prefs).unwrap();
        unknown["remotes"][profile.as_str()]["cloud"]["provider"] = "instacloud".into();
        assert!(serde_json::from_value::<LocationPreferences>(unknown).is_err());
    }

    #[test]
    fn version_one_instacloud_entries_keep_cwd_lose_binding_and_get_a_notice() {
        let cloud = ProfileId::generate();
        let plain = ProfileId::generate();
        let v1 = serde_json::json!({
            "version": 1,
            "default_profile": cloud,
            "remotes": {
                cloud.as_str(): {"cwd": "/data/workspace", "cloud": {"project": "p", "branch": "main", "service": "herdr-1", "service_id": "s"}},
                plain.as_str(): {"cwd": "/srv", "cloud": null}
            }
        });
        let (prefs, disabled) = migrate_v1(v1.to_string().as_bytes()).unwrap();
        assert_eq!(disabled, vec![cloud.clone()]);
        assert_eq!(prefs.version, VERSION);
        assert_eq!(prefs.remotes[&cloud].cwd, "/data/workspace");
        assert!(prefs.remotes[&cloud].cloud.is_none());
        assert_eq!(prefs.remotes[&plain].cwd, "/srv");
        assert_eq!(prefs.default_profile, Some(cloud));
        assert!(prefs.notice.as_deref().unwrap().contains("Instacloud"));
        let (quiet, none) = migrate_v1(
            serde_json::json!({"version": 1, "default_profile": null, "remotes": {}})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(none.is_empty());
        assert!(quiet.notice.is_none());
    }

    /// Runs `f` with a private state directory holding two profiles bound to the
    /// machine `ID`, one bound to another machine, and one SSH-only profile.
    fn with_remotes<T>(name: &str, f: impl FnOnce(&[SavedSshEndpoint]) -> T) -> T {
        let _guard = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = std::env::var_os("XDG_STATE_HOME");
        let base =
            std::env::temp_dir().join(format!("herdr-delete-remote-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_STATE_HOME", &base);
        let other = CloudBinding::Hangar(
            HangarBinding::new(
                "https://hangar.test",
                "m_bbbbbbbbbbbbbbbbbbbbbbbbbb",
                "other",
            )
            .unwrap(),
        );
        let alias = crate::hangar::binding::alias_for(ID);
        let profiles = vec![
            SavedSshEndpoint::new("box", &alias, "herdr-remote").unwrap(),
            SavedSshEndpoint::new("box (2)", &alias, "agents").unwrap(),
            SavedSshEndpoint::new(
                "other",
                "hangar-m_bbbbbbbbbbbbbbbbbbbbbbbbbb",
                "herdr-remote",
            )
            .unwrap(),
            SavedSshEndpoint::new("plain", "workbox", "default").unwrap(),
        ];
        let mut catalog = EndpointCatalog::default();
        catalog.ssh = profiles.clone();
        catalog.store_profiles().unwrap();
        let mut prefs = LocationPreferences {
            default_profile: Some(profiles[1].id.clone()),
            ..Default::default()
        };
        for (profile, cloud) in
            profiles
                .iter()
                .zip([Some(hangar()), Some(hangar()), Some(other), None])
        {
            prefs.remotes.insert(
                profile.id.clone(),
                RemoteOptions {
                    cwd: String::new(),
                    cloud,
                },
            );
        }
        prefs.store().unwrap();
        // Per-machine files written while the remotes were used.
        let ssh = crate::hangar::certs::SshPaths::herdr();
        std::fs::create_dir_all(ssh.key().parent().unwrap()).unwrap();
        std::fs::write(ssh.key(), "private").unwrap();
        std::fs::write(ssh.known_hosts(), "@cert-authority gateway").unwrap();
        for id in [ID, "m_bbbbbbbbbbbbbbbbbbbbbbbbbb"] {
            std::fs::write(ssh.cert(id), "cert").unwrap();
            std::fs::write(ssh.key().with_file_name(format!("{id}-cert.json")), "{}").unwrap();
        }
        let metadata = crate::client::endpoint::SshMachineMetadata {
            os: "linux".into(),
            executable: "/usr/bin/herdr".into(),
        };
        for profile in &profiles {
            crate::client::endpoint::SshMetadataCache::new(
                profile.id.as_str(),
                &profile.target,
                &profile.session,
            )
            .unwrap()
            .store(&metadata);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&profiles)));
        match old {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&base);
        result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    fn delete_via(
        http: &std::sync::Arc<crate::hangar::api::fake::FakeHttp>,
        profile: &SavedSshEndpoint,
        stopped: &std::cell::RefCell<Vec<String>>,
    ) -> Result<String, String> {
        let client = crate::hangar::api::fake::client(http);
        let options = LocationPreferences::load().unwrap().remotes[&profile.id].clone();
        delete_remote_with(
            profile,
            &options,
            |other| {
                stopped.borrow_mut().push(other.label.clone());
                Ok(())
            },
            |binding, progress| hangar::delete_with(&client, &binding.machine_id, progress),
            &mut |_| {},
        )
    }

    /// Which per-machine files remain: (certificate, certificate record, metadata).
    fn machine_files(id: &str, profiles: &[&SavedSshEndpoint]) -> (bool, bool, bool) {
        let ssh = crate::hangar::certs::SshPaths::herdr();
        assert!(ssh.key().exists() && ssh.known_hosts().exists());
        let metadata = profiles.iter().any(|profile| {
            crate::config::state_dir()
                .join("client/ssh-metadata")
                .join(format!("{}.json", profile.id))
                .exists()
        });
        (
            ssh.cert(id).exists(),
            ssh.key().with_file_name(format!("{id}-cert.json")).exists(),
            metadata,
        )
    }

    fn labels() -> Vec<String> {
        EndpointCatalog::load_profiles()
            .unwrap()
            .into_iter()
            .map(|profile| profile.label)
            .collect()
    }

    #[test]
    fn confirmation_names_the_machine_permanent_deletion_and_removed_remotes() {
        let text = delete_confirmation("box", &["box", "box (2)"]);
        assert!(text.contains("'box'"));
        assert!(text.contains("disks and snapshots are permanently deleted"));
        assert!(text.contains("cannot be undone"));
        assert!(text.contains("remotes 'box', 'box (2)'"));
        assert!(delete_confirmation("orphan", &[]).contains("No Herdr remote uses it"));
    }

    #[test]
    fn deleting_a_machine_removes_every_profile_and_binding_for_it() {
        with_remotes("success", |profiles| {
            use crate::hangar::api::fake::*;
            let http = FakeHttp::new();
            http.reply(202, operation("op_1", "delete", "running"))
                .reply(200, operation("op_1", "delete", "succeeded"));
            let stopped = std::cell::RefCell::new(Vec::new());
            let message = delete_via(&http, &profiles[0], &stopped).unwrap();
            assert!(
                message.contains("Deleted hangar machine 'box'"),
                "{message}"
            );
            assert!(message.contains("'box (2)'"), "{message}");
            assert_eq!(*stopped.borrow(), ["box", "box (2)"]);
            let sent = http.sent();
            assert_eq!(sent[0].method, "DELETE");
            assert!(sent[0].url.ends_with(&format!("/v1/machines/{ID}")));
            assert!(sent[0].idempotency_key.is_some());
            assert_eq!(labels(), ["other", "plain"]);
            let prefs = LocationPreferences::load().unwrap();
            assert!(prefs.remotes.values().all(|options| options
                .cloud
                .as_ref()
                .is_none_or(|cloud| !cloud.same_machine(&hangar()))));
            assert_eq!(prefs.remotes.len(), 2);
            assert_eq!(prefs.default_profile, None);
            assert_eq!(
                machine_files(ID, &[&profiles[0], &profiles[1]]),
                (false, false, false)
            );
            assert_eq!(
                machine_files("m_bbbbbbbbbbbbbbbbbbbbbbbbbb", &[&profiles[2]]),
                (true, true, true)
            );
        });
    }

    #[test]
    fn a_machine_already_gone_from_hangar_is_cleaned_up_locally() {
        with_remotes("gone", |profiles| {
            use crate::hangar::api::fake::*;
            let http = FakeHttp::new();
            http.error(404, "not_found").error(404, "not_found");
            let stopped = std::cell::RefCell::new(Vec::new());
            let message = delete_via(&http, &profiles[1], &stopped).unwrap();
            assert!(message.contains("already deleted"), "{message}");
            assert_eq!(labels(), ["other", "plain"]);
            assert_eq!(
                http.paths(),
                [
                    format!("DELETE /v1/machines/{ID}"),
                    format!("GET /v1/machines/{ID}")
                ]
            );
            assert_eq!(
                machine_files(ID, &[&profiles[0], &profiles[1]]),
                (false, false, false)
            );
        });
    }

    #[test]
    fn a_failed_delete_keeps_every_local_entry() {
        with_remotes("failed", |profiles| {
            use crate::hangar::api::fake::*;
            let http = FakeHttp::new();
            http.error(500, "internal");
            let stopped = std::cell::RefCell::new(Vec::new());
            let error = delete_via(&http, &profiles[0], &stopped).unwrap_err();
            assert!(error.contains("Nothing was removed from Herdr"), "{error}");
            assert_eq!(labels(), ["box", "box (2)", "other", "plain"]);
            let prefs = LocationPreferences::load().unwrap();
            assert_eq!(prefs.binding(&profiles[0].id), Some(&hangar()));
            assert_eq!(prefs.binding(&profiles[1].id), Some(&hangar()));
            assert_eq!(prefs.default_profile.as_ref(), Some(&profiles[1].id));
            // Like Stop machine, the remotes stay disabled until Start remote.
            assert!(EndpointCatalog::load_profiles()
                .unwrap()
                .iter()
                .filter(|profile| profile.label.starts_with("box"))
                .all(|profile| !profile.enabled));
            assert_eq!(
                machine_files(ID, &[&profiles[0], &profiles[1]]),
                (true, true, true)
            );
        });
    }

    #[test]
    fn suspend_disables_every_bound_remote_before_suspending() {
        with_remotes("suspend", |profiles| {
            use crate::hangar::api::fake::*;
            let http = FakeHttp::new();
            http.reply(202, operation("op_1", "suspend", "succeeded"));
            let client = client(&http);
            let options = LocationPreferences::load().unwrap().remotes[&profiles[0].id].clone();
            let message = suspend_remote_with(&profiles[0], &options, |binding, progress| {
                // The fence is persisted before hangar is asked to suspend.
                assert!(EndpointCatalog::load_profiles()
                    .unwrap()
                    .iter()
                    .filter(|profile| profile.label.starts_with("box"))
                    .all(|profile| !profile.enabled));
                hangar::suspend_with(&client, &binding.machine_id, progress)
            })
            .unwrap();
            assert!(message.contains("suspended"));
            assert_eq!(http.paths(), [format!("POST /v1/machines/{ID}/suspend")]);
            assert_eq!(labels().len(), 4, "suspend never removes remotes");
        });
    }

    #[test]
    fn remove_profile_never_orphans_a_hangar_machine() {
        with_remotes("remove-profile", |profiles| {
            let prefs = LocationPreferences::load().unwrap();
            let error = remove_remote(&profiles[0], &prefs.remotes[&profiles[0].id]).unwrap_err();
            assert!(error.contains("Remove remote"), "{error}");
            assert_eq!(labels().len(), 4);
            remove_remote(&profiles[3], &prefs.remotes[&profiles[3].id]).unwrap();
            assert_eq!(labels(), ["box", "box (2)", "other"]);
        });
    }

    #[test]
    fn machine_identity_decides_which_profiles_share_a_lifecycle() {
        let other_server =
            CloudBinding::Hangar(HangarBinding::new("https://elsewhere.test", ID, "box").unwrap());
        assert!(hangar().same_machine(&hangar()));
        assert!(!hangar().same_machine(&other_server));
    }
}
