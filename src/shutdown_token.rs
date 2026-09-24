//! Per-boot token for `/shutdown`: the proxy mints a random token file at
//! start and requires it as a header, so only local processes that can read
//! the user's own `%APPDATA%\Kebabify` dir (the CLI, the updater) can stop
//! the proxy — an `Origin` header alone is forgeable by any local process.
//!
//! Transparent: the CLI attaches the token automatically when the file
//! exists, and falls back to the Origin-only request against older proxies
//! that never minted one.

use anyhow::{Context, Result, anyhow};

/// Request header carrying the token.
pub const TOKEN_HEADER: &str = "X-Shutdown-Token";

fn appdata_base() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("[kebabify] APPDATA unset — shutdown token falls back to HOME");
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."))
        })
}

fn token_path_in(base: &std::path::Path) -> std::path::PathBuf {
    base.join("Kebabify").join("shutdown.token")
}

/// `%APPDATA%\Kebabify\shutdown.token` (same dir as the cookies file).
pub fn token_path() -> std::path::PathBuf {
    token_path_in(&appdata_base())
}

fn load_path(path: &std::path::Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let token = raw.trim().to_string();
    if token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(token)
    } else {
        None
    }
}

fn load_in(base: &std::path::Path) -> Option<String> {
    load_path(&token_path_in(base))
}

/// Reads the minted token, if this machine's proxy created one.
pub fn load() -> Option<String> {
    load_path(&token_path())
}

fn temp_token_path(path: &std::path::Path, attempt: u64) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("shutdown.token");
    path.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), attempt))
}

fn mint_token(counter: u64) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let seed = format!(
        "{}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
        counter
    );
    let mut first = RandomState::new().build_hasher();
    first.write(seed.as_bytes());
    let mut second = RandomState::new().build_hasher();
    second.write(seed.as_bytes());
    let material = format!("{}:{}:{}", first.finish(), second.finish(), seed);
    crate::digest::sha256_hex(material.as_bytes())
}

fn install_token(path: &std::path::Path, token: &str) -> Result<String> {
    use std::io::Write;

    for attempt in 0..100 {
        let tmp = temp_token_path(path, attempt);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot create {}", tmp.display()));
            }
        };
        if let Err(error) = file
            .write_all(token.as_bytes())
            .and_then(|()| file.sync_all())
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).context("Cannot write shutdown token");
        }
        drop(file);

        if let Some(existing) = load_path(path) {
            let _ = std::fs::remove_file(&tmp);
            return Ok(existing);
        }
        if let Ok(metadata) = std::fs::symlink_metadata(path)
            && metadata.file_type().is_symlink()
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow!("Refusing to replace symlink {}", path.display()));
        }
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(token.to_string()),
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
                if let Some(existing) = load_path(path) {
                    return Ok(existing);
                }
                return Err(anyhow!(
                    "Cannot install shutdown token at {}",
                    path.display()
                ));
            }
        }
    }
    Err(anyhow!(
        "Cannot create a unique shutdown token temporary file"
    ))
}

fn load_or_create_in(base: &std::path::Path) -> Result<String> {
    if let Some(token) = load_in(base) {
        return Ok(token);
    }
    let path = token_path_in(base);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create {}", parent.display()))?;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let token = mint_token(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    install_token(&path, &token)
}

/// Loads the token, minting a fresh one when absent or malformed.
pub fn load_or_create() -> Result<String> {
    load_or_create_in(&appdata_base())
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

    struct TestDir {
        path: std::path::PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kebabify-shutdown-token-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn base(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn assert_token_shape(token: &str) {
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn first_creation_writes_a_valid_token() {
        let dir = TestDir::new();
        assert!(load_in(dir.base()).is_none());

        let token = load_or_create_in(dir.base()).unwrap();
        assert_token_shape(&token);
        assert_eq!(load_in(dir.base()), Some(token));
    }

    #[test]
    fn existing_valid_token_is_reused() {
        let dir = TestDir::new();
        let first = load_or_create_in(dir.base()).unwrap();
        let second = load_or_create_in(dir.base()).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(token_path_in(dir.base())).unwrap(),
            second
        );
    }

    #[test]
    fn parent_directory_is_created() {
        let dir = TestDir::new();
        let path = token_path_in(dir.base());
        assert!(!path.parent().unwrap().exists());

        let token = load_or_create_in(dir.base()).unwrap();
        assert_token_shape(&token);
        assert!(path.is_file());
    }

    #[test]
    fn malformed_token_file_loads_as_none_and_is_replaced() {
        let dir = TestDir::new();
        let path = token_path_in(dir.base());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-a-token").unwrap();

        assert!(load_in(dir.base()).is_none());
        let token = load_or_create_in(dir.base()).unwrap();
        assert_token_shape(&token);
        assert_eq!(load_in(dir.base()), Some(token.clone()));
        assert!(verify(&token, &load_in(dir.base()).unwrap()));
    }

    #[test]
    fn unreadable_token_file_loads_as_none_and_is_replaced() {
        let dir = TestDir::new();
        let path = token_path_in(dir.base());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0xff, 0xfe]).unwrap();

        assert!(load_in(dir.base()).is_none());
        let token = load_or_create_in(dir.base()).unwrap();
        assert_token_shape(&token);
        assert_eq!(load_in(dir.base()), Some(token));
    }
}
