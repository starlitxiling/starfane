//! Identifier newtypes and ULID-style id generation.
//!
//! Format: `<prefix>-<26 Crockford base32 chars>` encoding 48-bit Unix
//! milliseconds followed by 80 random bits (read from `/dev/urandom`).

use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

macro_rules! id_newtype {
    ($($(#[$m:meta])* $n:ident),* $(,)?) => { $(
        $(#[$m])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $n(pub String);

        impl $n {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
            pub fn is_empty(&self) -> bool { self.0.is_empty() }
        }
        impl fmt::Display for $n {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl From<String> for $n { fn from(s: String) -> Self { Self(s) } }
        impl From<&str> for $n { fn from(s: &str) -> Self { Self(s.to_string()) } }
        impl AsRef<str> for $n { fn as_ref(&self) -> &str { &self.0 } }
    )* };
}

id_newtype!(
    /// `E-…`
    EventId,
    /// Root: agent session id / agy conversationId. Sub: `<root>/<agent_id>`.
    SessionId,
    /// tmux pane id, e.g. `%12`.
    PaneId,
    /// `P-…`
    PipelineId,
    /// `H-…`
    HandoffId,
    /// `A-…`
    ArtifactId,
    /// `R-…`
    ReviewId,
    /// `F-…`
    FindingId,
    /// `B-…` one-shot L4 spawn binding token (`$STARFANE_BIND`).
    BindToken,
);

/// Id prefixes (see contracts §0).
pub mod prefix {
    pub const EVENT: &str = "E";
    pub const PIPELINE: &str = "P";
    pub const HANDOFF: &str = "H";
    pub const ARTIFACT: &str = "A";
    pub const REVIEW: &str = "R";
    pub const FINDING: &str = "F";
    pub const BIND: &str = "B";
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const ULID_LEN: usize = 26;
const MS_MASK: u128 = (1u128 << 48) - 1;
const RAND_MASK: u128 = (1u128 << 80) - 1;

/// Current Unix time in milliseconds (0 if the clock is before the epoch).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate a new id: `<prefix>-<ULID>`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", ulid_string(now_ms(), random_80()))
}

/// Encode 48-bit `ms` + 80-bit `rand80` as 26 Crockford base32 chars.
pub fn ulid_string(ms: u64, rand80: u128) -> String {
    let value: u128 = ((ms as u128 & MS_MASK) << 80) | (rand80 & RAND_MASK);
    let mut out = [0u8; ULID_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (ULID_LEN - 1 - i);
        let idx = ((value >> shift) & 0x1f) as usize;
        *slot = CROCKFORD[idx];
    }
    // All bytes come from the ASCII alphabet above.
    String::from_utf8(out.to_vec()).unwrap_or_default()
}

/// Decode the millisecond timestamp from an id produced by [`new_id`].
pub fn timestamp_ms(id: &str) -> Option<u64> {
    let ulid = id.rsplit('-').next()?;
    if ulid.len() != ULID_LEN {
        return None;
    }
    let mut value: u128 = 0;
    for b in ulid.bytes() {
        let idx = CROCKFORD
            .iter()
            .position(|c| *c == b.to_ascii_uppercase())?;
        value = (value << 5) | idx as u128;
    }
    Some(((value >> 80) & MS_MASK) as u64)
}

fn random_80() -> u128 {
    let mut buf = [0u8; 10];
    let ok = File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        return fallback_random_80();
    }
    buf.iter().fold(0u128, |acc, b| (acc << 8) | *b as u128)
}

/// Non-cryptographic fallback when `/dev/urandom` is unavailable.
fn fallback_random_80() -> u128 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut parts = [0u64; 2];
    for (i, part) in parts.iter_mut().enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u64(n);
        h.write_u32(std::process::id());
        h.write_usize(i);
        *part = h.finish();
    }
    ((parts[0] as u128) << 64 | parts[1] as u128) & RAND_MASK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn format_prefix_and_length() {
        let id = new_id(prefix::PIPELINE);
        assert!(id.starts_with("P-"));
        assert_eq!(id.len(), 2 + ULID_LEN);
        assert!(id[2..].bytes().all(|b| CROCKFORD.contains(&b)));
    }

    #[test]
    fn timestamp_roundtrip() {
        let ms = 1_763_600_000_123u64;
        let s = format!("E-{}", ulid_string(ms, 0xABCDEF));
        assert_eq!(timestamp_ms(&s), Some(ms));
        let live = new_id("E");
        let ts = timestamp_ms(&live).unwrap_or(0);
        assert!(now_ms().abs_diff(ts) < 5_000);
    }

    #[test]
    fn known_vector() {
        assert_eq!(ulid_string(0, 0), "0".repeat(26));
        assert_eq!(ulid_string(0, 31), format!("{}Z", "0".repeat(25)));
    }

    #[test]
    fn unique_and_sortable() {
        let ids: Vec<String> = (0..2000).map(|_| new_id("F")).collect();
        let set: HashSet<_> = ids.iter().collect();
        assert_eq!(set.len(), ids.len());
        let a = format!("A-{}", ulid_string(1000, u128::MAX));
        let b = format!("A-{}", ulid_string(1001, 0));
        assert!(a < b);
    }

    #[test]
    fn fallback_random_is_masked() {
        assert!(fallback_random_80() <= RAND_MASK);
    }

    #[test]
    fn newtype_serde_is_transparent() {
        let s = SessionId::new("abc");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"abc\"");
        let p: PaneId = serde_json::from_str("\"%3\"").unwrap();
        assert_eq!(p.as_str(), "%3");
    }
}
