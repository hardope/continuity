use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("could not determine config directory")]
    NoConfigDir,
    #[error("io error reading/writing trust store: {0}")]
    Io(#[from] std::io::Error),
    #[error("trust store is corrupt: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    pub paired_at_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    devices: HashMap<String, TrustedDevice>,
}

/// The set of paired devices this one accepts connections from — the whole
/// security boundary given there's no separate account/auth layer. A device
/// id (hex Ed25519 public key) that isn't in here is rejected at the TLS
/// layer before any message is processed.
pub struct TrustStore {
    path: PathBuf,
    file: TrustFile,
}

impl TrustStore {
    /// `profile` scopes the trust store file, mirroring
    /// `Identity::load_or_create` — lets one dev machine host multiple
    /// independent "devices" for local testing. Real deployments always
    /// use `"default"`.
    pub fn default_path(profile: &str) -> Result<PathBuf, TrustError> {
        let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")
            .ok_or(TrustError::NoConfigDir)?;
        let file_name = format!("trusted_devices.{profile}.json");
        Ok(dirs.config_dir().join(file_name))
    }

    pub fn load_default(profile: &str) -> Result<Self, TrustError> {
        Self::load(Self::default_path(profile)?)
    }

    pub fn load(path: PathBuf) -> Result<Self, TrustError> {
        let file = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw)?
        } else {
            TrustFile::default()
        };
        Ok(Self { path, file })
    }

    pub fn is_trusted(&self, device_id: &str) -> bool {
        self.file.devices.contains_key(device_id)
    }

    pub fn get(&self, device_id: &str) -> Option<&TrustedDevice> {
        self.file.devices.get(device_id)
    }

    pub fn list(&self) -> impl Iterator<Item = &TrustedDevice> {
        self.file.devices.values()
    }

    pub fn trust(&mut self, device: TrustedDevice) -> Result<(), TrustError> {
        self.file.devices.insert(device.id.clone(), device);
        self.save()
    }

    pub fn revoke(&mut self, device_id: &str) -> Result<(), TrustError> {
        self.file.devices.remove(device_id);
        self.save()
    }

    fn save(&self) -> Result<(), TrustError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.file)?;
        std::fs::write(&self.path, raw)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (TrustStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_devices.json");
        (TrustStore::load(path).unwrap(), dir)
    }

    #[test]
    fn trusting_a_device_persists_across_reload() {
        let (mut store, dir) = temp_store();
        let path = store_path(&store);
        store
            .trust(TrustedDevice {
                id: "abc123".into(),
                name: "Test MacBook".into(),
                paired_at_unix: 1,
            })
            .unwrap();

        let reloaded = TrustStore::load(path).unwrap();
        assert!(reloaded.is_trusted("abc123"));
        drop(dir);
    }

    #[test]
    fn revoking_removes_trust() {
        let (mut store, _dir) = temp_store();
        store
            .trust(TrustedDevice {
                id: "abc123".into(),
                name: "Test MacBook".into(),
                paired_at_unix: 1,
            })
            .unwrap();
        assert!(store.is_trusted("abc123"));

        store.revoke("abc123").unwrap();
        assert!(!store.is_trusted("abc123"));
    }

    fn store_path(store: &TrustStore) -> PathBuf {
        store.path.clone()
    }
}
