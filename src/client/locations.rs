//! Client-owned defaults and provider bindings. Keep these out of the v1 SSH catalog
//! and the frozen runtime codecs: a server does not own another machine's locations.
use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::endpoint::{EndpointCatalog, ProfileId, SavedSshEndpoint};
use crate::hangar::binding::HangarBinding;

pub(crate) mod hangar;
mod remotes;
pub(crate) mod sync;

pub(crate) use remotes::{effective_catalog, effective_profiles, Remotes};

const VERSION: u32 = 3;
const MAX_REMOTES: usize = 64;
const MAX_MACHINE_PREFS: usize = 256;
const LOCK_WAIT: Duration = Duration::from_secs(3);

/// A remote whose machine lifecycle belongs to a provider. Only the in-memory view of
/// a hangar machine carries one; `locations.json` stores per-machine preferences.
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

    /// The same machine on the same server (its name may have changed).
    pub(crate) fn same_machine(&self, other: &Self) -> bool {
        let (left, right) = (self.hangar(), other.hangar());
        left.machine_id == right.machine_id && left.server == right.server
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteOptions {
    #[serde(default)]
    pub cwd: String,
    /// Set only in the in-memory view of a hangar machine (and in version 2 files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudBinding>,
}

/// Local preferences for one hangar machine, keyed by (server, machine ID). The machine
/// itself comes from hangar; these are dropped once hangar no longer lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MachinePrefs {
    pub server: String,
    pub machine_id: String,
    /// The endpoint ID a migrated remote already used, so its session state survives.
    /// Machines without one use an ID derived from (server, machine ID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<ProfileId>,
    /// The Herdr session on the machine when it is not the template's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Default directory for new workspaces.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// Not shown in the sidebar and not connected; still listed in Settings → Remotes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Shown instead of the hangar machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl MachinePrefs {
    fn new(server: &str, machine_id: &str) -> Self {
        Self {
            server: server.to_owned(),
            machine_id: machine_id.to_owned(),
            ..Default::default()
        }
    }

    /// Nothing differs from a machine without preferences.
    fn is_empty(&self) -> bool {
        self.profile_id.is_none()
            && self.session.is_none()
            && self.cwd.is_empty()
            && !self.hidden
            && self.display_name.is_none()
    }

    fn validate(&self) -> Result<(), String> {
        HangarBinding::new(&self.server, &self.machine_id, "")?;
        if let Some(id) = &self.profile_id {
            ProfileId::parse(id.to_string())?;
        }
        if let Some(session) = &self.session {
            crate::session::validate_name(session)?;
        }
        if let Some(name) = &self.display_name {
            validate_display_name(name)?;
        }
        RemoteOptions {
            cwd: self.cwd.clone(),
            cloud: None,
        }
        .validate()
    }
}

