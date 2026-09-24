//! Self-updater — checks GitHub releases, downloads and stages the new binary.
//!
//! Triggered two ways: `kebabify update` in a console, or the update badge in
//! the Spotify client (which calls the proxy's `/update/apply`, see
//! `audio_proxy`). Either way the flow is: check → download → stage
//! `kebabify.exe.new` next to the running exe → hand over to a detached
//! swap helper (Windows cannot replace a running exe) → relaunch `apply`.
//! Spotify itself keeps running with the old extension until the user
//! restarts it — the badge says so.

use anyhow::{Context, Result, anyhow};

/// GitHub API endpoint listing the latest release.
const RELEASES_LATEST: &str = "https://api.github.com/repos/kebab1337420/kebabify/releases/latest";

/// Expected asset name on the release.
const ASSET_NAME: &str = "kebabify.exe";

/// Checksum sidecar asset name (`"<hex>  kebabify.exe"`).
const SHA_ASSET_NAME: &str = "kebabify.exe.sha256";

/// Ceiling on a release asset. The real binary is a few MB; a request
/// timeout bounds time, never bytes, so an endless body would otherwise fill
/// the disk before any error surfaces.
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;

/// Hosts a release download may come from (TLS protects the rest; this pins
/// the trust root so a poisoned JSON payload can't point us anywhere else).
/// `github.com` URLs are additionally path-pinned to our repo's download
/// tree, so a payload pointing at an attacker's own repo fails too;
/// `objects.githubusercontent.com` serves opaque signed URLs (no stable path
/// to pin — the host pin is the check there).
fn pinned_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).context("Bad release URL")?;
    if url.scheme() != "https" {
        return Err(anyhow!("Release URL is not https"));
    }
    // Default port only: an explicit port is either a downgrade attempt or a
    // service we never asked for.
    if url.port_or_known_default() != Some(443) {
        return Err(anyhow!("Release URL port not allowed"));
    }
    match url.host_str() {
        Some("github.com") => {
            if url
                .path()
                .starts_with("/kebab1337420/kebabify/releases/download/")
            {
                Ok(url)
            } else {
                Err(anyhow!("Release URL path not allowed"))
            }
        }
        Some("objects.githubusercontent.com") => Ok(url),
        _ => Err(anyhow!("Release URL host not allowed")),
    }
}

/// Rejects off-host redirects: after `.send()`, the final URL must either
/// still pass [`pinned_url`] or share the original URL's host (reqwest
/// follows redirects by default, and only the JSON URL was pinned before).
fn redirect_ok(original: &str, resp: &reqwest::Response) -> Result<()> {
    let before = url::Url::parse(original).context("Bad release URL")?;
    let after = resp.url();
    // Production path: the original URL was pin-eligible, so every hop must
    // re-pass the full policy. The fallback only triggers for origins that
    // could never be pinned (a loopback mock in tests): same origin, nothing
    // looser.
    let ok = if pinned_url(before.as_str()).is_ok() {
        redirect_target_ok(&before, after)
    } else {
        after.scheme() == before.scheme() && after.host_str() == before.host_str()
    };
    if ok {
        Ok(())
    } else {
        Err(anyhow!(
            "Release download redirected off-host: {}",
            after.host_str().unwrap_or("?")
        ))
    }
}

/// Pure core of [`redirect_ok`], unit-testable without a response. Every hop
/// must re-pass the full policy: accepting "same host as the original" would
/// let a redirect leave the pinned download tree or downgrade to cleartext.
fn redirect_target_ok(before: &url::Url, after: &url::Url) -> bool {
    before.scheme() == "https" && after.scheme() == "https" && pinned_url(after.as_str()).is_ok()
}

/// How long an update check stays cached (GitHub rate-limits anonymous API
/// calls to 60/hour/IP — the proxy checks at most every 30 min anyway).
pub const CHECK_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// A newer release with a downloadable binary.
pub struct ReleaseInfo {
    /// Bare version, e.g. `0.5.0`.
    pub version: String,
    /// Direct download URL of the exe asset (host-pinned).
    pub download_url: String,
    /// Checksum sidecar URL, when the release publishes one.
    pub sha_url: Option<String>,
}

