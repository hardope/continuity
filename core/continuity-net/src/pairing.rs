use crate::connection::{Connection, ConnectionError};
use crate::framing::{read_message_up_to, write_message, FramingError, HANDSHAKE_MAX_MESSAGE_BYTES};
use continuity_crypto::{confirmation_code, confirmation_code_with_nonces, pairing_commitment, PAIRING_NONCE_BYTES};
use continuity_proto::{DeviceInfo, Message};

/// The protocol version from which the pairing code is derived from fresh,
/// committed nonces (`PairCommit`/`PairReveal`) rather than from the two
/// long-term keys alone — see `continuity_crypto::confirmation_code_with_nonces`
/// for why that matters. Both peers have to announce at least this; with an
/// older peer the old code is used.
pub const COMMITTED_PAIRING_VERSION: u32 = 4;

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
    #[error("peer's revealed pairing nonce doesn't match what it committed to — possible tampering")]
    CommitmentMismatch,
    #[error("peer sent a malformed pairing {0}")]
    Malformed(&'static str),
}

/// What the peer said about itself in its `DeviceAnnounce`.
#[derive(Debug, Clone)]
pub struct Announcement {
    pub peer: DeviceInfo,
    /// Whether the peer already trusts this device — `None` from a peer
    /// older than v4, which doesn't say.
    pub trusts_me: Option<bool>,
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
/// once a device is in the trust store. `trusts_peer` is what this side
/// tells the peer about its own trust store.
pub async fn announce_and_identify(
    conn: &mut Connection,
    my_device: &DeviceInfo,
    trusts_peer: bool,
) -> Result<Announcement, PairingError> {
    identify(conn, my_device, trusts_peer, None).await
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
    trusts_peer: bool,
    first: Message,
) -> Result<Announcement, PairingError> {
    identify(conn, my_device, trusts_peer, Some(first)).await
}

async fn identify(
    conn: &mut Connection,
    my_device: &DeviceInfo,
    trusts_peer: bool,
    first: Option<Message>,
) -> Result<Announcement, PairingError> {
    write_message(conn, &Message::DeviceAnnounce { device: my_device.clone(), trusts_peer: Some(trusts_peer) }).await?;

    let first = match first {
        Some(message) => message,
        // Nobody is trusted yet: a small message, or none.
        None => read_message_up_to(conn, HANDSHAKE_MAX_MESSAGE_BYTES).await?,
    };
    let (peer, trusts_me) = match first {
        Message::DeviceAnnounce { device, trusts_peer } => (device, trusts_peer),
        other => return Err(PairingError::UnexpectedMessage(other)),
    };

    let cryptographic_peer_id = conn.peer_device_id()?;
    if peer.id != cryptographic_peer_id {
        return Err(PairingError::IdentityMismatch);
    }

    Ok(Announcement { peer, trusts_me })
}

/// Runs the rest of pairing over a connection whose identities
/// `announce_and_identify` has already exchanged: derives the confirmation
/// code both sides will show independently. With a peer that announced
/// `COMMITTED_PAIRING_VERSION` or newer that's a commit-then-reveal
/// exchange of fresh nonces (`PairCommit`, `PairReveal`); with an older
/// one, the old key-fingerprint code. Works identically whether this side
/// dialed or accepted the connection.
pub async fn start_pairing(mut conn: Connection, my_device: &DeviceInfo, peer: DeviceInfo) -> Result<PendingPairing, PairingError> {
    let code = if my_device.protocol_version >= COMMITTED_PAIRING_VERSION && peer.protocol_version >= COMMITTED_PAIRING_VERSION {
        committed_code(&mut conn, my_device, &peer).await?
    } else {
        confirmation_code(my_device.id.as_bytes(), peer.id.as_bytes())
    };
    Ok(PendingPairing { peer, code, conn })
}

/// Both sides: commit to a fresh nonce, read the peer's commitment, only
/// then reveal, read the peer's nonce, check it against its commitment.
/// Each side writes before it reads at every step, so the two can't wait on
/// each other.
async fn committed_code(conn: &mut Connection, my_device: &DeviceInfo, peer: &DeviceInfo) -> Result<String, PairingError> {
    let nonce: [u8; PAIRING_NONCE_BYTES] = rand::random();
    write_message(conn, &Message::PairCommit { commitment: pairing_commitment(&my_device.id, &nonce) }).await?;
    let peer_commitment = match read_message_up_to(conn, HANDSHAKE_MAX_MESSAGE_BYTES).await? {
        Message::PairCommit { commitment } => commitment,
        other => return Err(PairingError::UnexpectedMessage(other)),
    };
    if peer_commitment.len() != 32 {
        return Err(PairingError::Malformed("commitment"));
    }

    write_message(conn, &Message::PairReveal { nonce: nonce.to_vec() }).await?;
    let peer_nonce = match read_message_up_to(conn, HANDSHAKE_MAX_MESSAGE_BYTES).await? {
        Message::PairReveal { nonce } => nonce,
        other => return Err(PairingError::UnexpectedMessage(other)),
    };
    if peer_nonce.len() != PAIRING_NONCE_BYTES {
        return Err(PairingError::Malformed("nonce"));
    }
    if pairing_commitment(&peer.id, &peer_nonce) != peer_commitment {
        return Err(PairingError::CommitmentMismatch);
    }
    Ok(confirmation_code_with_nonces(&my_device.id, &nonce, &peer.id, &peer_nonce))
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

        match read_message_up_to(&mut self.conn, HANDSHAKE_MAX_MESSAGE_BYTES).await? {
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
    use crate::framing::read_message;
    use continuity_crypto::{generate_self_signed, Identity, TlsIdentity};
    use continuity_proto::{Platform, PROTOCOL_VERSION};
    use std::net::SocketAddr;

    fn device(identity: &Identity, name: &str, version: u32) -> DeviceInfo {
        DeviceInfo { id: identity.device_id(), name: name.into(), platform: Platform::Linux, protocol_version: version }
    }

    /// A dialer and an accepter connected over loopback, as the engine
    /// would connect them.
    async fn connected_pair(dialer: &TlsIdentity, accepter: &TlsIdentity) -> (Connection, Connection) {
        let listener = Listener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap(), accepter).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (dialed, accepted) = tokio::join!(connect(addr, dialer), listener.accept());
        (dialed.unwrap(), accepted.unwrap().0)
    }

    /// The accepting side reading the dialer's `DeviceAnnounce` *before*
    /// sending its own (what the accept loop does when it has to look at a
    /// connection's first message to tell a file data connection from a
    /// mesh reconnect) still completes the same handshake on both ends, and
    /// each side hears what the other thinks of it.
    #[tokio::test]
    async fn identifying_after_reading_the_first_message_completes_the_handshake() {
        let dialer_identity = Identity::generate();
        let accepter_identity = Identity::generate();
        let dialer_device = device(&dialer_identity, "dialer", PROTOCOL_VERSION);
        let accepter_device = device(&accepter_identity, "accepter", PROTOCOL_VERSION);
        let dialer_tls = generate_self_signed(&dialer_identity).unwrap();
        let accepter_tls = generate_self_signed(&accepter_identity).unwrap();

        let listener = Listener::bind("127.0.0.1:0".parse().unwrap(), &accepter_tls).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dialer = tokio::spawn(async move {
            let mut conn = connect(addr, &dialer_tls).await.unwrap();
            announce_and_identify(&mut conn, &dialer_device, true).await.unwrap()
        });

        let (mut conn, _) = listener.accept().await.unwrap();
        let first = read_message(&mut conn).await.unwrap();
        let seen_by_accepter = announce_and_identify_after(&mut conn, &accepter_device, false, first).await.unwrap();
        assert_eq!(seen_by_accepter.peer.id, dialer_identity.device_id());
        assert_eq!(seen_by_accepter.trusts_me, Some(true));
        let seen_by_dialer = dialer.await.unwrap();
        assert_eq!(seen_by_dialer.peer.id, accepter_identity.device_id());
        assert_eq!(seen_by_dialer.trusts_me, Some(false));
    }

    /// Both sides run the full v4 pairing and show the same code; pairing
    /// the same two devices again shows a different one.
    #[tokio::test]
    async fn committed_pairing_gives_both_sides_the_same_fresh_code() {
        let a_identity = Identity::generate();
        let b_identity = Identity::generate();
        let a_device = device(&a_identity, "A", PROTOCOL_VERSION);
        let b_device = device(&b_identity, "B", PROTOCOL_VERSION);
        let a_tls = generate_self_signed(&a_identity).unwrap();
        let b_tls = generate_self_signed(&b_identity).unwrap();

        for _ in 0..2 {
            let (mut a_conn, mut b_conn) = connected_pair(&a_tls, &b_tls).await;
            let (a_seen, b_seen) = tokio::join!(
                announce_and_identify(&mut a_conn, &a_device, false),
                announce_and_identify(&mut b_conn, &b_device, false)
            );
            let (a_pending, b_pending) = tokio::join!(
                start_pairing(a_conn, &a_device, a_seen.unwrap().peer),
                start_pairing(b_conn, &b_device, b_seen.unwrap().peer)
            );
            let (a_pending, b_pending) = (a_pending.unwrap(), b_pending.unwrap());
            assert_eq!(a_pending.code, b_pending.code);
            assert_eq!(a_pending.code.len(), 6);
            assert!(a_pending.code.chars().all(|c| c.is_ascii_digit()));
            assert_ne!(a_pending.code, confirmation_code(a_device.id.as_bytes(), b_device.id.as_bytes()), "not the old key-only code");

            // And confirming on both sides pairs them.
            let (a_done, b_done) = tokio::join!(a_pending.confirm(true), b_pending.confirm(true));
            assert!(a_done.unwrap().is_some());
            assert!(b_done.unwrap().is_some());
        }
    }

    /// Two runs between the same devices show different codes — the nonces
    /// are fresh, so a code seen once is never good again.
    #[tokio::test]
    async fn committed_pairing_codes_differ_between_attempts() {
        let a_identity = Identity::generate();
        let b_identity = Identity::generate();
        let a_device = device(&a_identity, "A", PROTOCOL_VERSION);
        let b_device = device(&b_identity, "B", PROTOCOL_VERSION);
        let a_tls = generate_self_signed(&a_identity).unwrap();
        let b_tls = generate_self_signed(&b_identity).unwrap();

        let mut codes = Vec::new();
        for _ in 0..2 {
            let (mut a_conn, mut b_conn) = connected_pair(&a_tls, &b_tls).await;
            let (a_seen, b_seen) = tokio::join!(
                announce_and_identify(&mut a_conn, &a_device, false),
                announce_and_identify(&mut b_conn, &b_device, false)
            );
            let (a_pending, b_pending) = tokio::join!(
                start_pairing(a_conn, &a_device, a_seen.unwrap().peer),
                start_pairing(b_conn, &b_device, b_seen.unwrap().peer)
            );
            let (a_pending, b_pending) = (a_pending.unwrap(), b_pending.unwrap());
            assert_eq!(a_pending.code, b_pending.code);
            codes.push(a_pending.code);
        }
        assert_ne!(codes[0], codes[1]);
    }

    /// With a peer older than v4 the old code is used, unchanged, so the
    /// two still agree.
    #[tokio::test]
    async fn an_older_peer_gets_the_key_fingerprint_code() {
        let a_identity = Identity::generate();
        let b_identity = Identity::generate();
        let a_device = device(&a_identity, "A", PROTOCOL_VERSION);
        let old_b_device = device(&b_identity, "B", 3);
        let a_tls = generate_self_signed(&a_identity).unwrap();
        let b_tls = generate_self_signed(&b_identity).unwrap();

        let (mut a_conn, mut b_conn) = connected_pair(&a_tls, &b_tls).await;
        let (a_seen, b_seen) =
            tokio::join!(announce_and_identify(&mut a_conn, &a_device, false), announce_and_identify(&mut b_conn, &old_b_device, false));
        let (a_pending, b_pending) =
            tokio::join!(start_pairing(a_conn, &a_device, a_seen.unwrap().peer), start_pairing(b_conn, &old_b_device, b_seen.unwrap().peer));
        let (a_pending, b_pending) = (a_pending.unwrap(), b_pending.unwrap());
        assert_eq!(a_pending.code, b_pending.code);
        assert_eq!(a_pending.code, confirmation_code(a_device.id.as_bytes(), old_b_device.id.as_bytes()));
    }

    /// A peer that reveals a nonce other than the one it committed to is
    /// caught — that's the whole point of committing first.
    #[tokio::test]
    async fn a_reveal_that_doesnt_match_its_commitment_fails_the_pairing() {
        let honest_identity = Identity::generate();
        let cheat_identity = Identity::generate();
        let honest_device = device(&honest_identity, "honest", PROTOCOL_VERSION);
        let cheat_device = device(&cheat_identity, "cheat", PROTOCOL_VERSION);
        let honest_tls = generate_self_signed(&honest_identity).unwrap();
        let cheat_tls = generate_self_signed(&cheat_identity).unwrap();

        let (mut honest_conn, mut cheat_conn) = connected_pair(&honest_tls, &cheat_tls).await;
        let (honest_seen, _) = tokio::join!(
            announce_and_identify(&mut honest_conn, &honest_device, false),
            announce_and_identify(&mut cheat_conn, &cheat_device, false)
        );
        let cheat = async move {
            // Commits to one nonce, reveals another.
            write_message(&mut cheat_conn, &Message::PairCommit { commitment: pairing_commitment(&cheat_device.id, &[1u8; 32]) })
                .await
                .unwrap();
            let _their_commitment = read_message(&mut cheat_conn).await.unwrap();
            write_message(&mut cheat_conn, &Message::PairReveal { nonce: vec![2u8; 32] }).await.unwrap();
            let _ = read_message(&mut cheat_conn).await;
        };
        let (honest, _) = tokio::join!(start_pairing(honest_conn, &honest_device, honest_seen.unwrap().peer), cheat);
        match honest {
            Err(PairingError::CommitmentMismatch) => {}
            Err(other) => panic!("expected a commitment mismatch, got {other}"),
            Ok(_) => panic!("a cheating peer must not get a code"),
        }
    }

    /// Before anyone is trusted, a message announcing itself as larger than
    /// the handshake bound is refused rather than allocated for.
    #[tokio::test]
    async fn an_oversized_announce_is_refused() {
        let dialer_identity = Identity::generate();
        let accepter_identity = Identity::generate();
        let accepter_device = device(&accepter_identity, "accepter", PROTOCOL_VERSION);
        let dialer_tls = generate_self_signed(&dialer_identity).unwrap();
        let accepter_tls = generate_self_signed(&accepter_identity).unwrap();

        let (mut dialer_conn, mut accepter_conn) = connected_pair(&dialer_tls, &accepter_tls).await;
        let hostile = async move {
            use tokio::io::AsyncWriteExt;
            dialer_conn.write_all(&(HANDSHAKE_MAX_MESSAGE_BYTES + 1).to_be_bytes()).await.unwrap();
            // Keep the connection open: a reader that waited for the whole
            // message would wait here forever.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        };
        let identify = announce_and_identify(&mut accepter_conn, &accepter_device, false);
        tokio::select! {
            result = identify => assert!(matches!(result.unwrap_err(), PairingError::Framing(FramingError::TooLarge(_)))),
            _ = hostile => panic!("the oversized announce should have been refused at once"),
        }
    }
}
