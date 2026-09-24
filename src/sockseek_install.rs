//! Download and install the pinned upstream `sockseek` release.
//!
//! The executable is not bundled with kebabify. It is fetched from the
//! upstream release, checked against pinned size and SHA-256 values, and only
//! then published to the per-user Soulseek binary directory.

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

/// Version of the upstream `sockseek` release installed by this module.
pub const SOCKSEEK_VERSION: &str = "3.0.5";

/// Zip asset name for the upstream Windows release.
pub const SOCKSEEK_ASSET_NAME: &str = "sockseek_3.0.5_win-x64.zip";

/// Pinned upstream release download URL.
pub const SOCKSEEK_ZIP_URL: &str =
    "https://github.com/fiso64/sockseek/releases/download/v3.0.5/sockseek_3.0.5_win-x64.zip";

/// Upstream release page shown when installation is required.
pub const SOCKSEEK_RELEASE_PAGE: &str = "https://github.com/fiso64/sockseek/releases";

/// Pinned SHA-256 of the release zip.
pub const SOCKSEEK_ZIP_SHA256: &str =
    "1b5c1189dcfc24cc9fea22dc67a58b7a5f0a127d0367355cf2a8718100044802";

/// Pinned SHA-256 of the extracted Windows executable.
pub const SOCKSEEK_BINARY_SHA256: &str =
    "47b1d9abb78df23b66da807aba7610a2f3fbcc30d4d5fadfd550baee2f41a498";

/// Expected size of the release zip in bytes.
pub const SOCKSEEK_ZIP_SIZE: u64 = 50_404_459;

/// Expected size of the extracted executable in bytes.
pub const SOCKSEEK_BINARY_SIZE: u64 = 114_581_730;

/// Returns whether the configured downloader is an existing non-empty file.
pub fn is_installed() -> bool {
    crate::soulseek::binary_path().is_some_and(|path| path.parent().is_some_and(is_installed_in))
}

/// Downloads, verifies, and atomically installs the pinned downloader.
pub async fn ensure_installed(client: &reqwest::Client) -> Result<()> {
    if is_installed() {
        return Ok(());
    }

    let destination = crate::soulseek::binary_path()
        .ok_or_else(|| anyhow!("Soulseek: no application data directory for the downloader"))?;
    let destination_dir = destination
        .parent()
        .ok_or_else(|| anyhow!("Soulseek: downloader destination has no parent directory"))?;
    std::fs::create_dir_all(destination_dir)
        .map_err(|error| anyhow!("Cannot create {}: {}", destination_dir.display(), error))?;

    eprintln!(
        "[kebabify] Soulseek: installing sockseek {} (downloading about 50 MB)...",
        SOCKSEEK_VERSION
    );

    let (archive_guard, archive_file) = create_staged_file(destination_dir, "sockseek.zip")?;
    let archive_path = archive_guard.path().to_path_buf();
    download_zip(client, archive_file, &archive_path).await?;

    let (mut extracted_guard, extracted_file) =
        create_staged_file(destination_dir, "sockseek.exe")?;
    drop(extracted_file);
    let extracted_path = extracted_guard.path().to_path_buf();
    extract_sockseek(&archive_path, &extracted_path)?;
    verify_file_hash(
        &extracted_path,
        SOCKSEEK_BINARY_SHA256,
        SOCKSEEK_BINARY_SIZE,
    )?;

    std::fs::rename(&extracted_path, &destination).map_err(|error| {
        anyhow!(
            "Cannot publish {} to {}: {}",
            extracted_path.display(),
            destination.display(),
            error
        )
    })?;
    extracted_guard.disarm();
    drop(archive_guard);
    Ok(())
}

/// Returns a short instruction for status when the downloader is missing.
pub fn install_hint() -> String {
    let path = crate::soulseek::binary_path()
        .map(|value| value.display().to_string())
        .unwrap_or_else(|| "<no application data directory>".to_string());
    format!(
        "Soulseek binary missing — sockseek {} (AGPL-3.0) is downloaded automatically on first apply, or install {} from {} into {}.",
        SOCKSEEK_VERSION, SOCKSEEK_ASSET_NAME, SOCKSEEK_RELEASE_PAGE, path
    )
}

