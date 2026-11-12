//! Temporary rooms: codes that carry their end time, signed with the owner's secret
//! (STREAMER_TEMP_SECRET), so the server keeps nothing and a restart changes nothing.
//! See `protocol::temp` for the layout.

use hmac::{Hmac, Mac};
use protocol::temp::{self, Parts};
use sha2::Sha256;

/// The shortest secret accepted, in bytes.
pub const MIN_SECRET: usize = 16;

#[derive(Clone)]
pub struct TempKey(Vec<u8>);

impl TempKey {
    /// None for a secret too short to be one.
    pub fn new(secret: &[u8]) -> Option<Self> {
        (secret.len() >= MIN_SECRET).then(|| Self(secret.to_vec()))
    }

    /// From STREAMER_TEMP_SECRET; None when it is not set (temporary rooms are off).
    pub fn from_env() -> Result<Option<Self>, String> {
        match std::env::var("STREAMER_TEMP_SECRET") {
            Ok(s) if s.trim().is_empty() => Ok(None),
            Ok(s) => Self::new(s.trim().as_bytes()).map(Some).ok_or_else(|| format!("STREAMER_TEMP_SECRET is shorter than {MIN_SECRET} characters")),
            Err(_) => Ok(None),
        }
    }

    fn mac(&self, nonce: u32, ends: u64) -> u64 {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.0).expect("any key length");
        m.update(&temp::signed_bytes(nonce, ends));
        let out = m.finalize().into_bytes();
        u64::from_be_bytes(out[..8].try_into().unwrap()) >> (64 - temp::MAC_BITS)
    }

    /// A new room's code that ends at `ends` (unix seconds).
    pub fn make(&self, ends: u64) -> String {
        let nonce = random_nonce();
        temp::encode(Parts { nonce, ends, mac: self.mac(nonce, ends) })
    }

    /// The room's end time (unix seconds) if `room` is a code signed with this key.
    pub fn check(&self, room: &str) -> Option<u64> {
        let p = temp::decode(room)?;
        // A forged code is refused the same way however close it came.
        let diff = self.mac(p.nonce, p.ends) ^ p.mac;
        (diff == 0).then_some(p.ends)
    }
}

/// 20 bits from the OS's randomness (a keyed hash seeded by it).
fn random_nonce() -> u32 {
    use std::hash::BuildHasher;
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    (std::collections::hash_map::RandomState::new().hash_one(t) & ((1 << temp::NONCE_BITS) - 1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_code_checks_and_a_changed_one_does_not() {
        let k = TempKey::new(b"0123456789abcdef-test").unwrap();
        let c = k.make(2_000_000_000);
        assert_eq!(c.len(), 20);
        assert_eq!(k.check(&c), Some(2_000_000_000));
        // A later end time with the old signature.
        let p = temp::decode(&c).unwrap();
        let later = temp::encode(Parts { ends: p.ends + 3600, ..p });
        assert_eq!(k.check(&later), None);
        // Another key.
        assert_eq!(TempKey::new(b"another-secret-0123").unwrap().check(&c), None);
        assert!(TempKey::new(b"short").is_none());
    }
}
