use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
const KEYRING_SERVICE: &str = "app.continuity.identity";
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
const KEYRING_USER: &str = "device-signing-key";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    // Android has no keyring crate backend at all (its `target_os` is
    // "android", not "linux", despite the shared kernel — see
    // continuity-crypto/Cargo.toml's target-specific keyring deps) and iOS
    // identities never touch this crate's keyring code path either way
    // (continuity-ffi uses `from_pkcs8_der` — the host app owns storage via
    // the real iOS Keychain). This variant, and `load_or_create` below,
    // only exist where a backend is actually compiled in.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[error("keyring access failed: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("stored key was not valid PKCS8: {0}")]
    Pkcs8(#[from] ed25519_dalek::pkcs8::Error),
}

/// A device's permanent Ed25519 identity. The public key doubles as the
/// device's id on the wire (`DeviceId`); the private key never leaves this
/// struct except to feed TLS certificate generation.
pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    /// Generates a fresh identity without touching the keychain — for
    /// tests and other ephemeral uses (nothing persisted, callers own
    /// storage themselves).
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Rebuilds an identity from PKCS8 DER bytes — the counterpart to
    /// `to_pkcs8_der()`, for callers that own storage themselves (the FFI
    /// layer, since Android/iOS have no equivalent of a desktop OS
    /// keychain reachable from Rust; the host app persists these bytes in
    /// Android Keystore-backed storage or the real iOS Keychain instead).
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, IdentityError> {
        Ok(Self {
            signing_key: SigningKey::from_pkcs8_der(der)?,
        })
    }

    /// Loads the identity from the OS keychain/credential store, generating
    /// and persisting a new one on first run.
    ///
    /// `profile` scopes the keychain entry, so a single machine can host
    /// more than one identity — used for exercising the multi-device
    /// protocol from one dev box without needing separate hardware. Real
    /// deployments always use `"default"`.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pub fn load_or_create(profile: &str) -> Result<Self, IdentityError> {
        let service = format!("{KEYRING_SERVICE}.{profile}");
        let entry = keyring::Entry::new(&service, KEYRING_USER)?;

        match entry.get_secret() {
            Ok(der_bytes) => {
                let signing_key = SigningKey::from_pkcs8_der(&der_bytes)?;
                Ok(Self { signing_key })
            }
            Err(keyring::Error::NoEntry) => {
                let signing_key = SigningKey::generate(&mut OsRng);
                let der = signing_key.to_pkcs8_der()?;
                entry.set_secret(der.as_bytes())?;
                Ok(Self { signing_key })
            }
            Err(other) => Err(other.into()),
        }
    }

    /// Hex-encoded Ed25519 public key — this is the `DeviceId` used
    /// throughout the protocol and trust store.
    pub fn device_id(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// PKCS8 DER encoding of the private key, used to generate a TLS
    /// certificate tied to this identity (see `continuity_crypto::tls`).
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, IdentityError> {
        Ok(self.signing_key.to_pkcs8_der()?.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_hex_pubkey() {
        let identity = Identity::generate();
        let id = identity.device_id();
        assert_eq!(id.len(), 64); // 32 bytes hex-encoded
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
