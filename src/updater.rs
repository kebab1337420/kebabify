//! Self-updater — checks GitHub releases, downloads and stages the new binary.
//!
//! Triggered two ways: `kebabify update` in a console, or the update badge in
//! the Spotify client (which calls the proxy's `/update/apply`, see
//! `audio_proxy`). Either way the flow is: check → download → stage
//! `kebabify.exe.new` next to the running exe → hand over to a detached
//! swap helper (Windows cannot replace a running exe) → relaunch `apply`.
//! Spotify itself keeps running with the old extension until the user
//! restarts it — the badge says so.

use anyhow::{anyhow, Context, Result};

/// GitHub API endpoint listing the latest release.
const RELEASES_LATEST: &str = "https://api.github.com/repos/kebab1337420/kebabify/releases/latest";

/// Expected asset name on the release.
const ASSET_NAME: &str = "kebabify.exe";

/// How long an update check stays cached (GitHub rate-limits anonymous API
/// calls to 60/hour/IP — the proxy checks at most every 30 min anyway).
pub const CHECK_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// A newer release with a downloadable binary.
pub struct ReleaseInfo {
    /// Bare version, e.g. `0.5.0`.
    pub version: String,
    /// Direct download URL of the exe asset.
    pub download_url: String,
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
    let url = json
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(ASSET_NAME))?
        .get("browser_download_url")?
        .as_str()?;
    Some(ReleaseInfo {
        version: tag.strip_prefix('v').unwrap_or(tag).to_string(),
        download_url: url.to_string(),
    })
}

/// Streams the release asset to `dest`.
pub async fn download_release(
    client: &reqwest::Client,
    info: &ReleaseInfo,
    dest: &std::path::Path,
) -> Result<()> {
    let mut resp = client
        .get(&info.download_url)
        .timeout(std::time::Duration::from_secs(120))
        .header("User-Agent", crate::lucida::STOCK_UA)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .context("Update download failed")?
        .error_for_status()
        .context("Update download returned an error")?;
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("Cannot write {}", dest.display()))?;
    use std::io::Write;
    while let Some(chunk) = resp.chunk().await.context("Update download interrupted")? {
        file.write_all(&chunk)
            .context("Failed to write update file")?;
    }
    file.flush().ok();
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if size < 1024 {
        return Err(anyhow!("Downloaded update is suspiciously small"));
    }
    Ok(())
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
        // binary can be replaced. Quoted paths survive spaces; no console.
        let script = format!(
            "timeout /t 3 /nobreak >nul & move /Y \"{}\" \"{}\" & start \"\" \"{}\" apply",
            staged.display(),
            current_exe.display(),
            current_exe.display()
        );
        let mut cmd = std::process::Command::new("cmd.exe");
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
                {"name": "kebabify.exe", "browser_download_url": "https://x/kebabify.exe"},
                {"name": "notes.txt", "browser_download_url": "https://x/notes.txt"},
            ],
        });
        let r = find_release(&payload, "0.4.0").unwrap();
        assert_eq!(r.version, "0.5.0");
        assert_eq!(r.download_url, "https://x/kebabify.exe");

        assert!(find_release(&payload, "0.5.0").is_none());
        assert!(find_release(&payload, "0.9.0").is_none());
        let no_asset = serde_json::json!({"tag_name": "v0.6.0", "assets": []});
        assert!(find_release(&no_asset, "0.4.0").is_none());
        assert!(find_release(&serde_json::json!({}), "0.4.0").is_none());
    }
}
