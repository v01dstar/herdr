//! Client-owned defaults and provider bindings. Keep these out of the v1 SSH catalog
//! and the frozen runtime codecs: a server does not own another machine's locations.
use std::collections::BTreeMap;
use std::io::Read as _;

use serde::{Deserialize, Serialize};

use super::endpoint::{EndpointCatalog, ProfileId, SavedSshEndpoint};

pub(super) mod instacloud;
pub(super) use instacloud::{CloudOperation, CloudTarget};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoteOptions {
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub cloud: Option<CloudTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LocationPreferences {
    version: u32,
    pub default_profile: Option<ProfileId>,
    pub remotes: BTreeMap<ProfileId, RemoteOptions>,
}

impl Default for LocationPreferences {
    fn default() -> Self {
        Self {
            version: 1,
            default_profile: None,
            remotes: BTreeMap::new(),
        }
    }
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
        let prefs: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid remote locations: {error}"))?;
        prefs.validate()?;
        Ok(prefs)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.remotes.len() > 64 {
            return Err("Unsupported remote locations file".into());
        }
        for (id, options) in &self.remotes {
            ProfileId::parse(id.to_string())?;
            options.validate()?;
        }
        if let Some(id) = &self.default_profile {
            ProfileId::parse(id.to_string())?;
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
}

impl RemoteOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.cwd.len() > 4096 || self.cwd.chars().any(char::is_control) {
            return Err("Directory must be at most 4096 bytes with no control characters".into());
        }
        if let Some(cloud) = &self.cloud {
            cloud.validate()?;
        }
        Ok(())
    }
}

/// One bounded operation across attached clients at a time for the demo. Keeping
/// the open file holds the OS lock; process exit releases it without stale leases.
pub(super) fn operation_lock() -> Result<std::fs::File, String> {
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
    file.try_lock().map_err(|_| {
        "Another client is managing remotes. Wait for that operation to finish.".to_owned()
    })?;
    Ok(file)
}

pub(super) fn same_destination(left: &SavedSshEndpoint, right: &SavedSshEndpoint) -> bool {
    left.id == right.id && left.target == right.target && left.session == right.session
}

pub(super) fn validate_binding(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudTarget>,
) -> Result<(), String> {
    let profiles = EndpointCatalog::load_profiles()?;
    let prefs = LocationPreferences::load()?;
    validate_binding_in(profile, cloud, &profiles, &prefs)
}

fn validate_binding_in(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudTarget>,
    profiles: &[SavedSshEndpoint],
    prefs: &LocationPreferences,
) -> Result<(), String> {
    if !profiles.iter().any(|p| same_destination(p, profile))
        || prefs
            .remotes
            .get(&profile.id)
            .and_then(|o| o.cloud.as_ref())
            != cloud
    {
        return Err("Remote was removed or changed in another client. Close and reopen this dialog before continuing.".into());
    }
    Ok(())
}

pub(super) fn set_service_enabled(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudTarget>,
    enabled: bool,
) -> Result<(), String> {
    validate_binding(profile, cloud)?;
    let mut catalog = EndpointCatalog::load()?;
    let prefs = LocationPreferences::load()?;
    let ids = catalog
        .ssh
        .iter()
        .filter(|p| {
            p.id == profile.id
                || cloud.is_some_and(|target| {
                    prefs.remotes.get(&p.id).and_then(|o| o.cloud.as_ref()) == Some(target)
                })
        })
        .map(|p| p.id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        catalog.set_enabled(&id, enabled);
    }
    catalog.store_profiles()
}

pub(super) fn cloud_operation(
    target: &CloudTarget,
    operation: CloudOperation,
) -> Result<String, String> {
    instacloud::operate(target, operation)
}

/// Explicitly starting an installed session never installs or replaces a remote binary.
pub(super) fn start_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    validate_binding(profile, options.cloud.as_ref())?;
    let _resource_guard = options
        .cloud
        .as_ref()
        .map(instacloud::provisioning::resource_lock)
        .transpose()?;
    if let Some(cloud) = &options.cloud {
        instacloud::operate(cloud, CloudOperation::Start)?;
    }
    crate::remote::start_saved_ssh(&profile.target, &profile.session)
        .map_err(|error| error.to_string())?;
    set_service_enabled(profile, options.cloud.as_ref(), true)
}

pub(super) fn stop_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("Stop compute requires an Instacloud binding")?;
    let _guard = operation_lock()?;
    validate_binding(profile, Some(cloud))?;
    let _resource_guard = instacloud::provisioning::resource_lock(cloud)?;
    instacloud::inventory::verify_identity(cloud)?;
    // Persist the fence before either SSH or provider I/O. All clients observe the same
    // disabled profiles, and reconnect never invokes the provider's start operation.
    set_service_enabled(profile, options.cloud.as_ref(), false)?;
    let catalog = EndpointCatalog::load()?;
    let prefs = LocationPreferences::load()?;
    let mut warnings = Vec::new();
    for other in &catalog.ssh {
        if other.id == profile.id
            || options.cloud.as_ref().is_some_and(|cloud| {
                prefs.remotes.get(&other.id).and_then(|o| o.cloud.as_ref()) == Some(cloud)
            })
        {
            if let Err(error) = crate::remote::stop_saved_ssh(&other.target, &other.session) {
                warnings.push(format!("{}: {error}", other.label));
            }
        }
    }
    let message = instacloud::operate(cloud, CloudOperation::Stop)?;
    if warnings.is_empty() {
        Ok(message)
    } else {
        Ok(format!(
            "{message}. Graceful shutdown warnings: {}",
            warnings.join("; ")
        ))
    }
}

pub(super) fn create_workspace(
    profile: Option<&SavedSshEndpoint>,
    options: &RemoteOptions,
    cwd: String,
    label: String,
) -> Result<String, String> {
    let _guard = operation_lock()?;
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
        let cloud = CloudTarget {
            project: "p".into(),
            branch: "main".into(),
            service: "worker".into(),
            service_id: None,
        };
        assert!(validate_binding_in(
            &profile,
            Some(&cloud),
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
                cloud: None,
            },
        );
        let encoded = serde_json::to_value(&profile).unwrap();
        assert!(encoded.get("cwd").is_none());
        assert!(encoded.get("cloud").is_none());
        assert!(prefs.validate().is_ok());
    }
}