/// Parses `v0.5.0` / `0.5.0` into `(major, minor, patch)`. Pure.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.strip_prefix('v').unwrap_or(tag);
    let mut it = t.split('.');
    let parts: Vec<Option<u64>> = vec![
        it.next()?.parse().ok(),
        it.next()?.parse().ok(),
        it.next()?.parse().ok(),
    ];
    if it.next().is_some() {
        return None;
    }
    Some((parts[0]?, parts[1]?, parts[2]?))
}

/// Whether `latest` is strictly newer than `current`. Pure.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

/// Current binary version.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Checks GitHub for a newer release with a `kebabify.exe` asset.
/// `Ok(None)` = up to date (or no usable asset).
pub async fn check_update(client: &reqwest::Client) -> Result<Option<ReleaseInfo>> {
    let json: serde_json::Value = client
        .get(RELEASES_LATEST)
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", crate::lucida::STOCK_UA)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("Update check failed")?
        .error_for_status()
        .context("GitHub releases API error")?
        .json()
        .await
        .context("Failed to parse releases response")?;
    Ok(find_release(&json, current_version()))
}

/// Picks the release from an API payload. Pure for tests.
fn find_release(json: &serde_json::Value, current: &str) -> Option<ReleaseInfo> {
    let tag = json.get("tag_name")?.as_str()?;
    if !is_newer(current, tag) {
        return None;
    }
    let assets = json.get("assets")?.as_array()?;
    let asset = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(ASSET_NAME))?;
    let download_url = asset.get("browser_download_url")?.as_str()?;
    let sha_url = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(SHA_ASSET_NAME))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .map(str::to_string);
    // Host-pinned here so a poisoned payload fails before any download.
    pinned_url(download_url).ok()?;
    if let Some(ref sha) = sha_url {
        pinned_url(sha).ok()?;
    }
    Some(ReleaseInfo {
        version: tag.strip_prefix('v').unwrap_or(tag).to_string(),
        download_url: download_url.to_string(),
        sha_url,
    })
}

/// Streams the release asset to `dest`, verified when the release publishes
/// a checksum. A failed download never leaves a partial file behind: the
/// bytes land in a unique `.part` file and are published onto `dest` only
/// after the checksum matches, so a second writer can never truncate or
/// delete a staged binary that is being verified.
pub async fn download_release(
    client: &reqwest::Client,
    info: &ReleaseInfo,
    dest: &std::path::Path,
) -> Result<()> {
    let result = download_inner(client, info, dest).await;
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Sibling `.part` path unique to this process and attempt. Never a fixed
/// name: a predictable staging file is a pre-creation target for another
/// local process.
fn part_path(dest: &std::path::Path) -> Option<std::path::PathBuf> {
    let nonce = format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos()) | (d.as_secs() << 20))
            .unwrap_or(0)
    );
    let mut name = dest.file_name()?.to_os_string();
    name.push(format!(".{}.part", nonce));
    Some(dest.with_file_name(name))
}

