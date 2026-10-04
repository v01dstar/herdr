//! The remotes Herdr shows: saved SSH profiles (the endpoint catalog is their truth)
//! plus hangar machines from the synced list with their local preferences. A hangar
//! machine becomes an in-memory SSH profile whose target is its `hangar-<machineId>`
//! alias; it is never written to the generation-1 endpoint catalog.
use super::sync::{self, MachineCache, SyncStatus};
use super::{CloudBinding, LocationPreferences, RemoteOptions};
use crate::client::endpoint::{EndpointCatalog, ProfileId, SavedSshEndpoint};
use crate::hangar::api::MachineState;
use crate::hangar::binding::HangarBinding;

/// One hangar machine as a remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HangarRemote {
    pub profile: SavedSshEndpoint,
    pub binding: HangarBinding,
    pub state: MachineState,
    pub hidden: bool,
    /// Herdr is stopping, suspending or deleting it.
    pub fenced: bool,
    pub cwd: String,
    /// How the last fetch of its server went; the list may be outdated otherwise.
    pub sync: SyncStatus,
}

impl HangarRemote {
    pub(crate) fn options(&self) -> RemoteOptions {
        RemoteOptions {
            cwd: self.cwd.clone(),
            cloud: Some(CloudBinding::Hangar(self.binding.clone())),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Remotes {
    /// Saved SSH profiles only (never hangar machines).
    pub ssh: Vec<SavedSshEndpoint>,
    pub prefs: LocationPreferences,
    pub hangar: Vec<HangarRemote>,
}

impl Remotes {
    /// Reads the catalog, preferences and synced list. An unreadable list or preference
    /// file only hides hangar machines; it never removes anything.
    pub(crate) fn load() -> Result<Self, String> {
        let ssh = EndpointCatalog::load_profiles()?;
        let prefs = LocationPreferences::load()?;
        let cache = MachineCache::load().unwrap_or_else(|error| {
            tracing::warn!(%error, "hangar machine list is unavailable");
            MachineCache::default()
        });
        Ok(Self::build(ssh, prefs, &cache, sync::now_ms()))
    }

    pub(crate) fn build(
        ssh: Vec<SavedSshEndpoint>,
        prefs: LocationPreferences,
        cache: &MachineCache,
        now_ms: u64,
    ) -> Self {
        let mut hangar = Vec::new();
        for (server, list) in &cache.servers {
            for machine in &list.machines {
                let entry = prefs.machine(server, &machine.id);
                let Ok(binding) = HangarBinding::new(server, &machine.id, &machine.name) else {
                    continue;
                };
                let hidden = entry.is_some_and(|entry| entry.hidden);
                let fenced = machine.fenced(now_ms);
                let profile = SavedSshEndpoint {
                    id: sync::profile_id(entry, server, &machine.id),
                    label: entry
                        .and_then(|entry| entry.display_name.clone())
                        .unwrap_or_else(|| machine.name.clone()),
                    target: binding.alias.clone(),
                    session: entry
                        .and_then(|entry| entry.session.clone())
                        .unwrap_or_else(|| super::hangar::SESSION.to_owned()),
                    // Running machines connect; stopped ones never start implicitly.
                    enabled: !hidden && !fenced && machine.state == MachineState::Running,
                };
                hangar.push(HangarRemote {
                    profile,
                    binding,
                    state: machine.state,
                    hidden,
                    fenced,
                    cwd: entry.map(|entry| entry.cwd.clone()).unwrap_or_default(),
                    sync: list.status,
                });
            }
        }
        hangar.sort_by(|left, right| {
            left.profile
                .label
                .cmp(&right.profile.label)
                .then(left.binding.server.cmp(&right.binding.server))
                .then(left.binding.machine_id.cmp(&right.binding.machine_id))
        });
        // A profile a migration moved but could not delete yet is the machine now.
        let ssh = ssh
            .into_iter()
            .filter(|profile| !hangar.iter().any(|remote| remote.profile.id == profile.id))
            .collect();
        Self { ssh, prefs, hangar }
    }

    /// SSH profiles, then hangar machines. Hidden machines only with `include_hidden`.
    pub(crate) fn profiles(&self, include_hidden: bool) -> Vec<SavedSshEndpoint> {
        self.ssh
            .iter()
            .cloned()
            .chain(
                self.hangar
                    .iter()
                    .filter(|remote| include_hidden || !remote.hidden)
                    .map(|remote| remote.profile.clone()),
            )
            .collect()
    }

    pub(crate) fn hangar_remote(&self, id: &ProfileId) -> Option<&HangarRemote> {
        self.hangar.iter().find(|remote| &remote.profile.id == id)
    }

    pub(crate) fn machine(&self, server: &str, machine_id: &str) -> Option<&HangarRemote> {
        self.hangar.iter().find(|remote| {
            remote.binding.server == server && remote.binding.machine_id == machine_id
        })
    }

    /// Preferences with every hangar machine as a remote bound to it, for the dialogs.
    /// Never stored.
    pub(crate) fn view_prefs(&self) -> LocationPreferences {
        let mut view = self.prefs.clone();
        view.view = true;
        for remote in &self.hangar {
            view.remotes
                .insert(remote.profile.id.clone(), remote.options());
        }
        view
    }
}

/// The remotes this client connects to and shows in the sidebar.
/// Unreadable preferences or list files only hide hangar machines here.
pub(crate) fn effective_profiles() -> Result<Vec<SavedSshEndpoint>, String> {
    let ssh = EndpointCatalog::load_profiles()?;
    Ok(Remotes::lenient(ssh).profiles(false))
}

impl Remotes {
    fn lenient(ssh: Vec<SavedSshEndpoint>) -> Self {
        let prefs = LocationPreferences::load().unwrap_or_else(|error| {
            tracing::warn!(%error, "remote locations are unavailable");
            LocationPreferences::default()
        });
        let cache = MachineCache::load().unwrap_or_default();
        Self::build(ssh, prefs, &cache, sync::now_ms())
    }
}

/// The endpoint catalog with visible hangar machines, and the persisted selection
/// checked against both.
pub(crate) fn effective_catalog() -> Result<EndpointCatalog, String> {
    let extra = Remotes::lenient(EndpointCatalog::load_profiles()?)
        .hangar
        .into_iter()
        .filter(|remote| !remote.hidden)
        .map(|remote| remote.profile)
        .collect();
    EndpointCatalog::load_with(extra)
}
