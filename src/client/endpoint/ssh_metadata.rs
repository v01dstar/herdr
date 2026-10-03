use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ProfileId;

const MAX_METADATA_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SshMachineMetadata {
    pub(crate) os: String,
    pub(crate) executable: String,
}

impl SshMachineMetadata {
    pub(crate) fn is_valid(&self) -> bool {
        let path = &self.executable;
        if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
            return false;
        }
        match self.os.as_str() {
            "linux" | "macos" => path.starts_with('/') && !path.ends_with("/mise/shims/herdr"),
            "windows" => {
                let bytes = path.as_bytes();
                path.starts_with(r"\\")
                    || (bytes.len() >= 3
                        && bytes[0].is_ascii_alphabetic()
                        && bytes[1] == b':'
                        && matches!(bytes[2], b'\\' | b'/'))
            }
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredMetadata {
    version: u32,
    target: String,
    session: String,
    metadata: SshMachineMetadata,
}

pub(crate) struct SshMetadataCache {
    path: PathBuf,
    target: String,
    session: String,
}

impl SshMetadataCache {
    pub(crate) fn new(profile_id: &str, target: &str, session: &str) -> io::Result<Self> {
        let id = ProfileId::parse(profile_id)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(Self {
            path: metadata_dir().join(format!("{id}.json")),
            target: target.to_owned(),
            session: session.to_owned(),
        })
    }

    pub(crate) fn load(&self) -> Option<SshMachineMetadata> {
        load_metadata(&self.path, &self.target, &self.session)
    }

    pub(crate) fn store(&self, metadata: &SshMachineMetadata) {
        if !metadata.is_valid() {
            return;
        }
        let stored = StoredMetadata {
            version: 1,
            target: self.target.clone(),
            session: self.session.clone(),
            metadata: metadata.clone(),
        };
        let result = serde_json::to_vec(&stored)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                super::catalog::store_private_json(&self.path, &bytes, "SSH metadata")
            });
        if let Err(error) = result {
            tracing::debug!(%error, "could not cache SSH machine metadata");
        }
    }

    pub(crate) fn invalidate(&self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(%error, "could not invalidate SSH machine metadata");
            }
        }
    }
}

fn metadata_dir() -> PathBuf {
    crate::config::state_dir().join("client/ssh-metadata")
}

/// Removes cached metadata for every profile and session that used `target`,
/// including entries left behind by profiles removed earlier.
pub(crate) fn invalidate_target(target: &str) {
    if let Err(error) = invalidate_target_in(&metadata_dir(), target) {
        tracing::debug!(%error, "could not invalidate SSH metadata for a removed target");
    }
}

fn invalidate_target_in(dir: &Path, target: &str) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        let Some(stored) = read_stored(&path) else {
            continue;
        };
        if stored.target == target {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn read_stored(path: &Path) -> Option<StoredMetadata> {
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn load_metadata(path: &Path, target: &str, session: &str) -> Option<SshMachineMetadata> {
    let stored = read_stored(path)?;
    (stored.version == 1
        && stored.target == target
        && stored.session == session
        && stored.metadata.is_valid())
    .then_some(stored.metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_accepts_only_supported_platforms_and_absolute_paths() {
        for (os, path, valid) in [
            ("linux", "/home/a b/herdr", true),
            ("macos", "/opt/homebrew/bin/herdr", true),
            ("windows", r"C:\Program Files\herdr.exe", true),
            ("windows", r"\\server\share\herdr.exe", true),
            ("windows", "herdr.exe", false),
            ("linux", "$HOME/.local/bin/herdr", false),
            ("linux", "/home/user/.local/share/mise/shims/herdr", false),
            ("linux", "/bin/herdr\nmalformed", false),
            ("unknown", "/bin/herdr", false),
        ] {
            assert_eq!(
                SshMachineMetadata {
                    os: os.into(),
                    executable: path.into()
                }
                .is_valid(),
                valid,
                "{os}: {path}"
            );
        }
    }

    #[test]
    fn metadata_is_disposable_fingerprinted_and_independent_per_profile() {
        let root = std::env::temp_dir().join(format!("herdr-ssh-metadata-{}", std::process::id()));
        let first = SshMetadataCache {
            path: root.join("first.json"),
            target: "mac".into(),
            session: "fleet".into(),
        };
        let second = SshMetadataCache {
            path: root.join("second.json"),
            target: "mac".into(),
            session: "fleet".into(),
        };
        let metadata = SshMachineMetadata {
            os: "macos".into(),
            executable: "/some path/herdr".into(),
        };
        assert!(first.load().is_none());
        first.store(&metadata);
        second.store(&metadata);
        assert_eq!(first.load(), Some(metadata.clone()));
        assert!(load_metadata(&first.path, "different-host", "fleet").is_none());
        assert!(load_metadata(&first.path, "mac", "different-session").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first.path).unwrap()).unwrap();
        stored["future_field"] = true.into();
        std::fs::write(&first.path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert_eq!(first.load(), Some(metadata.clone()));
        stored["version"] = 2.into();
        std::fs::write(&first.path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(first.load().is_none());
        for bytes in [
            b"broken".to_vec(),
            vec![b' '; MAX_METADATA_BYTES as usize + 1],
        ] {
            std::fs::write(&first.path, bytes).unwrap();
            assert!(first.load().is_none());
        }
        first.invalidate();
        assert_eq!(second.load(), Some(metadata));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalidating_a_target_removes_only_its_entries() {
        let root =
            std::env::temp_dir().join(format!("herdr-ssh-metadata-target-{}", std::process::id()));
        let metadata = SshMachineMetadata {
            os: "linux".into(),
            executable: "/usr/bin/herdr".into(),
        };
        let cache = |name: &str, target: &str, session: &str| SshMetadataCache {
            path: root.join(format!("{name}.json")),
            target: target.into(),
            session: session.into(),
        };
        let gone = cache("gone", "hangar-m_gone", "herdr-remote");
        let gone_other_session = cache("gone2", "hangar-m_gone", "agents");
        let kept = cache("kept", "hangar-m_kept", "herdr-remote");
        for entry in [&gone, &gone_other_session, &kept] {
            entry.store(&metadata);
        }
        std::fs::write(root.join("broken.json"), "broken").unwrap();
        invalidate_target_in(&root, "hangar-m_gone").unwrap();
        assert!(!gone.path.exists());
        assert!(!gone_other_session.path.exists());
        assert_eq!(kept.load(), Some(metadata));
        assert!(root.join("broken.json").exists());
        invalidate_target_in(&root.join("missing"), "x").unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn metadata_does_not_follow_symlinks() {
        let root =
            std::env::temp_dir().join(format!("herdr-ssh-metadata-link-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let cache = SshMetadataCache {
            path: root.join("cache.json"),
            target: "mac".into(),
            session: "fleet".into(),
        };
        let other = root.join("other");
        std::fs::write(&other, "untouched").unwrap();
        std::os::unix::fs::symlink(&other, &cache.path).unwrap();
        assert!(cache.load().is_none());
        cache.store(&SshMachineMetadata {
            os: "macos".into(),
            executable: "/bin/herdr".into(),
        });
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "untouched");
        std::fs::remove_dir_all(root).unwrap();
    }
}