async fn download_inner(
    client: &reqwest::Client,
    info: &ReleaseInfo,
    dest: &std::path::Path,
) -> Result<()> {
    let part = part_path(dest).context("Cannot derive staging path for update")?;
    // RAII: every early return below (transport error, cap, short file,
    // checksum failure) drops the staging bytes. Only the verified rename
    // publishes them.
    let mut staged = Some(StagedPart(part.clone()));
    let result = download_to_part(client, info, dest, &part).await;
    match result {
        Ok(()) => {
            // Published: keep the file.
            std::mem::forget(staged.take().expect("staging guard present"));
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Removes its file unless forgotten, so no failure path can strand a
/// half-downloaded or unverified binary on disk.
struct StagedPart(std::path::PathBuf);

impl Drop for StagedPart {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn download_to_part(
    client: &reqwest::Client,
    info: &ReleaseInfo,
    dest: &std::path::Path,
    part: &std::path::Path,
) -> Result<()> {
    let mut resp = client
        .get(&info.download_url)
        .timeout(std::time::Duration::from_secs(120))
        .header("User-Agent", crate::lucida::STOCK_UA)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .context("Update download failed")?;
    redirect_ok(&info.download_url, &resp)?;
    resp = resp
        .error_for_status()
        .context("Update download returned an error")?;
    if let Some(declared) = resp.content_length() {
        if declared > MAX_UPDATE_BYTES {
            return Err(anyhow!(
                "Update asset declares {} bytes, above the {} byte cap",
                declared,
                MAX_UPDATE_BYTES
            ));
        }
    }
    let mut file =
        std::fs::File::create(part).with_context(|| format!("Cannot write {}", part.display()))?;
    use std::io::Write;
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("Update download interrupted")? {
        written += chunk.len() as u64;
        if written > MAX_UPDATE_BYTES {
            return Err(anyhow!(
                "Update asset exceeds the {} byte cap",
                MAX_UPDATE_BYTES
            ));
        }
        file.write_all(&chunk)
            .context("Failed to write update file")?;
    }
    file.flush().ok();
    drop(file);
    let size = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if size < 1024 {
        let _ = std::fs::remove_file(part);
        return Err(anyhow!("Downloaded update is suspiciously small"));
    }
    verify_checksum(client, info, part).await?;
    // Verified: publish onto the path the swap helper expects.
    std::fs::rename(part, dest).with_context(|| {
        format!(
            "Cannot publish verified update {} -> {}",
            part.display(),
            dest.display()
        )
    })?;
    Ok(())
}

/// Verifies the downloaded file against the release's sha256 sidecar,
/// fail-closed: a release without a sidecar is refused (a poisoned payload
/// would simply omit it). Sidecars are published since v0.6.0.
async fn verify_checksum(
    client: &reqwest::Client,
    info: &ReleaseInfo,
    dest: &std::path::Path,
) -> Result<()> {
    let Some(sha_url) = info.sha_url.as_deref() else {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!(
            "Release has no checksum sidecar — refusing an unverifiable binary"
        ));
    };
    let resp = client
        .get(sha_url)
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", crate::lucida::STOCK_UA)
        .send()
        .await
        .context("Checksum download failed")?;
    redirect_ok(sha_url, &resp)?;
    let body = resp
        .error_for_status()
        .context("Checksum download error")?
        .text()
        .await
        .context("Failed to read checksum")?;
    let expected = body
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("Checksum file is empty"))?
        .to_lowercase();
    if !expected.chars().all(|c| c.is_ascii_hexdigit()) || expected.len() != 64 {
        return Err(anyhow!("Checksum file is malformed"));
    }
    let actual = sha256_file(dest)?;
    if actual != expected {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!(
            "Update checksum mismatch — refusing a possibly tampered binary"
        ));
    }
    Ok(())
}

/// Hex SHA-256 of a file. Pure IO, no network.
fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file =
        std::fs::File::open(path).with_context(|| format!("Cannot hash {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("Failed to hash update file")?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// `kebabify.exe` → `kebabify.exe.new` staged next to it.
pub fn staged_path(exe: &std::path::Path) -> std::path::PathBuf {
    exe.with_extension("exe.new")
}

/// Hands the staged update to a detached swap helper and returns: on Windows
/// a `cmd` waits for this process to die, moves the new exe over the old
/// one, and relaunches `apply` (re-patch with the new version). Elsewhere,
/// stage + print manual swap instructions.
pub fn stage_and_relaunch(current_exe: &std::path::Path, staged: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // `timeout` + `move` + `start`: the helper outlives us, so the dead
        // binary can be replaced. Quoted paths survive spaces; `%` is doubled
        // because cmd.exe expands %VAR% even inside quotes. No console.
        // A failed `move` still launches the old binary (acceptable fallback).
        let escape = |p: &std::path::Path| p.display().to_string().replace('%', "%%");
        let script = format!(
            "timeout /t 3 /nobreak >nul & move /Y \"{}\" \"{}\" & start \"\" \"{}\" apply",
            escape(staged),
            escape(current_exe),
            escape(current_exe)
        );
        // The shell is invoked by absolute path: an unqualified `cmd.exe` is
        // resolved through the current directory and PATH, so a planted
        // cmd.exe would run with our privileges before the swap even starts.
        let shell = std::env::var("SystemRoot")
            .map(|root| {
                std::path::PathBuf::from(root)
                    .join("System32")
                    .join("cmd.exe")
            })
            .unwrap_or_else(|_| std::path::PathBuf::from("cmd.exe"));
        let mut cmd = std::process::Command::new(shell);
        cmd.args(["/C", &script]);
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn update helper")?;
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (current_exe, staged);
        Err(anyhow!(
            "Update staged — replace the binary manually and re-run apply"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parsed_and_compared() {
        assert_eq!(parse_version("v0.5.0"), Some((0, 5, 0)));
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version("v10.2.33"), Some((10, 2, 33)));
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("v0.4"), None);
        assert_eq!(parse_version("v0.4.0.1"), None);
        assert_eq!(parse_version(""), None);
        assert!(is_newer("0.4.0", "v0.5.0"));
        assert!(is_newer("0.4.0", "0.4.1"));
        assert!(!is_newer("0.5.0", "0.5.0"));
        assert!(!is_newer("0.5.0", "0.4.9"));
        assert!(!is_newer("0.4.0", "garbage"));
    }

    #[test]
    fn release_picked_only_when_newer_with_asset() {
        let payload = serde_json::json!({
            "tag_name": "v0.5.0",
            "assets": [
                {"name": "kebabify.exe", "browser_download_url": "https://github.com/kebab1337420/kebabify/releases/download/v0.5.0/kebabify.exe"},
                {"name": "kebabify.exe.sha256", "browser_download_url": "https://github.com/kebab1337420/kebabify/releases/download/v0.5.0/kebabify.exe.sha256"},
                {"name": "notes.txt", "browser_download_url": "https://github.com/x/notes.txt"},
            ],
        });
        let r = find_release(&payload, "0.4.0").unwrap();
        assert_eq!(r.version, "0.5.0");
        assert_eq!(
            r.sha_url.as_deref(),
            Some(
                "https://github.com/kebab1337420/kebabify/releases/download/v0.5.0/kebabify.exe.sha256"
            )
        );

        assert!(find_release(&payload, "0.5.0").is_none());
        assert!(find_release(&payload, "0.9.0").is_none());
        let no_asset = serde_json::json!({"tag_name": "v0.6.0", "assets": []});
        assert!(find_release(&no_asset, "0.4.0").is_none());
        assert!(find_release(&serde_json::json!({}), "0.4.0").is_none());
        // Poisoned payload: off-host asset URL rejected.
        let evil = serde_json::json!({
            "tag_name": "v0.9.9",
            "assets": [{"name": "kebabify.exe", "browser_download_url": "https://evil.com/kebabify.exe"}],
        });
        assert!(find_release(&evil, "0.4.0").is_none());
    }

    #[test]
    fn hosts_pinned() {
        assert!(
            pinned_url(
                "https://github.com/kebab1337420/kebabify/releases/download/v0.5.0/kebabify.exe"
            )
            .is_ok()
        );
        assert!(pinned_url("https://objects.githubusercontent.com/x/y").is_ok());
        assert!(pinned_url("http://github.com/x").is_err());
        assert!(pinned_url("https://evil.com/kebabify.exe").is_err());
        assert!(pinned_url("not a url").is_err());
        // Attacker's own repo passes the host pin but fails the path pin.
        assert!(
            pinned_url("https://github.com/attacker/evil/releases/download/v9/kebabify.exe")
                .is_err()
        );
        // Poisoned payload with attacker's repo: rejected in find_release too.
        let evil_repo = serde_json::json!({
            "tag_name": "v0.9.9",
            "assets": [{"name": "kebabify.exe", "browser_download_url": "https://github.com/attacker/evil/releases/download/v0.9.9/kebabify.exe"}],
        });
        assert!(find_release(&evil_repo, "0.4.0").is_none());
    }

    #[test]
    fn redirect_stays_on_host() {
        let dl = url::Url::parse(
            "https://github.com/kebab1337420/kebabify/releases/download/v0.6.0/kebabify.exe",
        )
        .unwrap();
        // Same pinned tree: fine.
        let same = url::Url::parse(
            "https://github.com/kebab1337420/kebabify/releases/download/v0.7.0/kebabify.exe",
        )
        .unwrap();
        assert!(redirect_target_ok(&dl, &same));
        // Hop onto the CDN host: still pinned.
        let cdn = url::Url::parse("https://objects.githubusercontent.com/abc").unwrap();
        assert!(redirect_target_ok(&dl, &cdn));
        // Hop off-host entirely: rejected.
        let evil = url::Url::parse("https://evil.com/kebabify.exe").unwrap();
        assert!(!redirect_target_ok(&dl, &evil));
    }

    /// A same-host hop that leaves the pinned download tree is an escape, and
    /// a cleartext hop is a downgrade: both must be refused even though the
    /// host matches the original URL.
    #[test]
    fn redirect_cannot_escape_pinned_tree_or_downgrade() {
        let dl = url::Url::parse(
            "https://github.com/kebab1337420/kebabify/releases/download/v0.6.0/kebabify.exe",
        )
        .unwrap();
        let off_tree =
            url::Url::parse("https://github.com/attacker/evil/releases/download/v9/kebabify.exe")
                .unwrap();
        assert!(!redirect_target_ok(&dl, &off_tree));
        let cleartext = url::Url::parse(
            "http://github.com/kebab1337420/kebabify/releases/download/v0.7.0/kebabify.exe",
        )
        .unwrap();
        assert!(!redirect_target_ok(&dl, &cleartext));
        let odd_port = url::Url::parse(
            "https://github.com:8443/kebab1337420/kebabify/releases/download/v0.7.0/kebabify.exe",
        )
        .unwrap();
        assert!(!redirect_target_ok(&dl, &odd_port));
        let evil_cdn = url::Url::parse("https://objects.githubusercontent.com.evil.com/x").unwrap();
        assert!(!redirect_target_ok(&dl, &evil_cdn));
    }

    /// Staging must be unique per attempt: a fixed `.part` name is a
    /// pre-creation target for another local process and collides between
    /// concurrent downloads.
    #[test]
    fn staging_part_path_is_unique_and_sibling() {
        let dest = std::path::Path::new("C:/tools/kebabify.exe.new");
        let a = part_path(dest).unwrap();
        let b = part_path(dest).unwrap();
        assert_ne!(a, b, "staging path must differ between attempts");
        assert_eq!(a.parent(), dest.parent());
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("kebabify.exe.new."));
        assert!(name.ends_with(".part"));
    }

    /// A failed download must not leave either the destination or any staging
    /// residue behind.
    #[tokio::test]
    async fn failed_download_leaves_no_staging_residue() {
        let mock = MockServer::start(vec![Route::new("/file", vec![(200, vec![7u8; 4096])])]).await;
        let client = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("kebabify_residue_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let info = ReleaseInfo {
            version: "9.9.9".to_string(),
            download_url: format!("{}/file", mock.base_url),
            // Points at a valid-looking body whose hash cannot match, so the
            // failure happens after the bytes are on disk.
            sha_url: Some(format!("{}/file", mock.base_url)),
        };
        let dest = dir.join("kebabify.exe.new");
        assert!(download_release(&client, &info, &dest).await.is_err());
        assert!(!dest.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leftovers.is_empty(),
            "unexpected leftovers: {:?}",
            leftovers
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fail-closed: a payload without a checksum sidecar refuses the binary
    /// and leaves no file behind.
    #[tokio::test]
    async fn download_without_sidecar_refused() {
        let payload = vec![65u8; 2048];
        let mock = MockServer::start(vec![Route::new("/file", vec![(200, payload)])]).await;
        let client = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("kebabify_nosha_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = ReleaseInfo {
            version: "9.9.9".to_string(),
            download_url: format!("{}/file", mock.base_url),
            sha_url: None,
        };
        let dest = dir.join("kebabify.exe.new");
        assert!(download_release(&client, &info, &dest).await.is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::mock::{MockServer, Route};

    /// Download + checksum against a mock: pass-through on match, hard fail
    /// (and no leftover file) on tamper.
    #[tokio::test]
    async fn download_verified_and_tamper_rejected() {
        let payload = vec![65u8; 2048];
        let expected = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(&payload))
        };
        let mock = MockServer::start(vec![
            Route::new("/file", vec![(200, payload.clone())]),
            Route::new(
                "/sha",
                vec![(200, format!("{}  kebabify.exe\n", expected).into_bytes())],
            ),
        ])
        .await;
        let client = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("kebabify_update_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = ReleaseInfo {
            version: "9.9.9".to_string(),
            download_url: format!("{}/file", mock.base_url),
            sha_url: Some(format!("{}/sha", mock.base_url)),
        };
        let dest = dir.join("kebabify.exe.new");
        download_release(&client, &info, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);

        // Tampered bytes under the same checksum: rejected, nothing left.
        let mock2 = MockServer::start(vec![
            Route::new("/file", vec![(200, vec![66u8; 2048])]),
            Route::new(
                "/sha",
                vec![(200, format!("{}  kebabify.exe\n", expected).into_bytes())],
            ),
        ])
        .await;
        let info2 = ReleaseInfo {
            version: "9.9.9".to_string(),
            download_url: format!("{}/file", mock2.base_url),
            sha_url: Some(format!("{}/sha", mock2.base_url)),
        };
        let dest2 = dir.join("kebabify2.exe.new");
        assert!(download_release(&client, &info2, &dest2).await.is_err());
        assert!(!dest2.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
