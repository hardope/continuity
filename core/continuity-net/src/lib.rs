//! Discovery, transport, and pairing primitives for the Continue
//! continuity protocol. Deliberately unopinionated about orchestration —
//! each shell (the `continuityctl` CLI today, `continuityd`'s tray app and
//! the mobile FFI layer later) drives these building blocks with whatever
//! event loop fits its platform.

mod connection;
mod discovery;
mod framing;
mod pairing;

pub use connection::{connect, Connection, ConnectionError, Listener};
pub use discovery::{peer_from_service_info, DiscoveredPeer, Discovery, DiscoveryError, SERVICE_TYPE};
pub use framing::{read_frame, read_message, write_frame, write_message, FramingError};
pub use pairing::{announce_and_identify, start_pairing, PairingError, PendingPairing};

// Re-exported so downstream crates don't need a direct `mdns-sd` dependency
// just to match on discovery events.
pub use mdns_sd::ServiceEvent;
