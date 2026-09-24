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
pub(crate) const OPEN_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// Minimum plausible size for a FLAC track. Anything smaller is a stub,
/// an error page, or a mislabeled stub — delete, don't serve.
const MIN_FLAC_BYTES: u64 = 512 * 1024;
pub(crate) const MAX_FLAC_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const CACHE_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    if let Err(error) = std::fs::create_dir_all(&staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error).context("Soulseek: cannot create staging dir");
    }
    let _staging_guard = StagingDir(staging.clone());

    let args = build_args(&user, &pass, &query, &staging);
    eprintln!(
        "[kebabify] Soulseek: downloading \"{}\" (P2P, up to 8 min)…",
        query
    );
    if let Err(e) = run_sockseek(&binary, &staging, &args).await {
        return Err(e.context(format!("Soulseek: download failed for \"{}\"", query)));
    }

    let downloaded = pick_flac(&staging).context("Soulseek: no FLAC in downloader output")?;
    // Atomic-ish publish: move into place, then validate the final path.
    if cached.exists() {
        let _ = std::fs::remove_file(&cached);
    }
    std::fs::rename(&downloaded, &cached).context("Soulseek: cannot publish to cache")?;
    let len = validate_flac(&cached).context("Soulseek: downloaded file failed validation")?;
    evict_cache(&dir, &cached);
    eprintln!(
        "[kebabify] Soulseek: cached {} ({} bytes)",
        cached.display(),
        len
    );
    Ok(SoulseekFile { path: cached, len })
}

fn append_stderr_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    let excess = tail.len().saturating_sub(400);
    if excess > 0 {
        tail.drain(..excess);
    }
}

async fn drain_child_stderr(mut stderr: tokio::process::ChildStderr) -> Vec<u8> {
    let mut tail = Vec::with_capacity(400);
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stderr, &mut chunk).await {
            Ok(0) => break,
            Ok(n) => append_stderr_tail(&mut tail, &chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    tail
}

/// Runs the downloader with a hard timeout. On timeout the child is killed:
/// an orphaned P2P client lingering after the request is exactly the kind of
/// background network presence we promised not to have.
///
/// The child runs with `staging` as its working dir (a detached proxy may
/// sit in an unwritable CWD) and piped stdio: on failure the stderr tail
/// goes into the error so `proxy.log` says WHY, not just "exit code 1".
async fn run_sockseek(binary: &Path, staging: &Path, args: &[String]) -> Result<()> {
    let mut child = tokio::process::Command::new(binary)
        .args(args)
        .current_dir(staging)
        // The child inherits nothing sensitive: creds travel as args (it
        // needs them there).
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Soulseek: cannot launch downloader binary")?;
    let stderr = child
        .stderr
        .take()
        .context("Soulseek: downloader stderr unavailable")?;
    let runtime = tokio::runtime::Handle::current();
    let stderr_thread = std::thread::spawn(move || runtime.block_on(drain_child_stderr(stderr)));
    let status = match tokio::time::timeout(DOWNLOAD_TIMEOUT, child.wait()).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_thread.join();
            return Err(anyhow!(
                "Soulseek: download timed out after {}s — falling through",
                DOWNLOAD_TIMEOUT.as_secs()
            ));
        }
    };
    let stderr = stderr_thread.join().unwrap_or_default();
    let status = status.context("Soulseek: downloader wait failed")?;
    if status.success() {
        return Ok(());
    }
    let stderr_tail = String::from_utf8_lossy(&stderr);
    Err(anyhow!(
        "Soulseek: downloader exited with status {} for this track (stderr tail: {})",
        status,
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

const MAX_FLAC_DEPTH: usize = 32;
const MAX_FLAC_ENTRIES: usize = 100_000;

/// Largest `.flac` under `dir` (recursive). The downloader names files its
/// own way; size is the only robust selector. Pure-ish for tests (takes any
/// dir).
fn pick_flac(dir: &Path) -> Option<PathBuf> {
    pick_flac_with_limits(dir, MAX_FLAC_DEPTH, MAX_FLAC_ENTRIES)
}

fn pick_flac_with_limits(dir: &Path, max_depth: usize, max_entries: usize) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if visited >= max_entries {
                return best.map(|(_, path)| path);
            }
            visited += 1;
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if is_link_or_reparse_point(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                if depth < max_depth {
                    pending.push((path, depth + 1));
                }
            } else if path
                .extension()
                .is_some_and(|extension| extension == "flac")
                && best.as_ref().is_none_or(|(size, _)| metadata.len() > *size)
            {
                best = Some((metadata.len(), path));
            }
        }
    }
    best.map(|(_, path)| path)
}

fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
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
    if meta.len() > MAX_FLAC_BYTES {
        let _ = std::fs::remove_file(path);
        return Err(anyhow!(
            "Soulseek: file too large to be FLAC ({} bytes, max {})",
            meta.len(),
            MAX_FLAC_BYTES
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

fn evict_cache(dir: &Path, keep: &Path) {
    evict_cache_with_budget(dir, keep, CACHE_BUDGET_BYTES);
}

fn evict_cache_with_budget(dir: &Path, keep: &Path, budget: u64) {
    let mut files = Vec::new();
    let mut total = 0u64;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let path = entry.path();
        let len = metadata.len();
        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total = total.saturating_add(len);
        if path != keep {
            files.push((path, len, modified));
        }
    }
    if total <= budget {
        return;
    }
    files.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));
    for (path, len, _) in files {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
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

    #[test]
    fn stderr_drain_keeps_bounded_tail() {
        let input = (0..4096)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let mut tail = Vec::new();
        for chunk in input.chunks(1024) {
            append_stderr_tail(&mut tail, chunk);
        }
        assert_eq!(tail, input[input.len() - 400..]);
    }

    #[test]
    fn staging_guard_removes_directory() {
        let dir = temp_flac_dir("staging-guard");
        let staging = dir.join(".staging-test");
        std::fs::create_dir_all(&staging).unwrap();
        {
            let _guard = StagingDir(staging.clone());
        }
        assert!(!staging.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flac_walk_stops_at_symlink_cycle_or_limits() {
        let dir = temp_flac_dir("walk-limit");
        let cycle = dir.join("cycle");
        let cycle_created = {
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(&dir, &cycle).is_ok()
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&dir, &cycle).is_ok()
            }
            #[cfg(not(any(windows, unix)))]
            {
                false
            }
        };
        if cycle_created {
            assert!(pick_flac_with_limits(&dir, 3, 32).is_none());
        }
        let deep = dir.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("track.flac"), b"fLaC").unwrap();
        assert!(pick_flac_with_limits(&dir, 0, MAX_FLAC_ENTRIES).is_none());
        assert!(pick_flac_with_limits(&dir, MAX_FLAC_DEPTH, 0).is_none());
        if cycle_created {
            #[cfg(windows)]
            let _ = std::fs::remove_dir(&cycle);
            #[cfg(unix)]
            let _ = std::fs::remove_file(&cycle);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_flac_is_rejected_and_removed() {
        let dir = temp_flac_dir("oversized");
        let path = dir.join("oversized.flac");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FLAC_BYTES + 1).unwrap();
        assert!(validate_flac(&path).is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_modified(path: &Path, seconds: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds));
        file.set_times(times).unwrap();
    }

    #[test]
    fn eviction_removes_oldest_entries_first() {
        let dir = temp_flac_dir("eviction");
        let keep = dir.join("published.flac");
        let old = dir.join("old.flac");
        let middle = dir.join("middle.flac");
        let newest = dir.join("newest.flac");
        std::fs::write(&keep, vec![0u8; 4]).unwrap();
        std::fs::write(&old, vec![0u8; 6]).unwrap();
        std::fs::write(&middle, vec![0u8; 6]).unwrap();
        std::fs::write(&newest, vec![0u8; 6]).unwrap();
        set_modified(&keep, 0);
        set_modified(&old, 1);
        set_modified(&middle, 2);
        set_modified(&newest, 3);
        evict_cache_with_budget(&dir, &keep, 10);
        assert!(keep.exists());
        assert!(!old.exists());
        assert!(!middle.exists());
        assert!(newest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_file_timeout_is_25_seconds() {
        assert_eq!(OPEN_FILE_TIMEOUT, std::time::Duration::from_secs(25));
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