#[cfg(target_os = "windows")]
fn binary_name() -> &'static str {
    "sockseek.exe"
}

#[cfg(not(target_os = "windows"))]
fn binary_name() -> &'static str {
    "sockseek"
}

fn is_installed_in(destination_dir: &Path) -> bool {
    let path = destination_dir.join(binary_name());
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

fn byte_count_is_within_cap(observed: u64, cap: u64) -> bool {
    observed <= cap
}

fn check_declared_size(declared: u64) -> Result<()> {
    if declared != SOCKSEEK_ZIP_SIZE {
        return Err(anyhow!(
            "Download declares {} bytes; expected exactly {}",
            declared,
            SOCKSEEK_ZIP_SIZE
        ));
    }
    Ok(())
}

fn check_observed_size(observed: u64) -> Result<()> {
    if !byte_count_is_within_cap(observed, SOCKSEEK_ZIP_SIZE) {
        return Err(anyhow!(
            "Download exceeds the {} byte cap",
            SOCKSEEK_ZIP_SIZE
        ));
    }
    if observed != SOCKSEEK_ZIP_SIZE {
        return Err(anyhow!(
            "Download contains {} bytes; expected exactly {}",
            observed,
            SOCKSEEK_ZIP_SIZE
        ));
    }
    Ok(())
}

fn hash_matches(actual: &str, expected: &str) -> bool {
    actual.len() == 64
        && expected.len() == 64
        && actual.bytes().all(|byte| byte.is_ascii_hexdigit())
        && expected.bytes().all(|byte| byte.is_ascii_hexdigit())
        && actual.eq_ignore_ascii_case(expected)
}

struct StagedFile(Option<PathBuf>);

impl StagedFile {
    fn path(&self) -> &Path {
        self.0.as_deref().expect("staged path present")
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn create_staged_file(directory: &Path, stem: &str) -> Result<(StagedFile, std::fs::File)> {
    for attempt in 0..10u32 {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let name = format!(".{stem}.{}-{nonce}-{attempt}.part", std::process::id());
        let path = directory.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((StagedFile(Some(path)), file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(anyhow!(
                    "Cannot create staging file {}: {}",
                    path.display(),
                    error
                ));
            }
        }
    }
    Err(anyhow!("Cannot create a unique staging file"))
}

async fn download_zip(
    client: &reqwest::Client,
    mut file: std::fs::File,
    archive_path: &Path,
) -> Result<()> {
    let mut response = client
        .get(SOCKSEEK_ZIP_URL)
        .timeout(std::time::Duration::from_secs(120))
        .header("User-Agent", "kebabify")
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .map_err(|error| anyhow!("Soulseek downloader download failed: {}", error))?;
    response = response
        .error_for_status()
        .map_err(|error| anyhow!("Soulseek downloader download returned an error: {}", error))?;

    if let Some(declared) = response.content_length() {
        check_declared_size(declared)?;
    }

    use std::io::Write;
    let mut observed = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("Soulseek downloader download interrupted: {}", error))?
    {
        observed = observed.saturating_add(chunk.len() as u64);
        if !byte_count_is_within_cap(observed, SOCKSEEK_ZIP_SIZE) {
            return Err(anyhow!(
                "Soulseek downloader download exceeds the {} byte cap",
                SOCKSEEK_ZIP_SIZE
            ));
        }
        file.write_all(&chunk)
            .map_err(|error| anyhow!("Cannot write {}: {}", archive_path.display(), error))?;
    }
    file.flush()
        .map_err(|error| anyhow!("Cannot flush {}: {}", archive_path.display(), error))?;
    drop(file);

    check_observed_size(observed)?;
    verify_file_hash(archive_path, SOCKSEEK_ZIP_SHA256, SOCKSEEK_ZIP_SIZE)
}

fn verify_file_hash(path: &Path, expected: &str, expected_size: u64) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| anyhow!("Cannot inspect {}: {}", path.display(), error))?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(anyhow!("{} has an unexpected size", path.display()));
    }

    let actual = crate::digest::sha256_file_hex(path)
        .map_err(|error| anyhow!("Cannot hash {}: {}", path.display(), error))?;
    if !hash_matches(&actual, expected) {
        return Err(anyhow!("{} failed SHA-256 verification", path.display()));
    }
    Ok(())
}

