//! Single place where SHA-256 is computed and rendered as hex.
//!
//! The `sha2` 0.11 digest output no longer implements `LowerHex`, and the
//! hasher is no longer an `io::Write`, so every call site would otherwise
//! grow its own hand-rolled hex encoder. One encoder, one known-answer test.

use sha2::{Digest, Sha256};

/// Lowercase hex of arbitrary bytes.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Hex SHA-256 of an in-memory buffer.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(Sha256::digest(data).as_slice())
}

/// Hex SHA-256 of a file, read in bounded chunks. A release asset or a
/// 114 MB downloader must not be slurped into memory to be hashed.
pub fn sha256_file_hex(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(hasher.finalize().as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST known answers. If the hex encoder or the chunked file reader were
    /// wrong, every checksum verification in the updater and the installer
    /// would silently reject (or worse, accept) the wrong bytes.
    #[test]
    fn known_answer_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex(&[]), "");
        let digest = sha256_hex(b"abc");
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    /// The chunked reader must agree with the one-shot digest, including
    /// across a chunk boundary and for an empty file.
    #[test]
    fn file_hash_matches_buffer_hash() {
        let dir = std::env::temp_dir().join(format!("kebabify_digest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.bin");
        let payload: Vec<u8> = (0..200_000u32).map(|value| (value % 251) as u8).collect();
        std::fs::write(&path, &payload).unwrap();
        assert_eq!(
            sha256_file_hex(&path).unwrap(),
            sha256_hex(&payload),
            "chunked file hashing must equal one-shot hashing"
        );
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256_file_hex(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
