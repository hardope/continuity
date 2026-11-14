//! Discovery, transport, and pairing primitives for the Continue
//! continuity protocol. Deliberately unopinionated about orchestration —
//! each shell (the `continuityctl` CLI today, `continuityd`'s tray app and
//! the mobile FFI layer later) drives these building blocks with whatever
//! event loop fits its platform.

mod connection;
mod discovery;
mod framing;
mod pairing;

pub use connection::{connect, Connection, ConnectionError, Incoming, Listener};
pub use discovery::{peer_from_service_info, DiscoveredPeer, Discovery, DiscoveryError, SERVICE_TYPE};
pub use framing::{
    read_frame, read_frame_up_to, read_message, read_message_up_to, write_frame, write_frame_unflushed, write_message, FramingError,
    HANDSHAKE_MAX_MESSAGE_BYTES,
};
pub use pairing::{
    announce_and_identify, announce_and_identify_after, start_pairing, Announcement, PairingError, PendingPairing,
    COMMITTED_PAIRING_VERSION,
};

// Re-exported so downstream crates don't need a direct `mdns-sd` dependency
// just to match on discovery events.
pub use mdns_sd::ServiceEvent;
