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
    /// Peers explicitly allowed, at least once, to remote-control this
    /// device — see `TrustStore::allow_remote_control`'s doc comment.
    /// `#[serde(default)]` so a trust file written before this field
    /// existed just deserializes as "nobody's allowed yet" instead of
    /// failing to load.
    #[serde(default)]
    remote_control_allowed: std::collections::HashSet<String>,
    /// Peers allowed to unlock this device's screen — see
    /// `TrustStore::set_unlock_allowed`. `#[serde(default)]` for the same
    /// reason as `remote_control_allowed`.
    #[serde(default)]
    unlock_allowed: std::collections::HashSet<String>,
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
        self.file.remote_control_allowed.remove(device_id);
        self.file.unlock_allowed.remove(device_id);
        self.save()
    }

    /// Forgets every paired device — a factory reset. Every previously
    /// trusted peer will need to be paired again from scratch.
    pub fn clear(&mut self) -> Result<(), TrustError> {
        self.file.devices.clear();
        self.file.remote_control_allowed.clear();
        self.file.unlock_allowed.clear();
        self.save()
    }

    /// Whether this peer has already been explicitly allowed, at least
    /// once, to remote-control this device — see `allow_remote_control`'s
    /// doc comment for the flow this supports. `false` for a peer that's
    /// never asked, was denied last time, or isn't paired at all.
    pub fn is_remote_control_allowed(&self, device_id: &str) -> bool {
        self.file.remote_control_allowed.contains(device_id)
    }

    /// Remembers that this peer was explicitly allowed to remote-control
    /// this device, so a future request from it skips the confirmation
    /// prompt and is accepted automatically — the same "ask once, then
    /// trust" shape reconnecting to an already-paired device already has.
    /// Deliberately opt-in: only ever called from a real "Allow" click
    /// (see `EngineCommand::RespondToRemoteControlRequest`'s handling in
    /// `engine.rs`), never implied by pairing trust alone or by a request
    /// merely arriving — remote control grants full keyboard/mouse/screen
    /// access, a materially bigger risk than clipboard/file sync, so it
    /// gets its own, narrower trust flag rather than riding along with
    /// `trust()`. `revoke`/`clear` above both also drop this — forgetting
    /// a device forgets everything about it, including this.
    pub fn allow_remote_control(&mut self, device_id: &str) -> Result<(), TrustError> {
        self.file.remote_control_allowed.insert(device_id.to_string());
        self.save()
    }

    /// Whether this peer may unlock this device's screen remotely — `false`
    /// unless the local user has explicitly turned it on for this exact
    /// peer via `set_unlock_allowed`.
    pub fn is_unlock_allowed(&self, device_id: &str) -> bool {
        self.file.unlock_allowed.contains(device_id)
    }

    /// Ids of every peer currently allowed to unlock this device.
    pub fn unlock_allowed_devices(&self) -> impl Iterator<Item = &String> {
        self.file.unlock_allowed.iter()
    }

    /// Turns remote unlock on or off for one paired peer. Off by default and
    /// never implied by anything else — not by pairing, and not by remote
    /// control consent: unlocking skips this device's own password
    /// entirely, so it needs its own explicit, local opt-in (the host
    /// shell should confirm before turning it on). Like remote control
    /// trust, `revoke`/`clear` drop it too. Allowing a peer that isn't
    /// paired is refused rather than stored, so a stale grant can never
    /// outlive the pairing it was given under.
    pub fn set_unlock_allowed(&mut self, device_id: &str, allowed: bool) -> Result<(), TrustError> {
        if allowed {
            if !self.is_trusted(device_id) {
                return Ok(());
            }
            self.file.unlock_allowed.insert(device_id.to_string());
        } else {
            self.file.unlock_allowed.remove(device_id);
        }
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

    #[test]
    fn remote_control_trust_persists_across_reload_and_is_scoped_per_peer() {
        let (mut store, dir) = temp_store();
        let path = store_path(&store);
        store
            .trust(TrustedDevice { id: "abc123".into(), name: "Test MacBook".into(), paired_at_unix: 1 })
            .unwrap();
        store
            .trust(TrustedDevice { id: "def456".into(), name: "Test iMac".into(), paired_at_unix: 2 })
            .unwrap();
        assert!(!store.is_remote_control_allowed("abc123"), "not allowed until explicitly granted");

        store.allow_remote_control("abc123").unwrap();
        assert!(store.is_remote_control_allowed("abc123"));
        assert!(!store.is_remote_control_allowed("def456"), "granting one peer shouldn't grant another");

        let reloaded = TrustStore::load(path).unwrap();
        assert!(reloaded.is_remote_control_allowed("abc123"));
        assert!(!reloaded.is_remote_control_allowed("def456"));
        drop(dir);
    }

    #[test]
    fn revoking_a_device_also_clears_its_remote_control_trust() {
        let (mut store, _dir) = temp_store();
        store
            .trust(TrustedDevice { id: "abc123".into(), name: "Test MacBook".into(), paired_at_unix: 1 })
            .unwrap();
        store.allow_remote_control("abc123").unwrap();
        assert!(store.is_remote_control_allowed("abc123"));

        store.revoke("abc123").unwrap();
        assert!(!store.is_remote_control_allowed("abc123"), "forgetting a device should forget its remote-control trust too");
    }

    #[test]
    fn unlock_permission_is_opt_in_per_peer_persists_and_is_cleared_on_revoke() {
        let (mut store, dir) = temp_store();
        let path = store_path(&store);
        store
            .trust(TrustedDevice { id: "abc123".into(), name: "Test Pixel".into(), paired_at_unix: 1 })
            .unwrap();
        store
            .trust(TrustedDevice { id: "def456".into(), name: "Test iPhone".into(), paired_at_unix: 2 })
            .unwrap();
        assert!(!store.is_unlock_allowed("abc123"), "off until explicitly allowed");
        store.allow_remote_control("abc123").unwrap();
        assert!(!store.is_unlock_allowed("abc123"), "remote control consent must not imply unlock");

        store.set_unlock_allowed("abc123", true).unwrap();
        assert!(store.is_unlock_allowed("abc123"));
        assert!(!store.is_unlock_allowed("def456"), "allowing one peer shouldn't allow another");

        let mut reloaded = TrustStore::load(path).unwrap();
        assert!(reloaded.is_unlock_allowed("abc123"));

        reloaded.set_unlock_allowed("abc123", false).unwrap();
        assert!(!reloaded.is_unlock_allowed("abc123"));

        reloaded.set_unlock_allowed("def456", true).unwrap();
        reloaded.revoke("def456").unwrap();
        assert!(!reloaded.is_unlock_allowed("def456"), "forgetting a device should forget its unlock permission too");
        drop(dir);
    }

    #[test]
    fn unlock_cannot_be_allowed_for_an_unpaired_device() {
        let (mut store, _dir) = temp_store();
        store.set_unlock_allowed("stranger", true).unwrap();
        assert!(!store.is_unlock_allowed("stranger"));
    }

    fn store_path(store: &TrustStore) -> PathBuf {
        store.path.clone()
    }
}
