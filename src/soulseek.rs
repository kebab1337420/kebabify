//! Soulseek P2P source (priority #1) — real FLAC via the Soulseek network.
//!
//! HTTP rippers (lucida, SpotiFLAC backends) all drink from the same
//! community proxy pools toward Tidal/Qobuz/Amazon: when rightsholders take
//! those down, everything fails together (verified live 23/09/2026). Soulseek
//! is peer-to-peer — no central server to kill — so it sits first in the
//! chain: soulseek → lucida → saavn.
//!
//! Chain:
//! 1. Spotify embed page → "Artist Title" query (reuses
//!    [`crate::saavn`] metadata parsing — one parser, no drift).
//! 2. `sockseek` binary (fiso64, né `slsk-batchdl`):
//!    `<query> --input-type string -s --format flac --user U --pass P
//!     -o <staging> --no-listen --shared-files 0 --shared-folders 0
//!     --no-config`.
//! 3. The `.flac` moves into the cache as `<trackid>.flac`, validated
//!    (`fLaC` magic + size floor) before anything is served.
//!
//! Security posture (deliberate, do not relax without thinking):
//! - `--no-listen`: no inbound listening port, outbound-only connections.
//! - `--shared-files 0 --shared-folders 0`: download-only, never upload.
//! - One P2P run at a time (Soulseek kicks concurrent logins).
//! - Files are validated before serving; garbage is deleted, not served.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

/// A resolved, cached FLAC file ready to be served from disk.
pub struct SoulseekFile {
    /// Cache path (`<cache>/<trackid>.flac`), validated.
    pub path: PathBuf,
    /// File length in bytes (for Content-Length / Content-Range).
    pub len: u64,
}

/// Budget for one P2P download. P2P means queues and slow peers: the verified
/// live run took ~1-2 min. Past this the player has long given up — fail over
/// to lucida/saavn instead of holding the connection forever.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(480);

/// Minimum plausible size for a FLAC track. Anything smaller is a stub,
/// an error page, or a mislabeled stub — delete, don't serve.
const MIN_FLAC_BYTES: u64 = 512 * 1024;

/// One Soulseek session at a time: concurrent logins on the same account
/// kick each other off the network.
static SLOT: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(1));

/// Endpoint set for the metadata step. `Default` is production; tests inject
/// a local mock.
#[derive(Clone, Debug)]
pub struct SoulseekEndpoints {
    /// Spotify embed host, e.g. `https://open.spotify.com`.
    pub embed_base: String,
}

impl Default for SoulseekEndpoints {
    fn default() -> Self {
        Self {
            embed_base: "https://open.spotify.com".to_string(),
        }
    }
}

/// Resolves a Spotify track ID to a cached, validated FLAC file.
///
/// Fast path: a valid `<trackid>.flac` already in cache is returned without
/// touching the network. Otherwise one serialized `sockseek` run downloads
/// it. No credentials or no binary = immediate error (fail-fast so the chain
/// falls through to lucida without wasting the player's patience).
pub async fn open_file(client: &reqwest::Client, track_id: &str) -> Result<SoulseekFile> {
    open_file_with(client, &SoulseekEndpoints::default(), track_id).await
}

