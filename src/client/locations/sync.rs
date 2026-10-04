//! hangar machines come from the server. Herdr keeps only the last successful
//! `GET /v1/machines` answer per server (`client/hangar-machines.json`) so the list
//! survives restarts, sign-out and network failures, plus per-machine preferences in
//! `client/locations.json`. Machines are added or removed only when the server returns a
//! successful list (or confirms a create, fork or delete); a failed fetch only marks the
//! cached list signed-out or offline.
//!
//! Every fetch runs on a worker thread. A result is applied only when no fetch that
//! started later has been applied already, so a slow, stale answer never overrides a
//! newer one, across threads and across clients sharing this state directory.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{operation_lock, LocationPreferences, MachinePrefs};
use crate::client::endpoint::ProfileId;
use crate::hangar::api::{HangarError, Machine, MachineState};
use crate::hangar::binding::{is_machine_id, HangarBinding};

const CACHE_VERSION: u32 = 1;
/// Discovery of machines created or deleted elsewhere.
pub(crate) const BACKGROUND_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// While Settings → remotes is open.
pub(crate) const SETTINGS_INTERVAL: Duration = Duration::from_secs(15);
/// At most one classification sync per server after connection failures.
const FAILURE_SYNC_INTERVAL: Duration = Duration::from_secs(15);
/// How long Stop, Suspend and Delete keep a still-running machine from reconnecting
/// while hangar has not reported the new state yet.
pub(crate) const FENCE: Duration = Duration::from_secs(120);
/// "Deleted on server" notices are matched against endpoints retired this recently.
const REMOVED_TTL_MS: u64 = 10 * 60 * 1000;
const MAX_REMOVED: usize = 16;
const MAX_MACHINES_PER_SERVER: usize = 256;
const MAX_SERVERS: usize = 8;

/// How the last fetch for a server went. The machine list is the last successful one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncStatus {
    /// Never fetched (seeded by migration from saved remotes).
    #[default]
    Pending,
    Ok,
    SignedOut,
    Unreachable,
}

