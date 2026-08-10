use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of `data`, used for clipboard-echo suppression and
/// file transfer integrity checks. Not a security primitive — just a
/// cheap, stable content fingerprint.
pub fn content_hash(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

/// Same algorithm as `content_hash`, fed incrementally — for hashing a
/// file as its chunks arrive over the wire instead of buffering the whole
/// thing in memory first.
#[derive(Default)]
pub struct IncrementalHash(Sha256);

impl IncrementalHash {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finalize_hex(self) -> String {
        hex::encode(self.0.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_hashes_equal() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
    }

    #[test]
    fn different_content_hashes_differ() {
        assert_ne!(content_hash(b"hello"), content_hash(b"world"));
    }
}