async fn open_file_with(
    client: &reqwest::Client,
    endpoints: &SoulseekEndpoints,
    track_id: &str,
) -> Result<SoulseekFile> {
    let (user, pass) = credentials().context(
        "Soulseek: no credentials — run `kebabify soulseek <user> <pass>` (or set SOULSEEK_USER/SOULSEEK_PASS)",
    )?;
    let binary = find_binary()?;
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).context("Soulseek: cannot create cache dir")?;
    let cached = dir.join(format!("{}.flac", track_id));

    // Fast path first: a previous run may already have this track.
    if let Ok(len) = validate_flac(&cached) {
        return Ok(SoulseekFile { path: cached, len });
    } else if cached.exists() {
        // Stale/corrupt entry: drop it so it can never be served.
        let _ = std::fs::remove_file(&cached);
    }

    let meta = crate::saavn::track_meta(client, track_id, &endpoints.embed_base)
        .await
        .context("Soulseek: Spotify metadata lookup failed")?;
    let query = search_query(&meta.artist, &meta.title);

    // Serialize P2P runs: the slot is held for the whole download.
    let _slot = SLOT
        .acquire()
        .await
        .context("Soulseek: session pool shut down")?;

    // Re-check under the slot: a concurrent request may have filled the
    // cache while we waited (the proxy semaphore still allows several
    // resolutions in flight).
    if let Ok(len) = validate_flac(&cached) {
        return Ok(SoulseekFile { path: cached, len });
    }

    let staging = dir.join(format!(".staging-{}", track_id));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging).context("Soulseek: cannot create staging dir")?;

    let args = build_args(&user, &pass, &query, &staging);
    eprintln!(
        "[kebabify] Soulseek: downloading \"{}\" (P2P, up to 8 min)…",
        query
    );
    if let Err(e) = run_sockseek(&binary, &staging, &args).await {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e.context(format!("Soulseek: download failed for \"{}\"", query)));
    }

    let downloaded = pick_flac(&staging).context("Soulseek: no FLAC in downloader output")?;
    // Atomic-ish publish: move into place, then validate the final path.
    if cached.exists() {
        let _ = std::fs::remove_file(&cached);
    }
    std::fs::rename(&downloaded, &cached).context("Soulseek: cannot publish to cache")?;
    let _ = std::fs::remove_dir_all(&staging);
    let len = validate_flac(&cached).context("Soulseek: downloaded file failed validation")?;
    eprintln!(
        "[kebabify] Soulseek: cached {} ({} bytes)",
        cached.display(),
        len
    );
    Ok(SoulseekFile { path: cached, len })
}

/// Runs the downloader with a hard timeout. On timeout the child is killed:
/// an orphaned P2P client lingering after the request is exactly the kind of
/// background network presence we promised not to have.
///
/// The child runs with `staging` as its working dir (a detached proxy may
/// sit in an unwritable CWD) and piped stdio: on failure the stderr tail
/// goes into the error so `proxy.log` says WHY, not just "exit code 1".
async fn run_sockseek(binary: &Path, staging: &Path, args: &[String]) -> Result<()> {
    let child = tokio::process::Command::new(binary)
        .args(args)
        .current_dir(staging)
        // The child inherits nothing sensitive: creds travel as args (it
        // needs them there).
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Soulseek: cannot launch downloader binary")?;
    let output = match tokio::time::timeout(DOWNLOAD_TIMEOUT, child.wait_with_output()).await {
        Ok(out) => out.context("Soulseek: downloader wait failed")?,
        // On expiry the timed-out future is dropped, and kill_on_drop(true)
        // above kills the child with it — no orphaned P2P client.
        Err(_) => {
            return Err(anyhow!(
                "Soulseek: download timed out after {}s — falling through",
                DOWNLOAD_TIMEOUT.as_secs()
            ));
        }
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr_tail: String = String::from_utf8_lossy(&output.stderr)
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Err(anyhow!(
        "Soulseek: downloader exited with status {} for this track (stderr tail: {})",
        output.status,
        stderr_tail.trim()
    ))
}

/// CLI args for one download. Pure for tests — the security flags live here
/// so a review sees them in one place.
fn build_args(user: &str, pass: &str, query: &str, staging: &Path) -> Vec<String> {
    vec![
        query.to_string(),
        "--input-type".to_string(),
        "string".to_string(),
        "-s".to_string(),
        "--format".to_string(),
        "flac".to_string(),
        "--user".to_string(),
        user.to_string(),
        "--pass".to_string(),
        pass.to_string(),
        "-o".to_string(),
        staging.to_string_lossy().into_owned(),
        "--no-listen".to_string(),
        "--shared-files".to_string(),
        "0".to_string(),
        "--shared-folders".to_string(),
        "0".to_string(),
        "--no-config".to_string(),
        // No index file next to the music: the cache is keyed by track ID,
        // an index would only litter it (and fail in a read-only CWD).
        "--no-write-index".to_string(),
    ]
}

/// Song-mode query: artist + title, trimmed. Pure for tests.
fn search_query(artist: &str, title: &str) -> String {
    format!("{} {}", artist.trim(), title.trim())
        .trim()
        .to_string()
}

/// Largest `.flac` under `dir` (recursive). The downloader names files its
/// own way; size is the only robust selector. Pure-ish for tests (takes any
/// dir).
fn pick_flac(dir: &Path) -> Option<PathBuf> {
    fn visit(dir: &Path, best: &mut Option<(u64, PathBuf)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, best);
            } else if path.extension().is_some_and(|e| e == "flac") {
                if let Ok(m) = std::fs::metadata(&path) {
                    if best.as_ref().is_none_or(|(size, _)| m.len() > *size) {
                        *best = Some((m.len(), path));
                    }
                }
            }
        }
    }
    let mut best = None;
    visit(dir, &mut best);
    best.map(|(_, path)| path)
}