fn powershell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[cfg(target_os = "windows")]
fn extract_sockseek(archive: &Path, output: &Path) -> Result<()> {
    let system_root = std::env::var_os("SystemRoot")
        .ok_or_else(|| anyhow!("PowerShell path is unavailable because SystemRoot is unset"))?;
    let powershell = PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        return Err(anyhow!(
            "PowerShell is unavailable at {}",
            powershell.display()
        ));
    }

    let script = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead('__ARCHIVE__')
try {
    $unexpected = @($archive.Entries | Where-Object {
        $_.FullName -cne 'sockseek.exe' -and $_.FullName -cne 'LICENSE'
    })
    if ($unexpected.Count -ne 0) {
        throw 'Archive contains an unexpected entry'
    }
    $entries = @($archive.Entries | Where-Object {
        $_.FullName -ceq 'sockseek.exe'
    })
    if ($entries.Count -ne 1) {
        throw 'Archive does not contain exactly one sockseek.exe'
    }
    [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], '__OUTPUT__', $true)
}
finally {
    $archive.Dispose()
}
"#;
    let command = script
        .replace("__ARCHIVE__", &powershell_quote(archive))
        .replace("__OUTPUT__", &powershell_quote(output));
    let result = std::process::Command::new(&powershell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(command)
        .output()
        .map_err(|error| anyhow!("Cannot run {}: {}", powershell.display(), error))?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr);
        return Err(anyhow!(
            "PowerShell could not extract sockseek.exe: {}",
            detail.trim()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn extract_sockseek(archive: &Path, output: &Path) -> Result<()> {
    let _ = (archive, output);
    Err(anyhow!(
        "sockseek installation is supported on Windows only"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kebabify-sockseek-install-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn is_installed_requires_a_non_empty_file() {
        let root = temp_dir("installed");
        let destination_dir = root.join("bin");
        assert!(!is_installed_in(&destination_dir));
        std::fs::create_dir_all(&destination_dir).unwrap();
        let path = destination_dir.join(binary_name());
        std::fs::write(&path, []).unwrap();
        assert!(!is_installed_in(&destination_dir));
        std::fs::write(&path, b"x").unwrap();
        assert!(is_installed_in(&destination_dir));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn byte_cap_rejects_oversized_body() {
        assert!(byte_count_is_within_cap(
            SOCKSEEK_ZIP_SIZE,
            SOCKSEEK_ZIP_SIZE
        ));
        assert!(!byte_count_is_within_cap(
            SOCKSEEK_ZIP_SIZE + 1,
            SOCKSEEK_ZIP_SIZE
        ));
    }

    #[test]
    fn hash_comparison_normalizes_case_and_validates_hex() {
        assert!(hash_matches(SOCKSEEK_ZIP_SHA256, SOCKSEEK_ZIP_SHA256));
        assert!(hash_matches(
            &SOCKSEEK_ZIP_SHA256.to_ascii_uppercase(),
            SOCKSEEK_ZIP_SHA256
        ));
        assert!(!hash_matches(
            &SOCKSEEK_ZIP_SHA256[..63],
            SOCKSEEK_ZIP_SHA256
        ));
        assert!(!hash_matches(&"g".repeat(64), SOCKSEEK_ZIP_SHA256));
        let different = "0".repeat(64);
        assert!(!hash_matches(
            &different.to_ascii_uppercase(),
            SOCKSEEK_ZIP_SHA256
        ));
    }
}