pub(crate) fn validate_display_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > super::endpoint::MAX_LABEL_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(format!(
            "Names must be 1–{} bytes with no control characters",
            super::endpoint::MAX_LABEL_BYTES
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocationPreferences {
    version: u32,
    pub default_profile: Option<ProfileId>,
    /// SSH remotes' options (default directory), keyed by their saved profile.
    pub remotes: BTreeMap<ProfileId, RemoteOptions>,
    /// Preferences of hangar machines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hangar: Vec<MachinePrefs>,
    /// Shown once in the remotes dialog, then cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// An in-memory merge with hangar machines (`Remotes::view_prefs`): never stored.
    #[serde(skip)]
    view: bool,
}

impl Default for LocationPreferences {
    fn default() -> Self {
        Self {
            version: VERSION,
            default_profile: None,
            remotes: BTreeMap::new(),
            hangar: Vec::new(),
            notice: None,
            view: false,
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

/// Version 2 saved each hangar machine as an SSH profile plus a binding here.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2Preferences {
    #[allow(dead_code)] // Checked before parsing; kept so the shape stays exact.
    version: u32,
    default_profile: Option<ProfileId>,
    remotes: BTreeMap<ProfileId, RemoteOptions>,
    #[serde(default)]
    notice: Option<String>,
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
        default_profile: old.default_profile,
        remotes,
        notice: (!disabled.is_empty()).then(|| INSTACLOUD_NOTICE.to_owned()),
        ..Default::default()
    };
    prefs.validate()?;
    Ok((prefs, disabled))
}

/// What the version 2 → 3 migration writes.
#[derive(Debug)]
struct V2Migration {
    prefs: LocationPreferences,
    /// Last known machines, so hangar remotes stay listed (and connected) until the
    /// first sync answers. Running when its profile was enabled, else unknown.
    seed: Vec<(String, sync::CachedMachine)>,
    /// Saved SSH profiles that became hangar machines.
    moved: Vec<ProfileId>,
}

/// Each hangar binding becomes preferences for its machine that keep the profile's ID
/// (the endpoint ID of its workspaces), session, default directory and label. When
/// several profiles were bound to one machine, the default (else the first saved) one
/// becomes the machine; the others stay plain SSH remotes of its alias.
fn migrate_v2(old: V2Preferences, profiles: &[SavedSshEndpoint]) -> Result<V2Migration, String> {
    let position = |id: &ProfileId| {
        profiles
            .iter()
            .position(|profile| &profile.id == id)
            .unwrap_or(usize::MAX)
    };
    let mut bound = old
        .remotes
        .iter()
        .filter_map(|(id, options)| Some((id, options, options.cloud.as_ref()?.hangar())))
        .collect::<Vec<_>>();
    bound.sort_by_key(|(id, _, _)| (old.default_profile.as_ref() != Some(*id), position(id)));
    let mut prefs = LocationPreferences {
        default_profile: old.default_profile.clone(),
        notice: old.notice,
        ..Default::default()
    };
    let mut seed = Vec::new();
    let mut moved = Vec::new();
    for (id, options, binding) in bound {
        let profile = profiles.iter().find(|profile| &profile.id == id);
        if prefs
            .machine(&binding.server, &binding.machine_id)
            .is_some()
        {
            if profile.is_some() {
                prefs.remotes.insert(
                    id.clone(),
                    RemoteOptions {
                        cwd: options.cwd.clone(),
                        cloud: None,
                    },
                );
            }
            continue;
        }
        let mut entry = MachinePrefs::new(&binding.server, &binding.machine_id);
        entry.profile_id = Some(id.clone());
        entry.cwd = options.cwd.clone();
        if let Some(profile) = profile {
            entry.session = (profile.session != hangar::SESSION).then(|| profile.session.clone());
            entry.display_name =
                (profile.label != binding.machine_name).then(|| profile.label.clone());
            moved.push(id.clone());
        }
        let name = if binding.machine_name.is_empty() {
            binding.machine_id.clone()
        } else {
            binding.machine_name.clone()
        };
        seed.push((
            binding.server.clone(),
            sync::CachedMachine {
                id: binding.machine_id.clone(),
                name,
                state: if profile.is_some_and(|profile| profile.enabled) {
                    crate::hangar::api::MachineState::Running
                } else {
                    crate::hangar::api::MachineState::Unknown
                },
                fence_until_ms: 0,
            },
        ));
        prefs.hangar.push(entry);
    }
    for (id, options) in old.remotes {
        if options.cloud.is_none() {
            prefs.remotes.insert(id, options);
        }
    }
    prefs.validate()?;
    Ok(V2Migration { prefs, seed, moved })
}

/// Unit tests never rewrite a developer's real state directory: they migrate in memory
/// unless the test set its own `XDG_STATE_HOME`.
fn migration_persists() -> bool {
    !cfg!(test) || std::env::var_os("XDG_STATE_HOME").is_some()
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
        match version {
            Some(1) => return Self::migrate(&bytes),
            Some(2) => return Self::migrate_from_v2(&bytes),
            _ => {}
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
        if !migration_persists() {
            return Ok(prefs);
        }
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
            "migrated remote locations to version 3"
        );
        Ok(prefs)
    }

    /// One-time upgrade from saved hangar profiles to server-sourced machines. Writes the
    /// seed list, then the preferences, then removes the moved profiles from the SSH
    /// catalog. Each step is idempotent; profiles left by an interrupted run are hidden
    /// by their ID and removed by the next sync. Needs no lock, like `migrate`.
    fn migrate_from_v2(bytes: &[u8]) -> Result<Self, String> {
        let old: V2Preferences = serde_json::from_slice(bytes)
            .map_err(|error| format!("Invalid remote locations: {error}"))?;
        let profiles = EndpointCatalog::load_profiles()?;
        let migration = migrate_v2(old, &profiles)?;
        if !migration_persists() {
            return Ok(migration.prefs);
        }
        if !migration.seed.is_empty() {
            let mut cache = sync::MachineCache::load().unwrap_or_default();
            for (server, machine) in &migration.seed {
                let entry = cache.servers.entry(server.clone()).or_default();
                if !entry.machines.iter().any(|known| known.id == machine.id) {
                    entry.machines.push(machine.clone());
                }
            }
            cache.store()?;
        }
        migration.prefs.store()?;
        if !migration.moved.is_empty() {
            let mut catalog = EndpointCatalog::load()?;
            for id in &migration.moved {
                catalog.remove_ssh(id);
            }
            catalog.store_profiles()?;
        }
        tracing::info!(
            machines = migration.prefs.hangar.len(),
            "migrated hangar remotes to server-sourced machines"
        );
        Ok(migration.prefs)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION
            || self.remotes.len() > MAX_REMOTES
            || self.hangar.len() > MAX_MACHINE_PREFS
        {
            return Err("Unsupported remote locations file".into());
        }
        for (id, options) in &self.remotes {
            ProfileId::parse(id.to_string())?;
            options.validate()?;
            if options.cloud.is_some() && !self.view {
                return Err("Unsupported remote locations file".into());
            }
        }
        for (index, entry) in self.hangar.iter().enumerate() {
            entry.validate()?;
            if self.hangar[..index]
                .iter()
                .any(|other| other.server == entry.server && other.machine_id == entry.machine_id)
            {
                return Err("Duplicate hangar machine preferences".into());
            }
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
        if self.view {
            return Err("Remote settings view cannot be stored".into());
        }
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

    pub fn machine(&self, server: &str, machine_id: &str) -> Option<&MachinePrefs> {
        self.hangar
            .iter()
            .find(|entry| entry.server == server && entry.machine_id == machine_id)
    }

    /// Edits one machine's preferences, dropping an entry that no longer differs from
    /// the defaults (unless it carries a migrated endpoint ID).
    pub fn edit_machine(
        &mut self,
        server: &str,
        machine_id: &str,
        edit: impl FnOnce(&mut MachinePrefs),
    ) -> Result<(), String> {
        let index = match self
            .hangar
            .iter()
            .position(|entry| entry.server == server && entry.machine_id == machine_id)
        {
            Some(index) => index,
            None => {
                if self.hangar.len() >= MAX_MACHINE_PREFS {
                    return Err("Too many hangar machine preferences".into());
                }
                self.hangar.push(MachinePrefs::new(server, machine_id));
                self.hangar.len() - 1
            }
        };
        edit(&mut self.hangar[index]);
        self.hangar[index].validate()?;
        if self.hangar[index].is_empty() {
            self.hangar.remove(index);
        }
        Ok(())
    }

    /// Forgets one machine's preferences. Returns whether anything changed.
    pub fn remove_machine(&mut self, server: &str, machine_id: &str) -> bool {
        let Some(index) = self
            .hangar
            .iter()
            .position(|entry| entry.server == server && entry.machine_id == machine_id)
        else {
            return false;
        };
        let entry = self.hangar.remove(index);
        let id = sync::profile_id(Some(&entry), server, machine_id);
        if self.default_profile.as_ref() == Some(&id) {
            self.default_profile = None;
        }
        true
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

/// The hangar machine reached through `target` (`hangar-<machineId>`), from the synced
/// list.
pub(crate) fn hangar_binding_for_target(target: &str) -> Result<Option<HangarBinding>, String> {
    Ok(sync::MachineCache::load()?.binding_for_alias(target))
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

fn same_binding(left: Option<&CloudBinding>, right: Option<&CloudBinding>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same_machine(right),
        _ => false,
    }
}

pub(super) fn validate_binding(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudBinding>,
) -> Result<(), String> {
    let remotes = Remotes::load()?;
    validate_binding_in(
        profile,
        cloud,
        &remotes.profiles(true),
        &remotes.view_prefs(),
    )
}

fn validate_binding_in(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudBinding>,
    profiles: &[SavedSshEndpoint],
    prefs: &LocationPreferences,
) -> Result<(), String> {
    if !profiles.iter().any(|p| same_destination(p, profile))
        || !same_binding(prefs.binding(&profile.id), cloud)
    {
        return Err("Remote was removed or changed in another client. Close and reopen this dialog before continuing.".into());
    }
    Ok(())
}

/// Enables or disables automatic connection. An SSH remote's enabled flag is saved in
/// its profile. A hangar machine connects while it is running; disabling fences it so
/// it does not reconnect while Herdr stops, suspends or deletes it, and enabling lifts
/// the fence.
pub(super) fn set_service_enabled(
    profile: &SavedSshEndpoint,
    cloud: Option<&CloudBinding>,
    enabled: bool,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    validate_binding(profile, cloud)?;
    if let Some(cloud) = cloud {
        return sync::set_fence_locked(cloud.hangar(), !enabled);
    }
    let mut catalog = EndpointCatalog::load()?;
    catalog.set_enabled(&profile.id, enabled);
    catalog.store_profiles()
}

/// Shows or hides a hangar machine in the sidebar. A hidden machine is not connected
/// but stays listed in Settings → Remotes.
pub(crate) fn set_hidden(binding: &HangarBinding, hidden: bool) -> Result<(), String> {
    let _guard = operation_lock()?;
    let remotes = Remotes::load()?;
    if remotes
        .machine(&binding.server, &binding.machine_id)
        .is_none()
    {
        return Err(format!(
            "hangar machine '{}' is no longer listed",
            binding.machine_name
        ));
    }
    let mut prefs = remotes.prefs;
    prefs.edit_machine(&binding.server, &binding.machine_id, |entry| {
        entry.hidden = hidden;
    })?;
    prefs.store()
}

/// Edits a hangar machine's local name, session and default directory. A name equal to
/// the hangar name (or empty) clears the override.
pub(crate) fn edit_machine_prefs(
    binding: &HangarBinding,
    name: Option<&str>,
    session: Option<&str>,
    cwd: Option<&str>,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    let remotes = Remotes::load()?;
    let Some(remote) = remotes.machine(&binding.server, &binding.machine_id) else {
        return Err(format!(
            "hangar machine '{}' is no longer listed",
            binding.machine_name
        ));
    };
    let machine_name = remote.binding.machine_name.clone();
    let mut prefs = remotes.prefs;
    prefs.edit_machine(&binding.server, &binding.machine_id, |entry| {
        if let Some(name) = name.map(str::trim) {
            entry.display_name =
                (!name.is_empty() && name != machine_name).then(|| name.to_owned());
        }
        if let Some(session) = session.map(str::trim) {
            entry.session =
                (!session.is_empty() && session != hangar::SESSION).then(|| session.to_owned());
        }
        if let Some(cwd) = cwd {
            entry.cwd = cwd.trim().to_owned();
        }
    })?;
    prefs.store()
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
        let machine = hangar::start_machine(cloud.hangar(), &mut |_| {})
            .map_err(|error| error.to_string())?;
        sync::record_state(cloud.hangar(), machine.state)?;
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
    let warnings = quiesce_machine(profile, cloud)?;
    hangar::stop_machine(cloud.hangar(), &mut |_| {}).map_err(|error| error.to_string())?;
    record_state_best_effort(cloud.hangar(), crate::hangar::api::MachineState::Stopped);
    let message = format!("{}: stopped", cloud.hangar().machine_name);
    Ok(with_warnings(message, &warnings))
}

fn record_state_best_effort(binding: &HangarBinding, state: crate::hangar::api::MachineState) {
    if let Err(error) = sync::record_state(binding, state) {
        tracing::debug!(%error, "could not record a hangar machine state; the next sync will");
    }
}

fn with_warnings(message: String, warnings: &[String]) -> String {
    if warnings.is_empty() {
        message
    } else {
        format!(
            "{message}. Graceful shutdown warnings: {}",
            warnings.join("; ")
        )
    }
}

/// Fences the machine against reconnecting, then asks its Herdr session to stop.
/// Returns graceful-shutdown warnings.
fn quiesce_machine(
    profile: &SavedSshEndpoint,
    cloud: &CloudBinding,
) -> Result<Vec<String>, String> {
    // Persist the fence before SSH or provider I/O: every client observes it, and
    // reconnect never starts the machine again.
    set_service_enabled(profile, Some(cloud), false)?;
    let mut warnings = Vec::new();
    if let Err(error) = crate::remote::stop_saved_ssh(&profile.target, &profile.session) {
        warnings.push(format!("{}: {error}", profile.label));
    }
    Ok(warnings)
}

/// Save as image…: brings the machine to a stopped, uploaded state as `plan` says
/// (the stop is the same as Stop machine), then saves its root disk as an image.
pub(super) fn save_image_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
    plan: hangar::SavePlan,
    name: &str,
    description: &str,
) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("Save as image requires a hangar machine")?;
    validate_binding(profile, Some(cloud))?;
    hangar::validate_image_name(name)?;
    let mut warnings = Vec::new();
    let image = hangar::save_image(
        cloud.hangar(),
        plan,
        &crate::hangar::api::CreateImageRequest { name, description },
        &mut || {
            warnings = quiesce_machine(profile, cloud)?;
            Ok(())
        },
        &mut |_| {},
    )
    .map_err(|error| {
        format!(
            "Could not save image {name}: {}",
            hangar::image_error(&error)
        )
    })?;
    let machine = &cloud.hangar().machine_name;
    let mut message = format!(
        "Saved image {} from {machine}. Choose it as Source in Add remote → Create new machine.",
        image.name
    );
    if plan != hangar::SavePlan::Save {
        record_state_best_effort(cloud.hangar(), crate::hangar::api::MachineState::Stopped);
        message.push_str(&format!(
            " {machine} stays stopped; use Start remote to work on it again."
        ));
    }
    Ok(with_warnings(message, &warnings))
}

/// Fork machine…: brings the machine to a stopped, uploaded state as `plan` says (the
/// stop is the same as Stop machine), forks it, then lists and connects the running
/// fork as Create new machine does. The source stays stopped.
pub(super) fn fork_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
    plan: hangar::SavePlan,
    name: &str,
) -> Result<String, String> {
    let cloud = options
        .cloud
        .as_ref()
        .ok_or("Fork machine requires a hangar machine")?;
    validate_binding(profile, Some(cloud))?;
    hangar::validate_fork_name(name)?;
    let mut warnings = Vec::new();
    let mut progress = |_| {};
    let fork = hangar::fork_machine(
        cloud.hangar(),
        plan,
        name,
        &mut || {
            warnings = quiesce_machine(profile, cloud)?;
            Ok(())
        },
        &mut progress,
    )
    .map_err(|error| format!("Could not fork into {name}: {}", hangar::fork_error(&error)))?;
    if plan != hangar::SavePlan::Save {
        record_state_best_effort(cloud.hangar(), crate::hangar::api::MachineState::Stopped);
    }
    let source = &cloud.hangar().machine_name;
    let added = hangar::adopt_machine(&cloud.hangar().server, &fork, &mut progress)
        .map_err(|error| format!("Forked {source} into {}. {error}", fork.name))?;
    let message = format!(
        "Forked {source} into {}. {added} {source} stays stopped; use Start remote to work on it again.",
        fork.name
    );
    Ok(with_warnings(message, &warnings))
}

/// Confirmation shown before an image is deleted.
pub(crate) fn image_delete_confirmation(name: &str) -> String {
    format!(
        "Delete image '{name}'? New machines can no longer be created from it. Machines already created from it are not affected; they keep their own disks. This cannot be undone."
    )
}

pub(super) fn delete_image(server: &str, id: &str, name: &str) -> Result<String, String> {
    match hangar::delete_image(server, id) {
        Ok(hangar::Deletion::Deleted) => Ok(format!("Deleted image {name}.")),
        Ok(hangar::Deletion::AlreadyGone) => Ok(format!("Image {name} was already deleted.")),
        Err(error) => Err(format!("Could not delete image {name}: {error}")),
    }
}

/// Suspend remote: fences the machine (so no client fights the gateway dropping its
/// connections), then snapshots its memory. Unlike Stop machine the Herdr server is not
/// shut down; its sessions and programs continue after Resume.
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
    record_state_best_effort(cloud.hangar(), crate::hangar::api::MachineState::Suspended);
    Ok(format!(
        "{}: suspended. Running programs resume with Resume remote.",
        cloud.hangar().machine_name
    ))
}

/// Removes an SSH profile and its location metadata. A hangar machine is listed by
/// hangar: it goes away only when it is deleted (`delete_remote`), or is hidden.
pub(super) fn remove_remote(
    profile: &SavedSshEndpoint,
    options: &RemoteOptions,
) -> Result<(), String> {
    let _guard = operation_lock()?;
    validate_binding(profile, options.cloud.as_ref())?;
    if let Some(cloud) = &options.cloud {
        return Err(hangar_remove_refusal(&cloud.hangar().machine_name));
    }
    let mut catalog = EndpointCatalog::load()?;
    catalog.remove_ssh(&profile.id);
    catalog.store_profiles()?;
    remove_binding(&profile.id)
}

/// Why a hangar machine cannot be removed like an SSH remote.
pub(crate) fn hangar_remove_refusal(machine_name: &str) -> String {
    format!(
        "'{machine_name}' is a hangar machine, listed from your hangar account; it cannot be removed from Herdr alone. Delete machine… deletes it on hangar; Hide from sidebar keeps it but stops showing and connecting it."
    )
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

/// What deleting a hangar machine destroys.
pub(crate) fn delete_consequences() -> String {
    "The machine, its disks and snapshots are permanently deleted, with every file and process on it. This cannot be undone. Herdr closes its workspaces and stops listing it.".to_owned()
}

/// Confirmation shown before a hangar machine is deleted.
pub(crate) fn delete_confirmation(machine_name: &str) -> String {
    format!(
        "Delete hangar machine '{machine_name}'? {}",
        delete_consequences()
    )
}

/// Deletes the hangar machine behind `profile`. Its preferences and local files are
/// forgotten only after hangar confirms the machine is gone.
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
        .ok_or("Delete machine requires a hangar machine; use Remove remote")?;
    // Same fence as Stop machine: no client reconnects to a machine being deleted.
    set_service_enabled(profile, Some(cloud), false)?;
    progress(format!("Stopping Herdr on {}…", profile.label));
    // The machine is deleted next; a session that cannot stop cleanly goes with it.
    if let Err(error) = stop_session(profile) {
        tracing::debug!(%error, label = %profile.label, "graceful stop before delete failed");
    }
    let name = &cloud.hangar().machine_name;
    let deletion = delete(cloud.hangar(), progress).map_err(|error| {
        let error = error.to_string();
        let error = error.trim_end_matches('.');
        format!(
            "Could not delete '{name}': {error}. It stays listed and does not reconnect for a few minutes. Retry Delete machine…, or use Start remote to keep using it."
        )
    })?;
    sync::forget_machine(cloud.hangar()).map_err(|error| {
        format!("hangar machine '{name}' was deleted, but Herdr could not update its list: {error}. It disappears with the next sync.")
    })?;
    Ok(match deletion {
        hangar::Deletion::Deleted => format!("Deleted hangar machine '{name}'."),
        hangar::Deletion::AlreadyGone => format!("hangar machine '{name}' was already deleted."),
    })
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

pub(super) fn create_workspace(
    profile: Option<&SavedSshEndpoint>,
    options: &RemoteOptions,
    cwd: String,
    label: String,
) -> Result<String, String> {
    if let Some(profile) = profile {
        validate_binding(profile, options.cloud.as_ref())?;
        if !Remotes::load()?
            .profiles(true)
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
    use crate::hangar::api::MachineState;

    const ID: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";
    const OTHER: &str = "m_bbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SERVER: &str = "https://hangar.test";

    fn hangar() -> CloudBinding {
        CloudBinding::Hangar(HangarBinding::new(SERVER, ID, "box").unwrap())
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
    fn a_machine_renamed_on_hangar_still_authorizes_its_remote() {
        let profile = SavedSshEndpoint::new("box", "hangar-x", "herdr-remote").unwrap();
        let mut prefs = LocationPreferences {
            view: true,
            ..Default::default()
        };
        let renamed = CloudBinding::Hangar(HangarBinding::new(SERVER, ID, "renamed").unwrap());
        prefs.remotes.insert(
            profile.id.clone(),
            RemoteOptions {
                cwd: String::new(),
                cloud: Some(renamed),
            },
        );
        assert!(validate_binding_in(
            &profile,
            Some(&hangar()),
            std::slice::from_ref(&profile),
            &prefs
        )
        .is_ok());
        let elsewhere =
            CloudBinding::Hangar(HangarBinding::new("https://elsewhere.test", ID, "box").unwrap());
        assert!(validate_binding_in(
            &profile,
            Some(&elsewhere),
            std::slice::from_ref(&profile),
            &prefs
        )
        .is_err());
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
    fn stored_preferences_hold_machine_prefs_and_never_a_binding() {
        let profile = SavedSshEndpoint::new("remote", "host", "demo").unwrap();
        let encoded = serde_json::to_value(&profile).unwrap();
        assert!(encoded.get("cwd").is_none());
        assert!(encoded.get("cloud").is_none());
        let mut prefs = LocationPreferences::default();
        prefs.remotes.insert(
            profile.id.clone(),
            RemoteOptions {
                cwd: "/srv".into(),
                cloud: None,
            },
        );
        prefs
            .edit_machine(SERVER, ID, |entry| {
                entry.cwd = "/data/project".into();
                entry.hidden = true;
                entry.display_name = Some("Box".into());
            })
            .unwrap();
        let value = serde_json::to_value(&prefs).unwrap();
        assert_eq!(value["version"], 3);
        assert_eq!(value["hangar"][0]["server"], SERVER);
        assert_eq!(value["hangar"][0]["machine_id"], ID);
        assert_eq!(value["hangar"][0]["hidden"], true);
        assert!(value["hangar"][0].get("profile_id").is_none());
        assert!(value["remotes"][profile.id.as_str()].get("cloud").is_none());
        let decoded: LocationPreferences = serde_json::from_value(value).unwrap();
        assert!(decoded.validate().is_ok());
        assert_eq!(decoded.machine(SERVER, ID).unwrap().cwd, "/data/project");
        // A binding in a stored file is from an older format; only views carry one.
        let mut bound = prefs.clone();
        bound.remotes.get_mut(&profile.id).unwrap().cloud = Some(hangar());
        assert!(bound.validate().is_err());
        // Clearing every preference drops the entry.
        prefs
            .edit_machine(SERVER, ID, |entry| {
                entry.cwd.clear();
                entry.hidden = false;
                entry.display_name = None;
            })
            .unwrap();
        assert!(prefs.hangar.is_empty());
        assert!(prefs
            .edit_machine(SERVER, ID, |entry| entry.display_name =
                Some("\u{1b}".into()))
            .is_err());
    }

    #[test]
    fn a_view_of_the_preferences_cannot_be_stored() {
        let remotes = Remotes::build(
            Vec::new(),
            LocationPreferences::default(),
            &sync::MachineCache::default(),
            0,
        );
        assert!(remotes
            .view_prefs()
            .store()
            .unwrap_err()
            .contains("cannot be stored"));
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
        assert!(prefs.hangar.is_empty());
        let (quiet, none) = migrate_v1(
            serde_json::json!({"version": 1, "default_profile": null, "remotes": {}})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(none.is_empty());
        assert!(quiet.notice.is_none());
    }

    /// A version 2 state: `box` (default, renamed, own session and directory), a second
    /// profile of the same machine, a stopped `other` machine, and a plain SSH remote.
    fn v2_state() -> (Vec<SavedSshEndpoint>, serde_json::Value) {
        let alias = crate::hangar::binding::alias_for(ID);
        let mut other_profile = SavedSshEndpoint::new(
            "other",
            crate::hangar::binding::alias_for(OTHER),
            "herdr-remote",
        )
        .unwrap();
        other_profile.enabled = false;
        let profiles = vec![
            SavedSshEndpoint::new("box (work)", &alias, "agents").unwrap(),
            SavedSshEndpoint::new("box (2)", &alias, "herdr-remote").unwrap(),
            other_profile,
            SavedSshEndpoint::new("plain", "workbox", "default").unwrap(),
        ];
        let binding = |id: &str, name: &str| {
            serde_json::to_value(CloudBinding::Hangar(
                HangarBinding::new(SERVER, id, name).unwrap(),
            ))
            .unwrap()
        };
        let v2 = serde_json::json!({
            "version": 2,
            "default_profile": profiles[0].id,
            "remotes": {
                profiles[0].id.as_str(): {"cwd": "/data/work", "cloud": binding(ID, "box")},
                profiles[1].id.as_str(): {"cwd": "", "cloud": binding(ID, "box")},
                profiles[2].id.as_str(): {"cwd": "", "cloud": binding(OTHER, "other")},
                profiles[3].id.as_str(): {"cwd": "/srv", "cloud": null},
            }
        });
        (profiles, v2)
    }

    #[test]
    fn version_two_hangar_remotes_become_machine_prefs_with_the_same_endpoint_ids() {
        let (profiles, v2) = v2_state();
        let old: V2Preferences = serde_json::from_value(v2).unwrap();
        let migration = migrate_v2(old, &profiles).unwrap();
        let prefs = &migration.prefs;
        assert_eq!(prefs.version, VERSION);
        let boxed = prefs.machine(SERVER, ID).unwrap();
        assert_eq!(
            boxed.profile_id.as_ref(),
            Some(&profiles[0].id),
            "default wins"
        );
        assert_eq!(boxed.session.as_deref(), Some("agents"));
        assert_eq!(boxed.cwd, "/data/work");
        assert_eq!(boxed.display_name.as_deref(), Some("box (work)"));
        assert!(!boxed.hidden);
        let other = prefs.machine(SERVER, OTHER).unwrap();
        assert_eq!(other.profile_id.as_ref(), Some(&profiles[2].id));
        assert_eq!(other.session, None, "the template session is the default");
        assert_eq!(other.display_name, None, "the hangar name is the default");
        assert_eq!(prefs.default_profile.as_ref(), Some(&profiles[0].id));
        // The second profile of `box` stays a plain SSH remote of its alias.
        assert_eq!(
            migration.moved,
            [profiles[0].id.clone(), profiles[2].id.clone()]
        );
        assert!(prefs.remotes[&profiles[1].id].cloud.is_none());
        assert_eq!(prefs.remotes[&profiles[3].id].cwd, "/srv");
        assert!(prefs
            .remotes
            .values()
            .all(|options| options.cloud.is_none()));
        // Seeded so the remotes stay listed until the first sync.
        let seeded = migration
            .seed
            .iter()
            .map(|(server, machine)| (server.as_str(), machine.id.as_str(), machine.state))
            .collect::<Vec<_>>();
        assert_eq!(
            seeded,
            [
                (SERVER, ID, MachineState::Running),
                (SERVER, OTHER, MachineState::Unknown)
            ]
        );
    }

    /// Runs `f` with a private state directory.
    fn with_state<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = std::env::var_os("XDG_STATE_HOME");
        let base =
            std::env::temp_dir().join(format!("herdr-locations-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_STATE_HOME", &base);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match old {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&base);
        result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    fn write_locations(value: &serde_json::Value) {
        let path = LocationPreferences::path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    #[test]
    fn migrating_saved_state_keeps_endpoint_ids_selection_and_plain_ssh_remotes() {
        with_state("migrate-v2", || {
            let (profiles, v2) = v2_state();
            let mut catalog = EndpointCatalog::default();
            catalog.ssh = profiles.clone();
            assert!(catalog.select_ssh(&profiles[0].id));
            catalog.store_profiles().unwrap();
            catalog.store_selection().unwrap();
            write_locations(&v2);
            // Characterization: before the migration, the workspace endpoint of `box`
            // is its saved profile.
            let before = crate::client::endpoint::ClientEndpointId::Ssh(profiles[0].id.clone());

            let prefs = LocationPreferences::load().unwrap();
            assert_eq!(prefs.hangar.len(), 2);
            let stored: serde_json::Value =
                serde_json::from_slice(&std::fs::read(LocationPreferences::path()).unwrap())
                    .unwrap();
            assert_eq!(stored["version"], 3);
            // Only plain SSH remotes stay in the generation-1 catalog, unchanged.
            let saved = EndpointCatalog::load_profiles().unwrap();
            assert_eq!(saved, [profiles[1].clone(), profiles[3].clone()]);

            let remotes = Remotes::load().unwrap();
            let boxed = remotes.machine(SERVER, ID).unwrap();
            assert_eq!(
                crate::client::endpoint::ClientEndpointId::Ssh(boxed.profile.id.clone()),
                before
            );
            assert_eq!(boxed.profile.target, profiles[0].target);
            assert_eq!(boxed.profile.session, "agents");
            assert_eq!(boxed.profile.label, "box (work)");
            assert!(boxed.profile.enabled, "a connected remote stays connected");
            assert_eq!(boxed.cwd, "/data/work");
            let other = remotes.machine(SERVER, OTHER).unwrap();
            assert_eq!(other.profile.id, profiles[2].id);
            assert!(!other.profile.enabled, "never started implicitly");
            assert_eq!(other.sync, sync::SyncStatus::Pending);
            // The persisted selection still resolves to the same endpoint.
            let effective = effective_catalog().unwrap();
            assert_eq!(effective.selected_profile.as_ref(), Some(&profiles[0].id));
            assert_eq!(
                hangar_binding_for_target(&profiles[0].target)
                    .unwrap()
                    .unwrap()
                    .machine_id,
                ID
            );
            // Loading again is a no-op.
            assert_eq!(LocationPreferences::load().unwrap().hangar, prefs.hangar);
            assert_eq!(EndpointCatalog::load_profiles().unwrap(), saved);
        });
    }

    #[test]
    fn an_interrupted_migration_never_shows_a_machine_twice_and_the_next_sync_cleans_up() {
        with_state("migrate-interrupted", || {
            let (profiles, v2) = v2_state();
            let mut catalog = EndpointCatalog::default();
            catalog.ssh = profiles.clone();
            catalog.store_profiles().unwrap();
            write_locations(&v2);
            LocationPreferences::load().unwrap();
            // As if the catalog step had not run.
            catalog.store_profiles().unwrap();
            let remotes = Remotes::load().unwrap();
            let ids = remotes
                .profiles(true)
                .into_iter()
                .map(|profile| profile.id)
                .collect::<Vec<_>>();
            assert_eq!(ids.len(), 4, "{ids:?}");
            let report = sync::sync_with(&[SERVER.to_owned()], &|_| {
                Err(crate::hangar::api::HangarError::NotSignedIn)
            });
            assert_eq!(report.errors.len(), 1);
            assert_eq!(
                EndpointCatalog::load_profiles().unwrap(),
                [profiles[1].clone(), profiles[3].clone()]
            );
            assert_eq!(
                Remotes::load().unwrap().hangar.len(),
                2,
                "a failure removes nothing"
            );
        });
    }

    fn listed(id: &str, name: &str, state: &str) -> crate::hangar::api::Machine {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "state": state, "runtime": {"ready": state == "running"}
        }))
        .unwrap()
    }

    #[test]
    fn synced_machines_appear_and_disappear_with_their_prefs_and_files() {
        with_state("sync", || {
            let plain = SavedSshEndpoint::new("plain", "workbox", "default").unwrap();
            let mut catalog = EndpointCatalog::default();
            catalog.ssh = vec![plain.clone()];
            catalog.store_profiles().unwrap();
            let fetch_both = |_: &str| {
                Ok(vec![
                    listed(ID, "box", "running"),
                    listed(OTHER, "cold", "stopped"),
                ])
            };
            let report = sync::sync_with(&[SERVER.to_owned()], &fetch_both);
            assert_eq!(report.added, ["box", "cold"]);
            let remotes = Remotes::load().unwrap();
            let profiles = remotes.profiles(false);
            assert_eq!(
                profiles
                    .iter()
                    .map(|p| p.label.as_str())
                    .collect::<Vec<_>>(),
                ["plain", "box", "cold"]
            );
            assert!(profiles[1].enabled && !profiles[2].enabled);
            assert_eq!(
                profiles[1].id,
                sync::derived_profile_id(SERVER, ID),
                "a stable endpoint ID without saved prefs"
            );
            assert_eq!(
                EndpointCatalog::load_profiles().unwrap(),
                std::slice::from_ref(&plain)
            );
            // Local preferences, then hiding.
            let binding = remotes.machine(SERVER, OTHER).unwrap().binding.clone();
            edit_machine_prefs(&binding, Some("Cold box"), None, Some("/data/cold")).unwrap();
            set_hidden(&binding, true).unwrap();
            let remotes = Remotes::load().unwrap();
            assert_eq!(remotes.profiles(false).len(), 2, "hidden from the sidebar");
            let cold = remotes.machine(SERVER, OTHER).unwrap();
            assert_eq!(cold.profile.label, "Cold box");
            assert!(cold.hidden);
            assert_eq!(remotes.profiles(true).len(), 3, "still listed");
            {
                let _guard = operation_lock().unwrap();
                let mut prefs = LocationPreferences::load().unwrap();
                prefs.default_profile = Some(cold.profile.id.clone());
                prefs.store().unwrap();
            }
            // Its certificate exists; deleted elsewhere, it goes with its prefs.
            let ssh = crate::hangar::certs::SshPaths::herdr();
            std::fs::create_dir_all(ssh.key().parent().unwrap()).unwrap();
            std::fs::write(ssh.cert(OTHER), "cert").unwrap();
            std::fs::write(ssh.cert(ID), "cert").unwrap();
            let report = sync::sync_with(&[SERVER.to_owned()], &|_| {
                Ok(vec![listed(ID, "box", "running")])
            });
            assert_eq!(report.removed, ["cold"]);
            let remotes = Remotes::load().unwrap();
            assert!(remotes.machine(SERVER, OTHER).is_none());
            let prefs = LocationPreferences::load().unwrap();
            assert!(prefs.machine(SERVER, OTHER).is_none());
            assert_eq!(prefs.default_profile, None);
            assert!(!ssh.cert(OTHER).exists());
            assert!(ssh.cert(ID).exists());
            assert_eq!(
                sync::removed_machine_name(&cold.profile.id).as_deref(),
                Some("Cold box")
            );
            // Signed out: the last list stays, marked.
            sync::sync_with(&[SERVER.to_owned()], &|_| {
                Err(crate::hangar::api::HangarError::NotSignedIn)
            });
            let remotes = Remotes::load().unwrap();
            assert_eq!(remotes.hangar.len(), 1);
            assert_eq!(remotes.hangar[0].sync, sync::SyncStatus::SignedOut);
            assert!(remotes.hangar[0].profile.enabled);
        });
    }

    /// Runs `f` with `box` (default, with prefs) and `other` listed, and a plain SSH remote.
    fn with_machines<T>(name: &str, f: impl FnOnce(&Remotes) -> T) -> T {
        with_state(name, || {
            let plain = SavedSshEndpoint::new("plain", "workbox", "default").unwrap();
            let mut catalog = EndpointCatalog::default();
            catalog.ssh = vec![plain];
            catalog.store_profiles().unwrap();
            sync::sync_with(&[SERVER.to_owned()], &|_| {
                Ok(vec![
                    listed(ID, "box", "running"),
                    listed(OTHER, "other", "running"),
                ])
            });
            let remotes = Remotes::load().unwrap();
            let boxed = remotes.machine(SERVER, ID).unwrap().clone();
            edit_machine_prefs(&boxed.binding, None, None, Some("/data/box")).unwrap();
            {
                let _guard = operation_lock().unwrap();
                let mut prefs = LocationPreferences::load().unwrap();
                prefs.default_profile = Some(boxed.profile.id.clone());
                prefs.store().unwrap();
            }
            let ssh = crate::hangar::certs::SshPaths::herdr();
            std::fs::create_dir_all(ssh.key().parent().unwrap()).unwrap();
            std::fs::write(ssh.key(), "private").unwrap();
            for id in [ID, OTHER] {
                std::fs::write(ssh.cert(id), "cert").unwrap();
            }
            f(&Remotes::load().unwrap())
        })
    }

    fn delete_via(
        http: &std::sync::Arc<crate::hangar::api::fake::FakeHttp>,
        remote: &remotes::HangarRemote,
        stopped: &std::cell::RefCell<Vec<String>>,
    ) -> Result<String, String> {
        let client = crate::hangar::api::fake::client(http);
        delete_remote_with(
            &remote.profile,
            &remote.options(),
            |other| {
                stopped.borrow_mut().push(other.label.clone());
                Ok(())
            },
            |binding, progress| hangar::delete_with(&client, &binding.machine_id, progress),
            &mut |_| {},
        )
    }

    #[test]
    fn confirmation_names_the_machine_and_permanent_deletion() {
        let text = delete_confirmation("box");
        assert!(text.contains("'box'"));
        assert!(text.contains("disks and snapshots are permanently deleted"));
        assert!(text.contains("cannot be undone"));
    }

    #[test]
    fn deleting_a_machine_forgets_it_its_prefs_and_files() {
        with_machines("delete", |remotes| {
            use crate::hangar::api::fake::*;
            let boxed = remotes.machine(SERVER, ID).unwrap();
            let http = FakeHttp::new();
            http.reply(202, operation("op_1", "delete", "running"))
                .reply(200, operation("op_1", "delete", "succeeded"));
            let stopped = std::cell::RefCell::new(Vec::new());
            let message = delete_via(&http, boxed, &stopped).unwrap();
            assert!(
                message.contains("Deleted hangar machine 'box'"),
                "{message}"
            );
            assert_eq!(*stopped.borrow(), ["box"]);
            let sent = http.sent();
            assert_eq!(sent[0].method, "DELETE");
            assert!(sent[0].url.ends_with(&format!("/v1/machines/{ID}")));
            assert!(sent[0].idempotency_key.is_some());
            let after = Remotes::load().unwrap();
            assert!(after.machine(SERVER, ID).is_none());
            assert!(after.machine(SERVER, OTHER).is_some());
            assert_eq!(after.ssh.len(), 1);
            let prefs = LocationPreferences::load().unwrap();
            assert!(prefs.machine(SERVER, ID).is_none());
            assert_eq!(prefs.default_profile, None);
            let ssh = crate::hangar::certs::SshPaths::herdr();
            assert!(!ssh.cert(ID).exists());
            assert!(ssh.cert(OTHER).exists() && ssh.key().exists());
            // Herdr's own delete is not reported as "deleted on server".
            assert_eq!(sync::removed_machine_name(&boxed.profile.id), None);
        });
    }

    #[test]
    fn a_machine_already_gone_from_hangar_is_cleaned_up_locally() {
        with_machines("gone", |remotes| {
            use crate::hangar::api::fake::*;
            let http = FakeHttp::new();
            http.error(404, "not_found").error(404, "not_found");
            let stopped = std::cell::RefCell::new(Vec::new());
            let message =
                delete_via(&http, remotes.machine(SERVER, ID).unwrap(), &stopped).unwrap();
            assert!(message.contains("already deleted"), "{message}");
            assert!(Remotes::load().unwrap().machine(SERVER, ID).is_none());
        });
    }

    #[test]
    fn a_failed_delete_keeps_the_machine_listed_and_fenced() {
        with_machines("failed", |remotes| {
            use crate::hangar::api::fake::*;
            let http = FakeHttp::new();
            http.error(500, "internal");
            let stopped = std::cell::RefCell::new(Vec::new());
            let error =
                delete_via(&http, remotes.machine(SERVER, ID).unwrap(), &stopped).unwrap_err();
            assert!(error.contains("It stays listed"), "{error}");
            let after = Remotes::load().unwrap();
            let boxed = after.machine(SERVER, ID).unwrap();
            assert!(
                boxed.fenced && !boxed.profile.enabled,
                "no reconnect meanwhile"
            );
            assert_eq!(boxed.cwd, "/data/box");
            assert!(LocationPreferences::load()
                .unwrap()
                .default_profile
                .is_some());
            assert!(crate::hangar::certs::SshPaths::herdr().cert(ID).exists());
            // Start remote lifts the fence.
            let _guard = operation_lock().unwrap();
            sync::set_fence_locked(&boxed.binding, false).unwrap();
            drop(_guard);
            assert!(
                Remotes::load()
                    .unwrap()
                    .machine(SERVER, ID)
                    .unwrap()
                    .profile
                    .enabled
            );
        });
    }

    #[test]
    fn suspend_fences_the_machine_before_suspending_and_records_the_state() {
        with_machines("suspend", |remotes| {
            use crate::hangar::api::fake::*;
            let boxed = remotes.machine(SERVER, ID).unwrap();
            let http = FakeHttp::new();
            http.reply(202, operation("op_1", "suspend", "succeeded"));
            let client = client(&http);
            let message =
                suspend_remote_with(&boxed.profile, &boxed.options(), |binding, progress| {
                    // The fence is persisted before hangar is asked to suspend.
                    assert!(
                        !Remotes::load()
                            .unwrap()
                            .machine(SERVER, ID)
                            .unwrap()
                            .profile
                            .enabled
                    );
                    hangar::suspend_with(&client, &binding.machine_id, progress)
                })
                .unwrap();
            assert!(message.contains("suspended"));
            assert_eq!(http.paths(), [format!("POST /v1/machines/{ID}/suspend")]);
            let after = Remotes::load().unwrap();
            assert_eq!(
                after.machine(SERVER, ID).unwrap().state,
                MachineState::Suspended
            );
            assert_eq!(after.hangar.len(), 2, "suspend never removes machines");
        });
    }

    #[test]
    fn a_hangar_machine_cannot_be_removed_only_deleted_or_hidden() {
        with_machines("remove", |remotes| {
            let boxed = remotes.machine(SERVER, ID).unwrap();
            let error = remove_remote(&boxed.profile, &boxed.options()).unwrap_err();
            assert!(error.contains("Delete machine…"), "{error}");
            assert!(error.contains("Hide from sidebar"), "{error}");
            assert_eq!(Remotes::load().unwrap().hangar.len(), 2);
            let plain = remotes.ssh[0].clone();
            remove_remote(&plain, &RemoteOptions::default()).unwrap();
            let after = Remotes::load().unwrap();
            assert!(after.ssh.is_empty());
            assert_eq!(after.hangar.len(), 2);
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