/// Validates a cached/downloaded file: `fLaC` magic + size floor. Returns the
/// length for Content-Length. Pure I/O, no network — safe to call on every
/// request (4 bytes + metadata).
fn validate_flac(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path).context("Soulseek: file missing")?;
    if meta.len() < MIN_FLAC_BYTES {
        return Err(anyhow!(
            "Soulseek: file too small to be FLAC ({} bytes)",
            meta.len()
        ));
    }
    let mut header = [0u8; 4];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(path).context("Soulseek: cannot open file")?;
        f.read_exact(&mut header)
            .context("Soulseek: cannot read file header")?;
    }
    if &header != b"fLaC" {
        return Err(anyhow!("Soulseek: bad magic — not a FLAC file"));
    }
    Ok(meta.len())
}

/// Locates the downloader binary: our own bin dir first (where `apply` can
/// place it), then PATH (`sockseek`, legacy `sldl`).
fn find_binary() -> Result<PathBuf> {
    if let Some(dir) = data_dir() {
        let bundled = dir.join("bin").join(binary_name());
        if bundled.is_file() {
            return Ok(bundled);
        }
    }
    for name in ["sockseek", "sockseek.exe", "sldl", "sldl.exe"] {
        if let Ok(p) = which::which(name) {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "Soulseek: downloader binary not found — put sockseek.exe in %APPDATA%\\Kebabify\\bin\\ (see https://github.com/fiso64/sockseek/releases)"
    ))
}

#[cfg(windows)]
fn binary_name() -> &'static str {
    "sockseek.exe"
}

#[cfg(not(windows))]
fn binary_name() -> &'static str {
    "sockseek"
}

/// Per-user data dir (credentials, binary, cache parent). Same root as the
/// lucida cookies so everything lives in one place.
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("Kebabify"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".kebabify")))
}

/// Public so `status` and the binary-placement hint agree on the location.
pub fn binary_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("bin").join(binary_name()))
}

