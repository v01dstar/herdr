//! Recoverable client-side cloud setup. No runtime codec or server state changes.
use super::{cli, inventory, run, run_with_timeout, CloudOperation, CloudTarget};
use crate::client::endpoint::{EndpointCatalog, ProfileId, SavedSshEndpoint};
use crate::client::locations::{operation_lock, LocationPreferences, RemoteOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Stage {
    Planned,
    Creating,
    Created,
    Deploying,
    Deployed,
    Ready,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Operation {
    version: u32,
    pub profile: SavedSshEndpoint,
    pub target: CloudTarget,
    stage: Stage,
    existing: bool,
    bundle_digest: Option<String>,
    pub error: Option<String>,
}

impl Operation {
    pub fn id(&self) -> &ProfileId {
        &self.profile.id
    }
    pub fn ready(&self) -> bool {
        self.stage == Stage::Ready
    }
    fn directory(&self) -> PathBuf {
        root().join(self.id().as_str())
    }
    fn store(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::client::endpoint::store_private_json(
            &self.directory().join("operation.json"),
            &bytes,
            "Instacloud setup",
        )
    }
}

fn root() -> PathBuf {
    crate::config::state_dir().join("client/instacloud")
}

fn lock_file(path: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = match crate::platform::create_private_state_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !std::fs::symlink_metadata(path)
                .map_err(|e| e.to_string())?
                .is_file()
            {
                return Err("Invalid cloud operation lock".into());
            }
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| e.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    file.try_lock().map_err(|_| {
        "This compute is being configured by another operation. Wait for setup to finish."
            .to_owned()
    })?;
    Ok(file)
}

pub(crate) fn resource_lock(target: &CloudTarget) -> Result<std::fs::File, String> {
    let key = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\n{}\n{}",
            target.project, target.branch, target.service
        ))
    );
    lock_file(&root().join(format!("resource-{key}.lock")))
}

fn read_operation(path: &Path) -> Result<Operation, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(32769)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 32768 {
        return Err("Instacloud setup record is too large".into());
    }
    let op: Operation = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if op.version != 1 {
        return Err("Unsupported Instacloud setup version".into());
    }
    ProfileId::parse(op.id().to_string())?;
    op.target.validate()?;
    Ok(op)
}

pub(crate) fn pending(project: &str) -> Result<Vec<Operation>, String> {
    let entries = match std::fs::read_dir(root()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let path = entry.path().join("operation.json");
        if !path.exists() {
            continue;
        }
        let op = read_operation(&path)?;
        if op.target.project == project && !op.ready() {
            found.push(op);
        }
    }
    found.sort_by(|a, b| a.profile.label.cmp(&b.profile.label));
    Ok(found)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    New,
    Existing { id: String, name: String },
    Resume(ProfileId),
}

pub(crate) fn provision(
    project: &inventory::Project,
    branch: &str,
    selection: Selection,
    mut progress: impl FnMut(String),
) -> Result<SavedSshEndpoint, String> {
    // Long cloud builds must not hold the catalog lock used by Local/other remotes.
    let _setup_guard = lock_file(&root().join("setup.lock"))?;
    let mut op = match selection {
        Selection::Resume(id) => {
            let op = read_operation(&root().join(id.as_str()).join("operation.json"))?;
            if op.target.project != project.id || op.target.branch != branch {
                return Err("Setup target changed; refresh Add remote".into());
            }
            op
        }
        choice => {
            if EndpointCatalog::load_profiles()?.len() >= 64 {
                return Err("At most 64 remotes can be saved".into());
            }
            let id = ProfileId::generate();
            let (service, service_id, existing) = match choice {
                Selection::Existing { id, name } => (name, Some(id), true),
                _ => (format!("herdr-{}", &id.as_str()[..12]), None, false),
            };
            let mut profile =
                SavedSshEndpoint::new(&service, format!("{service}.insta"), "herdr-remote")?;
            profile.id = id;
            profile.enabled = false;
            let op = Operation {
                version: 1,
                profile,
                target: CloudTarget {
                    project: project.id.clone(),
                    branch: branch.into(),
                    service,
                    service_id,
                },
                stage: Stage::Planned,
                existing,
                bundle_digest: None,
                error: None,
            };
            op.target.validate()?;
            if LocationPreferences::load()?
                .remotes
                .values()
                .any(|r| r.cloud.as_ref().is_some_and(|c| c == &op.target))
            {
                return Err(
                    "This compute is already added. Select it in Settings → remotes.".into(),
                );
            }
            op.store()?;
            op
        }
    };
    let _resource_guard = resource_lock(&op.target)?;
    let result = continue_setup(&mut op, &mut progress);
    if let Err(error) = &result {
        op.error = Some(error.clone());
        if let Err(write_error) = op.store() {
            return Err(format!(
                "{error}; could not save setup status: {write_error}"
            ));
        }
    }
    result.map(|()| op.profile)
}

