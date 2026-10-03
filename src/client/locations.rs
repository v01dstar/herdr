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

/// Explicit Start remote: starts the hangar machine first when the remote has one.
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

/// Removes the profile and its binding. The machine and its disk are kept.
pub(super) fn remove_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    validate_binding(profile, options.cloud.as_ref())?;
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

    #[test]
    fn machine_identity_decides_which_profiles_share_a_lifecycle() {
        let other_server =
            CloudBinding::Hangar(HangarBinding::new("https://elsewhere.test", ID, "box").unwrap());
        assert!(hangar().same_machine(&hangar()));
        assert!(!hangar().same_machine(&other_server));
    }
}
