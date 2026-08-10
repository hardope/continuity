use crate::identity::{Identity, IdentityError};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

#[derive(Debug, thiserror::Error)]
pub enum TlsIdentityError {
    #[error("failed to build TLS certificate from identity: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// A self-signed TLS certificate whose keypair *is* the device's Ed25519
/// identity. There's no CA — trust comes entirely from the pinned-pubkey
/// `TrustStore`, not from certificate validation, so the cert only needs to
/// bind the identity key to something rustls can present on the wire.
pub struct TlsIdentity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

pub fn generate_self_signed(identity: &Identity) -> Result<TlsIdentity, TlsIdentityError> {
    let pkcs8_der = identity.to_pkcs8_der()?;
    let key_pair = KeyPair::try_from(pkcs8_der.as_slice())?;

    let mut params = CertificateParams::new(vec![identity.device_id()])?;
    params.distinguished_name = DistinguishedName::new();

    let cert = params.self_signed(&key_pair)?;

    Ok(TlsIdentity {
        cert_der: cert.der().clone(),
        key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8_der)),
    })
}
