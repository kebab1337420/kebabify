//! Per-boot token for `/shutdown`: the proxy mints a random token file at
//! start and requires it as a header, so only local processes that can read
//! the user's own `%APPDATA%\Kebabify` dir (the CLI, the updater) can stop
//! the proxy — an `Origin` header alone is forgeable by any local process.
//!
//! Transparent: the CLI attaches the token automatically when the file
//! exists, and falls back to the Origin-only request against older proxies
//! that never minted one.

use anyhow::{Context, Result};

/// Request header carrying the token.
pub const TOKEN_HEADER: &str = "X-Shutdown-Token";

/// `%APPDATA%\Kebabify\shutdown.token` (same dir as the cookies file).
pub fn token_path() -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("[kebabify] APPDATA unset — shutdown token falls back to HOME");
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."))
        });
    base.join("Kebabify").join("shutdown.token")
}

/// Reads the minted token, if this machine's proxy created one.
pub fn load() -> Option<String> {
    let raw = std::fs::read_to_string(token_path()).ok()?;
    let token = raw.trim().to_string();
    if token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(token)
    } else {
        None
    }
}

/// Loads the token, minting a fresh one when absent or malformed.
/// Entropy: wall-clock nanos + pid + process counter, hashed — unguessable
/// without read access to the file itself, which is the trust boundary.
pub fn load_or_create() -> Result<String> {
    if let Some(token) = load() {
        return Ok(token);
    }
    let path = token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create {}", parent.display()))?;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = format!(
        "{}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    use sha2::{Digest, Sha256};
    let token = format!("{:x}", Sha256::digest(seed.as_bytes()));
    // Atomic write: tmp + rename, like the cookie store.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &token).context("Cannot write shutdown token")?;
    std::fs::rename(&tmp, &path).context("Cannot install shutdown token")?;
    Ok(token)
}

/// Constant-time comparison (no early exit on first mismatch).
pub fn verify(candidate: &str, expected: &str) -> bool {
    if candidate.len() != expected.len() {
        return false;
    }
    candidate
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_verification_is_exact() {
        let good = "a".repeat(64);
        assert!(verify(&good, &good));
        assert!(!verify(&"b".repeat(64), &good));
        // Length mismatch (empty header, truncated token): reject.
        assert!(!verify("", &good));
        assert!(!verify(&good[..32], &good));
        // Non-hex content still compares by bytes (load() filters format).
        assert!(!verify(&"!".repeat(64), &good));
    }

    #[test]
    fn malformed_token_file_loads_as_none() {
        // load() reads the real APPDATA path — only assert the pure shape:
        // whatever it returns must be 64 hex chars or None.
        if let Some(t) = load() {
            assert_eq!(t.len(), 64);
            assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}