impl SyncStatus {
    /// Shown next to machines of a server whose list may be outdated.
    pub(crate) fn note(self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::Pending => Some("not synced yet"),
            Self::SignedOut => Some("signed out"),
            Self::Unreachable => Some("offline"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedMachine {
    pub id: String,
    pub name: String,
    pub state: MachineState,
    /// Until then a machine hangar still reports running does not connect: Herdr is
    /// stopping, suspending or deleting it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fence_until_ms: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl CachedMachine {
    pub(crate) fn fenced(&self, now_ms: u64) -> bool {
        self.fence_until_ms > now_ms
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerMachines {
    #[serde(default)]
    pub status: SyncStatus,
    /// The last fetch error, for signed-out and offline lists.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// When the newest applied fetch started; older results are ignored.
    #[serde(default)]
    pub fetch_started_ms: u64,
    /// When the list was last fetched successfully.
    #[serde(default)]
    pub synced_ms: u64,
    #[serde(default)]
    pub machines: Vec<CachedMachine>,
}

/// A machine that disappeared from a successful list, so a client whose endpoint it
/// retires can say why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovedMachine {
    pub server: String,
    pub machine_id: String,
    pub name: String,
    pub profile_id: ProfileId,
    pub removed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MachineCache {
    version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerMachines>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<RemovedMachine>,
}

impl Default for MachineCache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            servers: BTreeMap::new(),
            removed: Vec::new(),
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// The endpoint ID of a hangar machine that no saved remote used before: stable for
/// (server, machine ID) on every client, so session state keyed by it survives.
pub(crate) fn derived_profile_id(server: &str, machine_id: &str) -> ProfileId {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(format!("hangar\n{server}\n{machine_id}").as_bytes());
    let hex = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // 32 lowercase hex characters always parse.
    ProfileId::parse(hex).unwrap_or_else(|_| ProfileId::generate())
}

/// The endpoint ID of a machine: the one a migrated remote used, else the derived one.
pub(crate) fn profile_id(
    prefs: Option<&MachinePrefs>,
    server: &str,
    machine_id: &str,
) -> ProfileId {
    prefs
        .and_then(|prefs| prefs.profile_id.clone())
        .unwrap_or_else(|| derived_profile_id(server, machine_id))
}

impl MachineCache {
    fn path() -> std::path::PathBuf {
        crate::config::state_dir()
            .join("client")
            .join("hangar-machines.json")
    }

    pub(crate) fn load() -> Result<Self, String> {
        let bytes = match std::fs::read(Self::path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(format!("Cannot read hangar machines: {error}")),
        };
        let cache: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid hangar machine list: {error}"))?;
        if cache.version != CACHE_VERSION {
            return Err("Unsupported hangar machine list".into());
        }
        Ok(cache)
    }

    pub(crate) fn store(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        crate::client::endpoint::store_private_json(&Self::path(), &bytes, "hangar machines")
    }

    #[cfg(test)]
    pub(crate) fn machine(&self, server: &str, id: &str) -> Option<&CachedMachine> {
        self.servers
            .get(server)?
            .machines
            .iter()
            .find(|machine| machine.id == id)
    }

    fn machine_mut(&mut self, server: &str, id: &str) -> Option<&mut CachedMachine> {
        self.servers
            .get_mut(server)?
            .machines
            .iter_mut()
            .find(|machine| machine.id == id)
    }

    /// The machine reached through `alias` (`hangar-<machineId>`), on any server.
    pub(crate) fn binding_for_alias(&self, alias: &str) -> Option<HangarBinding> {
        let id = crate::hangar::binding::machine_id_for_target(alias)?;
        self.servers.iter().find_map(|(server, list)| {
            list.machines
                .iter()
                .find(|machine| machine.id == id)
                .and_then(|machine| HangarBinding::new(server, &machine.id, &machine.name).ok())
        })
    }

    fn prune_removed(&mut self, now_ms: u64) {
        self.removed
            .retain(|removed| now_ms.saturating_sub(removed.removed_ms) < REMOVED_TTL_MS);
        let excess = self.removed.len().saturating_sub(MAX_REMOVED);
        self.removed.drain(..excess);
    }
}

/// What applying one fetch changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// A fetch that started later was applied already; nothing changed.
    pub stale: bool,
    pub added: Vec<String>,
    /// Machines (and their preferences) dropped because hangar no longer lists them.
    pub removed: Vec<HangarBinding>,
    pub prefs_changed: bool,
}

fn listed(machine: &Machine) -> Option<CachedMachine> {
    if !is_machine_id(&machine.id) || machine.state == MachineState::Deleted {
        return None;
    }
    let name = if machine.name.is_empty()
        || machine.name.len() > 128
        || machine.name.chars().any(char::is_control)
    {
        machine.id.clone()
    } else {
        machine.name.clone()
    };
    Some(CachedMachine {
        id: machine.id.clone(),
        name,
        state: machine.state,
        fence_until_ms: 0,
    })
}

/// Applies one fetch for `server` that started at `started_ms`. Pure: the caller
/// loads and stores the files. A successful list replaces the cached one and drops
/// the preferences of machines it no longer has; a failure keeps the list and only
/// records why it may be outdated.
pub(crate) fn reconcile(
    cache: &mut MachineCache,
    prefs: &mut LocationPreferences,
    server: &str,
    started_ms: u64,
    now_ms: u64,
    result: Result<Vec<Machine>, &HangarError>,
) -> Applied {
    let mut applied = Applied::default();
    if cache
        .servers
        .get(server)
        .is_some_and(|entry| entry.fetch_started_ms > started_ms)
    {
        applied.stale = true;
        return applied;
    }
    if !cache.servers.contains_key(server) && cache.servers.len() >= MAX_SERVERS {
        applied.stale = true;
        return applied;
    }
    let entry = cache.servers.entry(server.to_owned()).or_default();
    entry.fetch_started_ms = started_ms;
    let machines = match result {
        Err(error) => {
            entry.status = if error.needs_sign_in() {
                SyncStatus::SignedOut
            } else {
                SyncStatus::Unreachable
            };
            entry.message = error.to_string();
            return applied;
        }
        Ok(machines) => machines,
    };
    let mut next = machines.iter().filter_map(listed).collect::<Vec<_>>();
    next.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    next.dedup_by(|left, right| left.id == right.id);
    next.truncate(MAX_MACHINES_PER_SERVER);
    for machine in &mut next {
        match entry.machines.iter().find(|old| old.id == machine.id) {
            // Herdr is stopping it: keep it from reconnecting until hangar catches up.
            Some(old) if old.fenced(now_ms) && machine.state == MachineState::Running => {
                machine.fence_until_ms = old.fence_until_ms;
            }
            Some(_) => {}
            None => applied.added.push(machine.name.clone()),
        }
    }
    let gone = entry
        .machines
        .iter()
        .filter(|old| !next.iter().any(|machine| machine.id == old.id))
        .cloned()
        .collect::<Vec<_>>();
    entry.machines = next;
    entry.status = SyncStatus::Ok;
    entry.message.clear();
    entry.synced_ms = now_ms;
    let listed_ids = entry
        .machines
        .iter()
        .map(|machine| machine.id.clone())
        .collect::<Vec<_>>();
    for old in gone {
        let machine_prefs = prefs.machine(server, &old.id).cloned();
        let profile_id = profile_id(machine_prefs.as_ref(), server, &old.id);
        cache.removed.push(RemovedMachine {
            server: server.to_owned(),
            machine_id: old.id.clone(),
            name: machine_prefs
                .as_ref()
                .and_then(|prefs| prefs.display_name.clone())
                .unwrap_or_else(|| old.name.clone()),
            profile_id,
            removed_ms: now_ms,
        });
        if let Ok(binding) = HangarBinding::new(server, &old.id, &old.name) {
            applied.removed.push(binding);
        }
    }
    // Preferences exist only for machines hangar lists.
    let before = prefs.hangar.len();
    let dropped = prefs
        .hangar
        .iter()
        .filter(|entry| entry.server == server && !listed_ids.contains(&entry.machine_id))
        .map(|entry| profile_id(Some(entry), server, &entry.machine_id))
        .collect::<Vec<_>>();
    prefs
        .hangar
        .retain(|entry| entry.server != server || listed_ids.contains(&entry.machine_id));
    let removed_ids = applied
        .removed
        .iter()
        .map(|binding| profile_id(None, server, &binding.machine_id))
        .chain(dropped)
        .chain(
            cache
                .removed
                .iter()
                .filter(|removed| removed.server == server)
                .map(|removed| removed.profile_id.clone()),
        )
        .collect::<Vec<_>>();
    if prefs
        .default_profile
        .as_ref()
        .is_some_and(|id| removed_ids.contains(id))
    {
        prefs.default_profile = None;
        applied.prefs_changed = true;
    }
    applied.prefs_changed |= prefs.hangar.len() != before;
    cache.prune_removed(now_ms);
    applied
}

/// Removes saved SSH profiles that a migration moved into hangar preferences but did
/// not get to delete (an interrupted migration). Callers hold `operation_lock`.
fn sweep_migrated_profiles(prefs: &LocationPreferences) {
    let moved = prefs
        .hangar
        .iter()
        .filter_map(|entry| entry.profile_id.clone())
        .collect::<Vec<_>>();
    if moved.is_empty() {
        return;
    }
    let Ok(mut catalog) = crate::client::endpoint::EndpointCatalog::load() else {
        return;
    };
    let mut changed = false;
    for id in &moved {
        changed |= catalog.remove_ssh(id);
    }
    if changed {
        if let Err(error) = catalog.store_profiles() {
            tracing::warn!(%error, "could not remove migrated hangar profiles");
        }
    }
}

/// The servers to fetch: every cached one that lists machines, plus the signed-in
/// server.
pub(crate) fn servers_to_sync(cache: &MachineCache) -> Vec<String> {
    let mut servers = cache
        .servers
        .iter()
        .filter(|(_, list)| !list.machines.is_empty())
        .map(|(server, _)| server.clone())
        .collect::<Vec<_>>();
    let signed_in = crate::hangar::auth::CredentialStore::shared()
        .and_then(|store| store.load().ok().flatten())
        .is_some();
    let default = crate::hangar::default_server();
    if (signed_in || std::env::var_os(crate::hangar::SERVER_ENV).is_some())
        && !servers.contains(&default)
    {
        servers.push(default);
    }
    servers
}

/// What one sync did, for messages.
#[derive(Debug, Default)]
pub(crate) struct SyncReport {
    pub errors: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

pub(crate) type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<Machine>, HangarError>;

/// Applies one fetch result to the files under `operation_lock`, then forgets the local
/// files of machines hangar no longer has.
fn apply(
    server: &str,
    started_ms: u64,
    result: Result<Vec<Machine>, &HangarError>,
) -> Result<Applied, String> {
    let applied = {
        let _guard = operation_lock()?;
        let mut cache = MachineCache::load()?;
        let mut prefs = LocationPreferences::load()?;
        let before = cache.clone();
        let applied = reconcile(&mut cache, &mut prefs, server, started_ms, now_ms(), result);
        if cache != before {
            // The list first: preferences of a vanished machine are dropped again by
            // the next successful sync if storing them fails.
            cache.store()?;
        }
        if applied.prefs_changed {
            prefs.store()?;
        }
        sweep_migrated_profiles(&prefs);
        applied
    };
    for binding in &applied.removed {
        super::forget_machine_files(binding);
    }
    Ok(applied)
}

/// Fetches and applies each server in turn.
pub(crate) fn sync_with(servers: &[String], fetch: Fetch<'_>) -> SyncReport {
    let mut report = SyncReport::default();
    for server in servers {
        let started = now_ms();
        let result = fetch(server);
        if let Err(error) = &result {
            report.errors.push(error.to_string());
        }
        match apply(
            server,
            started,
            result.as_ref().map(|machines| machines.to_vec()),
        ) {
            Ok(applied) => {
                report.added.extend(applied.added);
                report.removed.extend(
                    applied
                        .removed
                        .into_iter()
                        .map(|binding| binding.machine_name),
                );
            }
            Err(error) => {
                tracing::debug!(%error, server, "could not store hangar machines");
                report.errors.push(error);
            }
        }
    }
    report
}

/// One sync of every server. Blocks on HTTP: worker threads only.
pub(crate) fn sync_now() -> SyncReport {
    let cache = MachineCache::load().unwrap_or_else(|error| {
        tracing::debug!(%error, "hangar machine list unreadable; syncing from scratch");
        MachineCache::default()
    });
    let servers = servers_to_sync(&cache);
    if servers.is_empty() {
        return SyncReport::default();
    }
    sync_with(&servers, &|server| super::hangar::list_machines(server))
}

/// Edits the cached entry of one machine. Callers hold `operation_lock`.
pub(super) fn edit_machine_locked(
    binding: &HangarBinding,
    edit: impl FnOnce(&mut CachedMachine),
) -> Result<(), String> {
    let mut cache = MachineCache::load()?;
    let Some(machine) = cache.machine_mut(&binding.server, &binding.machine_id) else {
        return Ok(());
    };
    let before = machine.clone();
    edit(machine);
    if *machine != before {
        cache.store()?;
    }
    Ok(())
}

/// Keeps a machine Herdr is about to stop, suspend or delete from reconnecting, or lifts
/// that once Herdr started it. Callers hold `operation_lock`.
pub(super) fn set_fence_locked(binding: &HangarBinding, fenced: bool) -> Result<(), String> {
    edit_machine_locked(binding, |machine| {
        machine.fence_until_ms = if fenced {
            now_ms() + FENCE.as_millis() as u64
        } else {
            0
        };
    })
}

/// Records a state hangar just confirmed for one machine (an operation finished).
pub(crate) fn record_state(binding: &HangarBinding, state: MachineState) -> Result<(), String> {
    let _guard = operation_lock()?;
    edit_machine_locked(binding, |machine| {
        machine.state = state;
        if state == MachineState::Running {
            machine.fence_until_ms = 0;
        }
    })
}

/// Lists a machine hangar just created or forked, before the next sync.
pub(crate) fn record_listed(server: &str, machine: &Machine) -> Result<(), String> {
    let Some(listed) = listed(machine) else {
        return Err("hangar returned an invalid machine".into());
    };
    let server = crate::hangar::normalize_server(server);
    let _guard = operation_lock()?;
    let mut cache = MachineCache::load()?;
    let entry = cache.servers.entry(server).or_default();
    match entry.machines.iter().position(|old| old.id == listed.id) {
        Some(index) => entry.machines[index] = listed,
        None if entry.machines.len() < MAX_MACHINES_PER_SERVER => entry.machines.push(listed),
        None => return Err("Too many hangar machines to list".into()),
    }
    cache.store()
}

/// Drops a machine hangar confirmed deleted, with its preferences and local files.
pub(crate) fn forget_machine(binding: &HangarBinding) -> Result<(), String> {
    {
        let _guard = operation_lock()?;
        let mut cache = MachineCache::load()?;
        if let Some(entry) = cache.servers.get_mut(&binding.server) {
            let before = entry.machines.len();
            entry
                .machines
                .retain(|machine| machine.id != binding.machine_id);
            if entry.machines.len() != before {
                cache.store()?;
            }
        }
        let mut prefs = LocationPreferences::load()?;
        let removed = prefs.remove_machine(&binding.server, &binding.machine_id);
        if removed {
            prefs.store()?;
        }
    }
    super::forget_machine_files(binding);
    Ok(())
}

/// The name to show when `profile_id`'s endpoint disappears because hangar no longer
/// lists its machine.
pub(crate) fn removed_machine_name(profile_id: &ProfileId) -> Option<String> {
    let cache = MachineCache::load().ok()?;
    let now = now_ms();
    cache
        .removed
        .iter()
        .rev()
        .find(|removed| {
            &removed.profile_id == profile_id
                && now.saturating_sub(removed.removed_ms) < REMOVED_TTL_MS
        })
        .map(|removed| removed.name.clone())
}

/// A connection to a hangar machine failed: fetch its server's list now (at most once
/// per [`FAILURE_SYNC_INTERVAL`]) so a deleted machine disappears and a stopped one stops
/// reconnecting. Runs on the connecting worker; network failures only mark the list.
pub(crate) fn after_connection_failure(binding: &HangarBinding) {
    static LAST: OnceLock<Mutex<BTreeMap<String, Instant>>> = OnceLock::new();
    {
        let mut last = LAST
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if last
            .get(&binding.server)
            .is_some_and(|at| now.duration_since(*at) < FAILURE_SYNC_INTERVAL)
        {
            return;
        }
        last.insert(binding.server.clone(), now);
    }
    let report = sync_with(std::slice::from_ref(&binding.server), &|server| {
        super::hangar::list_machines(server)
    });
    tracing::debug!(?report, "classified a failed hangar connection");
}

/// Whether a server's list is older than the background interval.
fn due(cache: &MachineCache, servers: &[String], now_ms: u64) -> bool {
    servers.iter().any(|server| {
        cache.servers.get(server).is_none_or(|entry| {
            now_ms.saturating_sub(entry.fetch_started_ms) >= BACKGROUND_INTERVAL.as_millis() as u64
        })
    })
}

/// Discovers machines created or deleted elsewhere every [`BACKGROUND_INTERVAL`]; a
/// fetch by any client sharing this state directory counts.
pub(crate) fn spawn_background(should_quit: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !should_quit.load(Ordering::Acquire) {
            let cache = MachineCache::load().unwrap_or_default();
            let servers = servers_to_sync(&cache);
            if !servers.is_empty() && due(&cache, &servers, now_ms()) {
                let report = sync_now();
                tracing::debug!(?report, "synced hangar machines");
            }
            for _ in 0..30 {
                if should_quit.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "https://hangar.test";
    const A: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";
    const B: &str = "m_bbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "m_cccccccccccccccccccccccccc";

    fn machine(id: &str, name: &str, state: &str) -> Machine {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "state": state, "runtime": {"ready": state == "running"}
        }))
        .unwrap()
    }

    fn prefs_for(ids: &[&str]) -> LocationPreferences {
        let mut prefs = LocationPreferences::default();
        for id in ids {
            prefs.hangar.push(MachinePrefs {
                server: SERVER.into(),
                machine_id: (*id).into(),
                cwd: format!("/data/{id}"),
                ..Default::default()
            });
        }
        prefs
    }

    fn ids(cache: &MachineCache) -> Vec<&str> {
        cache.servers[SERVER]
            .machines
            .iter()
            .map(|machine| machine.id.as_str())
            .collect()
    }

    #[test]
    fn a_successful_list_adds_removes_and_updates_machines_and_cleans_prefs() {
        let mut cache = MachineCache::default();
        let mut prefs = prefs_for(&[A, B]);
        prefs.default_profile = Some(derived_profile_id(SERVER, B));
        let first = reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            1,
            10,
            Ok(vec![
                machine(A, "box", "running"),
                machine(B, "cold", "stopped"),
            ]),
        );
        assert_eq!(first.added, ["box", "cold"]);
        assert!(first.removed.is_empty());
        assert_eq!(cache.servers[SERVER].status, SyncStatus::Ok);
        // B was deleted elsewhere, C created elsewhere, A stopped.
        let second = reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            2,
            20,
            Ok(vec![
                machine(A, "box", "stopped"),
                machine(C, "new", "running"),
                machine("m_deleteddeleteddeleteddele", "gone", "deleted"),
            ]),
        );
        assert_eq!(second.added, ["new"]);
        assert_eq!(
            second
                .removed
                .iter()
                .map(|binding| binding.machine_id.as_str())
                .collect::<Vec<_>>(),
            [B]
        );
        assert_eq!(ids(&cache), [A, C]);
        assert_eq!(
            cache.machine(SERVER, A).unwrap().state,
            MachineState::Stopped
        );
        assert!(second.prefs_changed);
        assert!(
            prefs.machine(SERVER, B).is_none(),
            "prefs follow the server"
        );
        assert_eq!(prefs.machine(SERVER, A).unwrap().cwd, format!("/data/{A}"));
        assert_eq!(prefs.default_profile, None, "a deleted default falls back");
        assert_eq!(cache.removed.len(), 1);
        assert_eq!(cache.removed[0].name, "cold");
        assert_eq!(cache.removed[0].profile_id, derived_profile_id(SERVER, B));
    }

    #[test]
    fn a_failed_fetch_keeps_the_list_and_marks_it_offline_or_signed_out() {
        let mut cache = MachineCache::default();
        let mut prefs = prefs_for(&[A]);
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            1,
            10,
            Ok(vec![machine(A, "box", "running")]),
        );
        let offline = HangarError::Transport(crate::hangar::api::TransportError::Unreachable(
            "no route".into(),
        ));
        let applied = reconcile(&mut cache, &mut prefs, SERVER, 2, 20, Err(&offline));
        assert_eq!(applied, Applied::default());
        assert_eq!(ids(&cache), [A]);
        assert_eq!(cache.servers[SERVER].status, SyncStatus::Unreachable);
        assert!(cache.servers[SERVER].message.contains("no route"));
        assert_eq!(cache.servers[SERVER].synced_ms, 10);
        assert!(prefs.machine(SERVER, A).is_some());
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            3,
            30,
            Err(&HangarError::NotSignedIn),
        );
        assert_eq!(ids(&cache), [A]);
        assert_eq!(cache.servers[SERVER].status, SyncStatus::SignedOut);
        assert_eq!(SyncStatus::SignedOut.note(), Some("signed out"));
        // A failure for a server never listed records it without machines.
        reconcile(
            &mut cache,
            &mut prefs,
            "https://other.test",
            4,
            40,
            Err(&HangarError::NotSignedIn),
        );
        assert!(cache.servers["https://other.test"].machines.is_empty());
        assert!(prefs.machine(SERVER, A).is_some());
    }

