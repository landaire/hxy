//! Cryptographically-secure opaque tokens used by the plugin host as
//! identity tags for things plugins hand back -- mount instances,
//! pending-prompt handles, etc.
//!
//! Tokens are generated from the OS CSPRNG (`getrandom`), encoded as
//! URL-safe base64 without padding so they survive round-tripping
//! through TOML / JSON / file paths. 16 bytes (128 bits) of entropy
//! is plenty for a non-persistent, non-guessable handle: collisions
//! at 2^64 generations and brute-forcing is meaningless because
//! tokens have no privileged value on their own (the host validates
//! every use against an in-memory map keyed by token).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use thiserror::Error;

/// Number of random bytes per token. 16 = 128 bits, which yields a
/// 22-character base64 string. Tracked as a constant so consumers
/// that want to size buffers (or write tests asserting the shape)
/// have one place to look.
pub const TOKEN_BYTES: usize = 16;

#[derive(Debug, Error)]
pub enum TokenError {
    /// Carrying the underlying [`getrandom::Error`] code rather than
    /// the type itself: in `getrandom = "0.3"` the error doesn't
    /// implement `std::error::Error` so it can't be a `#[source]`.
    /// The numeric code round-trips to `getrandom::Error::from(code)`
    /// for callers that want to inspect it.
    #[error("read system entropy: getrandom code {0}")]
    Entropy(u32),
}

/// Mint a fresh opaque token. Each call yields a distinct value with
/// overwhelming probability; uniqueness is *not* guaranteed by the
/// type (callers that need a uniqueness invariant should keep their
/// own dedup map). Suitable for short-lived in-memory handles and
/// for tokens persisted alongside other plugin state.
pub fn fresh() -> Result<String, TokenError> {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut buf).map_err(|e| TokenError::Entropy(e.raw_os_error().unwrap_or(0) as u32))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn token_shape_matches_byte_count() {
        let t = fresh().expect("entropy");
        // base64-no-pad of 16 bytes = ceil(16 / 3 * 4) = 22 chars.
        assert_eq!(t.len(), 22);
        assert!(
            t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "URL-safe base64 alphabet only, got {t}"
        );
    }

    #[test]
    fn many_tokens_do_not_collide() {
        // 1024 draws from 2^128 -- birthday probability is ~10^-32.
        // Any collision here means the RNG is broken, not an off-by-one.
        let mut seen = HashSet::new();
        for _ in 0..1024 {
            let t = fresh().expect("entropy");
            assert!(seen.insert(t), "duplicate token");
        }
    }
}