fn adopt_created_service(op: &mut Operation, service_id: &str) -> Result<(), String> {
    if op.stage != Stage::Creating {
        return Err(
            "Generated compute name already exists; refusing to replace an unrelated environment"
                .into(),
        );
    }
    op.target.service_id = Some(service_id.into());
    op.stage = Stage::Created;
    Ok(())
}

fn continue_setup(op: &mut Operation, progress: &mut impl FnMut(String)) -> Result<(), String> {
    op.error = None;
    progress(format!("Checking {}…", op.target.service));
    // A successful fresh list reconciles an uncertain create before any mutation.
    let services = inventory::services(&op.target.project, &op.target.branch)?;
    if op.target.service_id.is_some() {
        inventory::matching_identity(&op.target, &services)?;
    } else if let Some(existing) = services.iter().find(|s| s.name == op.target.service) {
        adopt_created_service(op, &existing.id)?;
        op.store()?;
    } else {
        prepare_bundle(op)?; // Validate the local runtime before allocating cloud resources.
        op.stage = Stage::Creating;
        op.store()?;
        progress(format!(
            "Creating {} with a persistent disk…",
            op.target.service
        ));
        let mut command = cli(
            &[
                "service",
                "add",
                "compute",
                &op.target.service,
                "--branch",
                &op.target.branch,
                "--volume",
                "10",
                "--mount-path",
                "/data",
                "--always-on",
                "--json",
            ],
            Some(&op.target.project),
        );
        let created = run_with_timeout(&mut command, Duration::from_secs(180))?;
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Create returned no service ID; resume to check its outcome")?;
        op.target.service_id = Some(id.into());
        op.stage = Stage::Created;
        op.store()?;
    }
    if !op.existing {
        save_profile(op, false)?;
    }
    if !op.existing && matches!(op.stage, Stage::Created | Stage::Deploying) {
        prepare_bundle(op)?;
        inventory::verify_identity(&op.target)?;
        op.stage = Stage::Deploying;
        op.store()?;
        progress("Deploying the Herdr environment… This can take several minutes.".into());
        let bundle = op.directory().join("runtime");
        let bundle = bundle.to_str().ok_or("Runtime path is not UTF-8")?;
        // The CLI's archive deploy is idempotent for the same target and source bytes.
        // Keep this exact bundle across interruptions instead of rebuilding it on retry.
        let mut command = cli(
            &[
                "deploy",
                bundle,
                "--branch",
                &op.target.branch,
                "--group",
                &op.target.service,
                "--port",
                "8080",
                "--json",
            ],
            Some(&op.target.project),
        );
        run_with_timeout(&mut command, Duration::from_secs(31 * 60))?;
        op.stage = Stage::Deployed;
        op.store()?;
    }
    progress("Starting compute and waiting for it to be ready…".into());
    super::operate(&op.target, CloudOperation::Start)?;
    progress("Configuring SSH access…".into());
    inventory::verify_identity(&op.target)?;
    let ssh = run(cli(
        &[
            "compute",
            "ssh",
            &op.target.service,
            "--branch",
            &op.target.branch,
            "--setup",
            "--json",
        ],
        Some(&op.target.project),
    ))?;
    if ssh.get("configured").and_then(|v| v.as_bool()) != Some(true) {
        return Err("Instacloud did not finish SSH setup; resume to try again".into());
    }
    let alias = ssh
        .get("alias")
        .and_then(|v| v.as_str())
        .ok_or("SSH setup returned no alias")?;
    crate::remote::validate_remote_target(alias).map_err(|e| e.to_string())?;
    if alias != format!("{}.insta", op.target.service) {
        return Err("Unexpected SSH alias returned by Instacloud".into());
    }
    op.profile.target = alias.into();
    op.store()?;
    progress("Checking the persistent Herdr environment…".into());
    let mut check = Command::new("ssh");
    check.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ControlMaster=no", "-o", "ControlPath=none", alias,
        "test -f /opt/herdr/runtime-v1 && test -x /usr/local/bin/herdr && python3 -c 'import os,sys; sys.exit(not os.path.ismount(\"/data\"))' && printf '{\"ready\":true}'"])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    run(check).map_err(|e| {
        format!("Compute is not a prepared Herdr environment (its image was not changed): {e}")
    })?;
    progress("Connecting to the Herdr session…".into());
    crate::remote::start_saved_ssh(&op.profile.target, &op.profile.session)
        .map_err(|e| e.to_string())?;
    crate::remote::check_saved_ssh(&op.profile.target, &op.profile.session)
        .map_err(|e| e.to_string())?;
    inventory::verify_identity(&op.target)?;
    save_profile(op, true)?;
    op.profile.enabled = true;
    op.stage = Stage::Ready;
    op.store()?;
    Ok(())
}