    #[test]
    fn an_incomplete_listing_adds_and_removes_nothing() {
        use crate::hangar::api::fake::{client, FakeHttp};
        let mut cache = MachineCache::default();
        let mut prefs = prefs_for(&[A]);
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            1,
            10,
            Ok(vec![machine(A, "box", "running")]),
        );
        // The second page fails after the first listed only a new machine.
        let http = FakeHttp::new();
        http.reply(
            200,
            serde_json::json!({"machines": [crate::hangar::api::fake::machine(C, "running", true)], "nextCursor": "c2"}),
        )
        .error(500, "internal");
        let result = client(&http).machines();
        let error = result.as_ref().unwrap_err();
        let applied = reconcile(&mut cache, &mut prefs, SERVER, 2, 20, Err(error));
        assert!(applied.added.is_empty() && applied.removed.is_empty());
        assert_eq!(ids(&cache), [A]);
        assert!(prefs.machine(SERVER, A).is_some());
        assert_eq!(cache.servers[SERVER].status, SyncStatus::Unreachable);
    }

    #[test]
    fn a_result_from_an_older_fetch_never_overrides_a_newer_one() {
        let mut cache = MachineCache::default();
        let mut prefs = prefs_for(&[A]);
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            5,
            50,
            Ok(vec![machine(A, "box", "running")]),
        );
        // Started before the applied fetch: it must not delete A or its prefs.
        let applied = reconcile(&mut cache, &mut prefs, SERVER, 4, 60, Ok(Vec::new()));
        assert!(applied.stale);
        assert_eq!(ids(&cache), [A]);
        assert!(prefs.machine(SERVER, A).is_some());
        let applied = reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            3,
            60,
            Err(&HangarError::NotSignedIn),
        );
        assert!(applied.stale);
        assert_eq!(cache.servers[SERVER].status, SyncStatus::Ok);
    }

    #[test]
    fn a_fence_survives_a_running_report_until_hangar_catches_up_or_it_expires() {
        let mut cache = MachineCache::default();
        let mut prefs = LocationPreferences::default();
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            1,
            10,
            Ok(vec![machine(A, "box", "running")]),
        );
        cache.machine_mut(SERVER, A).unwrap().fence_until_ms = 100;
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            2,
            20,
            Ok(vec![machine(A, "box", "running")]),
        );
        assert!(cache.machine(SERVER, A).unwrap().fenced(20));
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            3,
            30,
            Ok(vec![machine(A, "box", "stopping")]),
        );
        assert!(!cache.machine(SERVER, A).unwrap().fenced(30));
        cache.machine_mut(SERVER, A).unwrap().fence_until_ms = 100;
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            4,
            200,
            Ok(vec![machine(A, "box", "running")]),
        );
        assert!(!cache.machine(SERVER, A).unwrap().fenced(200));
    }

    #[test]
    fn invalid_ids_and_names_from_the_server_are_not_listed_as_is() {
        let mut cache = MachineCache::default();
        let mut prefs = LocationPreferences::default();
        reconcile(
            &mut cache,
            &mut prefs,
            SERVER,
            1,
            10,
            Ok(vec![
                machine("m_bad", "x", "running"),
                machine(A, "evil\u{1b}[2J", "running"),
            ]),
        );
        assert_eq!(ids(&cache), [A]);
        assert_eq!(cache.machine(SERVER, A).unwrap().name, A);
    }

    #[test]
    fn derived_endpoint_ids_are_stable_per_server_and_machine() {
        let id = derived_profile_id(SERVER, A);
        assert_eq!(id, derived_profile_id(SERVER, A));
        assert_ne!(id, derived_profile_id("https://other.test", A));
        assert_ne!(id, derived_profile_id(SERVER, B));
        assert_eq!(id.as_str().len(), 32);
        let migrated = ProfileId::generate();
        let prefs = MachinePrefs {
            server: SERVER.into(),
            machine_id: A.into(),
            profile_id: Some(migrated.clone()),
            ..Default::default()
        };
        assert_eq!(profile_id(Some(&prefs), SERVER, A), migrated);
    }

    #[test]
    fn removed_notices_expire_and_are_bounded() {
        let mut cache = MachineCache::default();
        for index in 0..20 {
            cache.removed.push(RemovedMachine {
                server: SERVER.into(),
                machine_id: A.into(),
                name: format!("m{index}"),
                profile_id: derived_profile_id(SERVER, A),
                removed_ms: 1_000_000 + index,
            });
        }
        cache.prune_removed(1_000_100);
        assert_eq!(cache.removed.len(), MAX_REMOVED);
        assert_eq!(cache.removed[0].name, "m4");
        cache.prune_removed(1_000_000 + REMOVED_TTL_MS + 50);
        assert!(cache.removed.is_empty());
    }

    #[test]
    fn the_background_sync_is_due_after_its_interval_for_any_client() {
        let mut cache = MachineCache::default();
        let servers = vec![SERVER.to_owned()];
        assert!(due(&cache, &servers, 1));
        cache.servers.insert(
            SERVER.into(),
            ServerMachines {
                fetch_started_ms: 1_000,
                ..Default::default()
            },
        );
        assert!(!due(&cache, &servers, 2_000));
        assert!(due(
            &cache,
            &servers,
            1_000 + BACKGROUND_INTERVAL.as_millis() as u64
        ));
    }
}
