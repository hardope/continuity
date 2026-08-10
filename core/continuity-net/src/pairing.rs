use crate::connection::{Connection, ConnectionError};
use crate::framing::{read_message, write_message, FramingError};
use continuity_proto::{DeviceInfo, Message};

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Framing(#[from] FramingError),
    #[error("peer sent an unexpected message during pairing: {0:?}")]
    UnexpectedMessage(Message),
    #[error("peer's announced device id didn't match its TLS certificate — possible tampering")]
    IdentityMismatch,
}

/// A pairing handshake that has exchanged identities and computed the
/// human-verifiable confirmation code, waiting on the local user (and the
/// remote user, independently) to confirm it matches what's shown on the
/// other screen.
pub struct PendingPairing {
    pub peer: DeviceInfo,
    pub code: String,
    conn: Connection,
}

/// Exchanges `DeviceAnnounce` in both directions and cross-checks the
/// announced id against the cryptographic identity the TLS handshake
/// already proved (peer *must* control the private key matching the cert
/// it presented). Used both as the first step of pairing and, for already
/// -trusted peers, as the whole handshake — no code confirmation needed
/// once a device is in the trust store.
pub async fn announce_and_identify(
    conn: &mut Connection,
    my_device: &DeviceInfo,
) -> Result<DeviceInfo, PairingError> {
    write_message(
        conn,
        &Message::DeviceAnnounce {
            device: my_device.clone(),
        },
    )
    .await?;

    let peer = match read_message(conn).await? {
        Message::DeviceAnnounce { device } => device,
        other => return Err(PairingError::UnexpectedMessage(other)),
    };

    let cryptographic_peer_id = conn.peer_device_id()?;
    if peer.id != cryptographic_peer_id {
        return Err(PairingError::IdentityMismatch);
    }

    Ok(peer)
}

/// Runs the symmetric first half of pairing over a freshly-established
/// connection: identifies the peer via `announce_and_identify`, then
/// derives the confirmation code both sides will show independently.
/// Works identically whether this side dialed or accepted the connection.
pub async fn start_pairing(
    mut conn: Connection,
    my_device: &DeviceInfo,
) -> Result<PendingPairing, PairingError> {
    let peer = announce_and_identify(&mut conn, my_device).await?;
    let code = continuity_crypto::confirmation_code(my_device.id.as_bytes(), peer.id.as_bytes());
    Ok(PendingPairing { peer, code, conn })
}

impl PendingPairing {
    /// Sends our confirmation decision and waits for the peer's. Pairing
    /// only succeeds if *both* sides confirm — either side declining (or
    /// the codes not having matched, which the caller must check with the
    /// user before calling this) aborts it.
    pub async fn confirm(mut self, accepted: bool) -> Result<Option<(Connection, DeviceInfo)>, PairingError> {
        write_message(&mut self.conn, &Message::PairConfirm { accepted }).await?;

        if !accepted {
            return Ok(None);
        }

        match read_message(&mut self.conn).await? {
            Message::PairConfirm { accepted: true } => Ok(Some((self.conn, self.peer))),
            Message::PairConfirm { accepted: false } => Ok(None),
            other => Err(PairingError::UnexpectedMessage(other)),
        }
    }
}