fn save_profile(op: &Operation, enabled: bool) -> Result<(), String> {
    let _guard = operation_lock()?;
    let mut prefs = LocationPreferences::load()?;
    let mut catalog = EndpointCatalog::load()?;
    let mut profile = op.profile.clone();
    profile.enabled = enabled;
    if let Some(current) = catalog.ssh.iter_mut().find(|p| p.id == profile.id) {
        if current.target != profile.target || current.session != profile.session {
            return Err("Remote profile changed while setup was pending".into());
        }
        *current = profile;
    } else {
        catalog.ssh.push(profile);
    }
    prefs.remotes.insert(
        op.id().clone(),
        RemoteOptions {
            cwd: "/data/workspace".into(),
            cloud: Some(op.target.clone()),
        },
    );
    prefs.store()?;
    catalog.store_profiles()
}

fn prepare_bundle(op: &mut Operation) -> Result<(), String> {
    let dir = op.directory().join("runtime");
    if let Some(expected) = &op.bundle_digest {
        if &bundle_digest(&dir)? != expected {
            return Err(
                "Saved runtime bundle changed; refusing to deploy different bytes during recovery"
                    .into(),
            );
        }
        return Ok(());
    }
    let binary = std::env::var_os("HERDR_INSTACLOUD_BINARY")
        .map(PathBuf::from)
        .map_or_else(|| std::env::current_exe().map_err(|e| e.to_string()), Ok)?;
    let mut header = [0; 20];
    std::fs::File::open(&binary)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| e.to_string())?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 || header[18..20] != [62, 0] {
        return Err("This demo deploys a Linux x86_64 build. Set HERDR_INSTACLOUD_BINARY to a compatible Herdr executable.".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::copy(binary, dir.join("herdr")).map_err(|e| e.to_string())?;
    for (name, content) in [
        ("Dockerfile", include_str!("runtime.Dockerfile")),
        ("herdr-wrapper", include_str!("herdr-wrapper")),
        ("entrypoint.py", include_str!("entrypoint.py")),
    ] {
        std::fs::write(dir.join(name), content).map_err(|e| e.to_string())?;
    }
    op.bundle_digest = Some(bundle_digest(&dir)?);
    op.store()
}

fn bundle_digest(dir: &Path) -> Result<String, String> {
    let expected = ["Dockerfile", "entrypoint.py", "herdr", "herdr-wrapper"];
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("Runtime bundle contains a symlink or non-file".into());
        }
        found.push(entry.file_name().to_string_lossy().into_owned());
    }
    found.sort();
    if found != expected {
        return Err("Runtime bundle contains unexpected or missing files".into());
    }
    let mut hash = Sha256::new();
    for name in expected {
        hash.update(name);
        let mut file = std::fs::File::open(dir.join(name)).map_err(|e| e.to_string())?;
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(stage: Stage) -> Operation {
        Operation {
            version: 1,
            profile: SavedSshEndpoint::new("remote", "remote.insta", "herdr-remote").unwrap(),
            target: CloudTarget {
                project: "p".into(),
                branch: "main".into(),
                service: "remote".into(),
                service_id: None,
            },
            stage,
            existing: false,
            bundle_digest: None,
            error: None,
        }
    }

    #[test]
    fn lost_create_response_adopts_the_resource_and_preserves_profile_identity() {
        let op = operation(Stage::Creating);
        let bytes = serde_json::to_vec(&op).unwrap();
        let mut restored: Operation = serde_json::from_slice(&bytes).unwrap();
        adopt_created_service(&mut restored, "service-id").unwrap();
        assert_eq!(restored.profile.id, op.profile.id);
        assert_eq!(restored.target.service, op.target.service);
        assert_eq!(restored.target.service_id.as_deref(), Some("service-id"));
        assert_eq!(restored.stage, Stage::Created);
    }

    #[test]
    fn never_issued_create_does_not_adopt_an_existing_compute() {
        let mut op = operation(Stage::Planned);
        assert!(adopt_created_service(&mut op, "unrelated").is_err());
        assert!(op.target.service_id.is_none());
    }

    #[test]
    fn deployment_recovery_checks_the_whole_source_bundle() {
        let root =
            std::env::temp_dir().join(format!("herdr-cloud-bundle-{}", ProfileId::generate()));
        std::fs::create_dir_all(&root).unwrap();
        for name in ["Dockerfile", "entrypoint.py", "herdr", "herdr-wrapper"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let digest = bundle_digest(&root).unwrap();
        std::fs::write(root.join(".dockerignore"), "herdr").unwrap();
        assert!(bundle_digest(&root).is_err());
        std::fs::remove_file(root.join(".dockerignore")).unwrap();
        assert_eq!(bundle_digest(&root).unwrap(), digest);
        std::fs::write(root.join("herdr"), "different version").unwrap();
        assert_ne!(bundle_digest(&root).unwrap(), digest);
        std::fs::remove_dir_all(root).unwrap();
    }
}
