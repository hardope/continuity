//! Device identity, pairing, and trust for the Continue continuity
//! protocol. See `docs/protocol.md` for the overall security model:
//! trust-on-first-use pairing with a human-verified confirmation code,
//! followed by pinned-pubkey TLS for every subsequent connection.

mod hash;
mod identity;
mod pairing;
mod tls;
mod trust;

pub use hash::{content_hash, IncrementalHash};
pub use identity::{Identity, IdentityError};
pub use pairing::confirmation_code;
pub use tls::{generate_self_signed, TlsIdentity, TlsIdentityError};
pub use trust::{TrustError, TrustStore, TrustedDevice};