/// Cache dir for downloaded FLACs. `KEBABIFY_SOULSEEK_CACHE` overrides (tests).
/// Prefers LOCALAPPDATA (roaming profiles must not carry 25 MB tracks).
fn cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("KEBABIFY_SOULSEEK_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("Kebabify").join("cache");
    }
    data_dir()
        .map(|d| d.join("cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("kebabify-cache"))
}

/// Stored login: line 1 = username, line 2 = password. Env pair
/// (`SOULSEEK_USER` + `SOULSEEK_PASS`) wins when both are set.
fn credentials() -> Result<(String, String)> {
    match (
        std::env::var("SOULSEEK_USER").ok(),
        std::env::var("SOULSEEK_PASS").ok(),
    ) {
        (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => return Ok((u, p)),
        _ => {}
    }
    let path = credentials_path()?;
    let content = std::fs::read_to_string(&path).context("Soulseek: no stored credentials")?;
    let mut lines = content.lines();
    match (lines.next(), lines.next()) {
        (Some(u), Some(p)) if !u.trim().is_empty() && !p.is_empty() => {
            Ok((u.trim().to_string(), p.to_string()))
        }
        _ => Err(anyhow!("Soulseek: credentials file malformed")),
    }
}

/// True when a login is available (env or file). For `status`.
pub fn has_credentials() -> bool {
    credentials().is_ok()
}

/// Public accessor so `status` can point at the file.
pub fn credentials_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("KEBABIFY_SOULSEEK_CREDS") {
        return Ok(PathBuf::from(p));
    }
    data_dir()
        .map(|d| d.join("soulseek.txt"))
        .context("Soulseek: no data dir on this platform")
}

/// Stores the login (never read back for display — the password is write-only).
pub fn save_credentials(user: &str, pass: &str) -> Result<PathBuf> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Soulseek: cannot create data dir")?;
    }
    std::fs::write(&path, format!("{}\n{}\n", user.trim(), pass))
        .context("Soulseek: cannot write credentials file")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_joins_artist_and_title() {
        assert_eq!(
            search_query("Rick Astley", "Never Gonna Give You Up"),
            "Rick Astley Never Gonna Give You Up"
        );
        assert_eq!(
            search_query("  Daft Punk ", " Get Lucky "),
            "Daft Punk Get Lucky"
        );
    }

    #[test]
    fn args_carry_security_flags() {
        let args = build_args("u", "p", "Artist Title", Path::new("C:\\stage"));
        let has = |flag: &str| args.iter().any(|a| a == flag);
        assert!(
            has("--input-type")
                && args[args.iter().position(|a| a == "--input-type").unwrap() + 1] == "string"
        );
        assert!(has("-s"));
        assert!(
            has("--format")
                && args[args.iter().position(|a| a == "--format").unwrap() + 1] == "flac"
        );
        assert!(has("--no-listen"));
        assert!(
            has("--shared-files")
                && args[args.iter().position(|a| a == "--shared-files").unwrap() + 1] == "0"
        );
        assert!(
            has("--shared-folders")
                && args[args.iter().position(|a| a == "--shared-folders").unwrap() + 1] == "0"
        );
        assert!(has("--no-config"));
        assert!(has("--no-write-index"));
    }

    /// One temp dir for the file-validation tests (created once, removed at
    /// the end of each test that uses it).
    fn temp_flac_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kebabify-slsk-test-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn valid_flac_passes() {
        let dir = temp_flac_dir("valid");
        let path = dir.join("ok.flac");
        let mut bytes = b"fLaC".to_vec();
        bytes.resize(MIN_FLAC_BYTES as usize + 16, 0);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(validate_flac(&path).unwrap(), bytes.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_and_stubs_rejected() {
        let dir = temp_flac_dir("bad");
        let mp3 = dir.join("fake.flac");
        let mut bytes = b"ID3".to_vec();
        bytes.resize(MIN_FLAC_BYTES as usize + 16, 0);
        std::fs::write(&mp3, &bytes).unwrap();
        assert!(validate_flac(&mp3).is_err());
        let tiny = dir.join("tiny.flac");
        std::fs::write(&tiny, b"fLaC").unwrap();
        assert!(validate_flac(&tiny).is_err());
        assert!(validate_flac(&dir.join("missing.flac")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picks_largest_flac() {
        let dir = temp_flac_dir("pick");
        std::fs::write(dir.join("a.mp3"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("small.flac"), vec![0u8; 10]).unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("big.flac"), vec![0u8; 50]).unwrap();
        assert_eq!(pick_flac(&dir).unwrap(), sub.join("big.flac"));
        assert!(pick_flac(&dir.join("nothing-here")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Credentials round-trip in one test: env vars are process-global, so
    /// parallel tests would race (same lesson as the cookie tests).
    #[test]
    fn credentials_roundtrip_and_missing() {
        let dir = temp_flac_dir("creds");
        let file = dir.join("soulseek.txt");
        unsafe {
            std::env::set_var("KEBABIFY_SOULSEEK_CREDS", &file);
        }
        assert!(!has_credentials());
        save_credentials("someuser", "s3cr3t!").unwrap();
        let (u, p) = credentials().unwrap();
        assert_eq!((u.as_str(), p.as_str()), ("someuser", "s3cr3t!"));
        unsafe {
            std::env::remove_var("KEBABIFY_SOULSEEK_CREDS");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
