use crate::hash::content_hash;
use sha2::{Digest, Sha256};

/// Derives the human-verifiable confirmation code shown on both devices
/// during pairing with a peer older than protocol v4. Order-independent
/// (sorts the two keys first) so it doesn't matter which side is
/// "initiator" — both screens must show the same 6-digit code for the user
/// to confirm.
///
/// **Weak, kept only for older peers.** This is a fingerprint comparison
/// of the two long-term keys and nothing else, and both keys are public
/// (every device announces its id over mDNS). An attacker in the middle
/// presents a key of its own to each side and can grind that key, offline
/// and ahead of time, until the code the victim sees matches the code the
/// other victim sees — about a million key generations, seconds of work.
/// `confirmation_code_with_nonces` below is what v4 peers use instead.
pub fn confirmation_code(local_pubkey: &[u8], remote_pubkey: &[u8]) -> String {
    let (a, b) = if local_pubkey <= remote_pubkey {
        (local_pubkey, remote_pubkey)
    } else {
        (remote_pubkey, local_pubkey)
    };

    let mut material = Vec::with_capacity(a.len() + b.len());
    material.extend_from_slice(a);
    material.extend_from_slice(b);

    let digest_hex = content_hash(&material);
    // Take the first 6 hex digits (24 bits) and render as a 6-digit code —
    // enough entropy to make guessing impractical within a pairing window,
    // easy for a human to read aloud and compare.
    let numeric = u32::from_str_radix(&digest_hex[..6], 16).expect("hex slice is valid");
    format!("{:06}", numeric % 1_000_000)
}

/// Size of the random nonce each side contributes to a v4 pairing.
pub const PAIRING_NONCE_BYTES: usize = 32;

/// What a device sends first in a v4 pairing: a commitment to its nonce,
/// `SHA-256(tag || device id || nonce)`. The id is in it so one side's
/// commitment can't simply be echoed back by the other.
pub fn pairing_commitment(device_id: &str, nonce: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"continuity-pairing-commit\0");
    hasher.update(device_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(nonce);
    hasher.finalize().to_vec()
}

/// The v4 confirmation code: 6 digits from `SHA-256(tag || id_1 || nonce_1
/// || id_2 || nonce_2)`, the two (id, nonce) pairs ordered by id so both
/// sides compute the same thing whoever dialed.
///
/// Why this beats `confirmation_code`: each side commits to its nonce
/// (`pairing_commitment`) before it learns the other's, and only then
/// reveals it. An attacker in the middle therefore has to fix its nonce
/// towards each victim before it knows the code the other victim will see,
/// so making the two codes agree is one blind guess in a million per
/// attempt — not something to grind for. Fresh nonces also mean the code
/// differs every time the same two devices pair, so a code seen once is
/// never good again.
pub fn confirmation_code_with_nonces(local_id: &str, local_nonce: &[u8], remote_id: &str, remote_nonce: &[u8]) -> String {
    let (first, second) = if local_id <= remote_id {
        ((local_id, local_nonce), (remote_id, remote_nonce))
    } else {
        ((remote_id, remote_nonce), (local_id, local_nonce))
    };
    let mut hasher = Sha256::new();
    hasher.update(b"continuity-pairing-code\0");
    for (id, nonce) in [first, second] {
        hasher.update(id.as_bytes());
        hasher.update(b"\0");
        hasher.update(nonce);
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    let numeric = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", numeric % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_order_independent() {
        let a = b"device-a-pubkey";
        let b = b"device-b-pubkey";
        assert_eq!(confirmation_code(a, b), confirmation_code(b, a));
    }

    #[test]
    fn code_is_six_digits() {
        let code = confirmation_code(b"a", b"b");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn different_pairs_usually_differ() {
        let code1 = confirmation_code(b"device-a", b"device-b");
        let code2 = confirmation_code(b"device-a", b"device-c");
        assert_ne!(code1, code2);
    }

    #[test]
    fn nonce_code_is_order_independent_six_digits_and_fresh_per_nonce() {
        let (a, b) = ("aaaa", "bbbb");
        let (na, nb) = ([1u8; 32], [2u8; 32]);
        let code = confirmation_code_with_nonces(a, &na, b, &nb);
        assert_eq!(code, confirmation_code_with_nonces(b, &nb, a, &na), "whoever dialed, the same code");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        // The pairs stay attached to their ids: swapping the nonces is a different pairing.
        assert_ne!(code, confirmation_code_with_nonces(a, &nb, b, &na));
        // A fresh nonce means a fresh code (with overwhelming probability).
        assert_ne!(code, confirmation_code_with_nonces(a, &[3u8; 32], b, &nb));
    }

    #[test]
    fn commitments_are_deterministic_and_bound_to_the_device() {
        let nonce = [5u8; 32];
        assert_eq!(pairing_commitment("abc", &nonce), pairing_commitment("abc", &nonce));
        assert_eq!(pairing_commitment("abc", &nonce).len(), 32);
        assert_ne!(pairing_commitment("abc", &nonce), pairing_commitment("abd", &nonce), "an echoed commitment doesn't check out under another id");
        assert_ne!(pairing_commitment("abc", &nonce), pairing_commitment("abc", &[6u8; 32]));
    }
}
