use crate::hash::content_hash;

/// Derives the human-verifiable confirmation code shown on both devices
/// during pairing. Order-independent (sorts the two keys first) so it
/// doesn't matter which side is "initiator" — both screens must show the
/// same 6-digit code for the user to confirm.
///
/// This is a fingerprint-comparison scheme (like SSH host key verification,
/// shown on both ends simultaneously), not a full Diffie-Hellman SAS
/// exchange. It's sufficient to catch a MITM as long as the user actually
/// compares the codes — a stronger ECDH-based short-authentication-string
/// scheme (as used in Signal/Bluetooth SSP) is a reasonable v2 hardening if
/// this project grows beyond personal use.
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
}
