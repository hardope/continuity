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
    identify(conn, my_device, None).await
}

/// `announce_and_identify` for an inbound connection whose first message
/// the accepting side has already read — `first` takes the place of the
/// peer's `DeviceAnnounce`. Needed because an inbound connection from a
/// peer this device is expecting a v3 file data connection from has to be
/// read before it can be told apart from a mesh reconnect (see the accept
/// loop in `continuity-daemon`). Safe in that order: the dialing side
/// always writes its `DeviceAnnounce` before reading anything, so reading
/// it first can't deadlock.
pub async fn announce_and_identify_after(
    conn: &mut Connection,
    my_device: &DeviceInfo,
    first: Message,
) -> Result<DeviceInfo, PairingError> {
    identify(conn, my_device, Some(first)).await
}

async fn identify(conn: &mut Connection, my_device: &DeviceInfo, first: Option<Message>) -> Result<DeviceInfo, PairingError> {
    write_message(
        conn,
        &Message::DeviceAnnounce {
            device: my_device.clone(),
        },
    )
    .await?;

    let first = match first {
        Some(message) => message,
        None => read_message(conn).await?,
    };
    let peer = match first {
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

/// `start_pairing` for an inbound connection whose first message was
/// already read — see `announce_and_identify_after`.
pub async fn start_pairing_after(
    mut conn: Connection,
    my_device: &DeviceInfo,
    first: Message,
) -> Result<PendingPairing, PairingError> {
    let peer = announce_and_identify_after(&mut conn, my_device, first).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{connect, Listener};
    use continuity_crypto::{generate_self_signed, Identity};
    use continuity_proto::{Platform, PROTOCOL_VERSION};

    fn device(identity: &Identity, name: &str) -> DeviceInfo {
        DeviceInfo { id: identity.device_id(), name: name.into(), platform: Platform::Linux, protocol_version: PROTOCOL_VERSION }
    }

    /// The accepting side reading the dialer's `DeviceAnnounce` *before*
    /// sending its own (what the accept loop does when it has to look at a
    /// connection's first message to tell a file data connection from a
    /// mesh reconnect) still completes the same handshake on both ends.
    #[tokio::test]
    async fn identifying_after_reading_the_first_message_completes_the_handshake() {
        let dialer_identity = Identity::generate();
        let accepter_identity = Identity::generate();
        let dialer_device = device(&dialer_identity, "dialer");
        let accepter_device = device(&accepter_identity, "accepter");
        let dialer_tls = generate_self_signed(&dialer_identity).unwrap();
        let accepter_tls = generate_self_signed(&accepter_identity).unwrap();

        let listener = Listener::bind("127.0.0.1:0".parse().unwrap(), &accepter_tls).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dialer = tokio::spawn(async move {
            let mut conn = connect(addr, &dialer_tls).await.unwrap();
            announce_and_identify(&mut conn, &dialer_device).await.unwrap()
        });

        let (mut conn, _) = listener.accept().await.unwrap();
        let first = read_message(&mut conn).await.unwrap();
        let seen_by_accepter = announce_and_identify_after(&mut conn, &accepter_device, first).await.unwrap();
        assert_eq!(seen_by_accepter.id, dialer_identity.device_id());
        assert_eq!(dialer.await.unwrap().id, accepter_identity.device_id());
    }
}
