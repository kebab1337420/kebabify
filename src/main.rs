//! kebabify — Spotify client extension/patcher.
//!
//! Modifies the Spotify desktop app to be ad-free, unlocks lossless
//! audio via the lucida.to API, and enables hidden features.
//!
//! Usage:
//!   kebabify.exe              # Apply patches + launch Spotify (default)
//!   kebabify.exe apply        # Apply patches and launch
//!   kebabify.exe run          # Apply, launch Spotify supervised, stop the proxy on exit
//!   kebabify.exe update-ext   # Update embedded extensions
//!   kebabify.exe uninstall    # Remove patches (restore original)
//!   kebabify.exe status       # Show patch status
//!   kebabify.exe cookie `<user-agent>` `"<cf_clearance=…>"`  # Unlock lucida.to
//!   kebabify.exe import-cookies  # Open Chrome, solve challenge, auto-store
//!   kebabify.exe soulseek `<user>` `<pass>`  # Soulseek P2P login (source #1)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::process::Stdio;

mod audio_proxy;
mod browser_import;
mod lucida;
mod patcher;
mod saavn;
mod soulseek;
mod updater;

#[cfg(test)]
mod mock;

#[derive(Parser)]
#[command(name = "kebabify", version, about)]
#[command(long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Apply patches to Spotify and launch it
    #[command(alias = "patch", alias = "install")]
    Apply,

    /// Remove patches and restore original Spotify
    #[command(alias = "restore", alias = "remove")]
    Uninstall,

    /// Update embedded extensions (CSS themes, JS plugins)
    UpdateExt,

    /// Check GitHub releases and self-update to the newest kebabify
    #[command(alias = "upgrade")]
    Update,

    /// Show detailed status
    Status,

    /// Store the Cloudflare cookies that unlock lucida.to (solve the challenge
    /// in a browser, then paste the Cookie header value here)
    #[command(alias = "cookies")]
    Cookie {
        /// User-Agent of the browser that solved the challenge
        user_agent: String,
        /// Cookie header value, e.g. "cf_clearance=…; __cf_bm=…"
        cookie: String,
        /// Overwrite even without cf_clearance (normally refused to protect
        /// a working session from a bad paste)
        #[arg(long)]
        force: bool,
    },

    /// Open a browser on lucida.to and import the Cloudflare cookies
    /// automatically — solve the challenge in the window, no copy/paste
    #[command(alias = "import")]
    ImportCookies,

    /// Store the Soulseek P2P login (priority #1 FLAC source).
    /// Written to %APPDATA%\Kebabify\soulseek.txt — write-only, never shown.
    #[command(alias = "slsk")]
    Soulseek {
        /// Soulseek username
        user: String,
        /// Soulseek password
        pass: String,
    },    /// Apply patches, launch Spotify supervised, and stop the audio proxy
    /// when Spotify exits — the proxy lives exactly as long as Spotify.
    /// Use this (e.g. pinned instead of Spotify) so every Spotify start gets
    /// a proxy and no proxy lingers afterwards.
    Run,

    /// Hidden: run only the audio proxy as a detached process
    #[command(hide = true)]
    AudioProxyOnly,
}

/// Proxy + cookie state for `status` (and the no-Spotify branch): independent
/// of the install, and the part that decides whether FLAC can work.
async fn print_runtime_state() {
    if audio_proxy::AudioProxy::is_running().await {
        println!("Audio proxy:           running");
    } else {
        println!("Audio proxy:           stopped");
    }
    // A session without cf_clearance 403s just the same.
    match (lucida::has_session(), lucida::has_cf_clearance()) {
        (false, _) => println!("Cookies stored:        no — run `kebabify import-cookies`"),
        (true, true) => println!("Cookies stored:        yes"),
        (true, false) => println!(
            "Cookies stored:        partial (no cf_clearance) — re-run `kebabify import-cookies`"
        ),
    }
    // Soulseek is source #1: without a login every track skips it.
    if soulseek::has_credentials() {
        println!("Soulseek login:        yes (priority #1 source)");
    } else {
        println!("Soulseek login:        no — run `kebabify soulseek <user> <pass>`");
    }
    match soulseek::binary_path() {
        Some(p) if p.is_file() => println!("Soulseek binary:       yes"),
        _ => println!("Soulseek binary:       missing — put sockseek.exe in %APPDATA%\\Kebabify\\bin\\"),
    }
}

/// stdio for the detached proxy: append to a log file next to the cookies
/// instead of the void. Without this, panics and errors in the detached
/// process vanish without a trace.
fn proxy_stdio() -> (Stdio, Stdio) {
    let fallback = || (Stdio::null(), Stdio::null());
    let Some(dir) = lucida::cookies_file_path()
        .parent()
        .map(|p| p.to_path_buf())
    else {
        return fallback();
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("proxy.log");
    // Cheap rotation: start fresh past 2 MB.
    if let Ok(m) = std::fs::metadata(&path) {
        if m.len() > 2 * 1024 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => match f.try_clone() {
            Ok(out) => (Stdio::from(out), Stdio::from(f)),
            Err(_) => fallback(),
        },
        Err(_) => fallback(),
    }
}

/// Stops a running proxy instance, tolerantly (already-stopped is fine).
async fn stop_proxy() {
    match audio_proxy::AudioProxy::request_shutdown().await {
        Ok(_) => println!("Audio proxy stopped."),
        Err(_) => {
            if audio_proxy::AudioProxy::is_running().await {
                println!(
                    "Warning: could not stop the audio proxy — it may still be running on port {}.",
                    audio_proxy::PROXY_PORT
                );
            } else {
                println!("Audio proxy not running (nothing to stop).");
            }
        }
    }
}

/// Ensures a proxy instance: reuse a running one, else spawn detached and
/// wait up to 5s for readiness. A running proxy from an older release is
/// restarted instead of reused forever (its fixes would never take effect).
/// A proxy that never becomes ready is a warning, not a failure — Spotify
/// still plays natively.
async fn ensure_proxy() -> Result<()> {
    if audio_proxy::AudioProxy::is_running().await {
        match audio_proxy::AudioProxy::proxy_version().await {
            Some(v) if v == env!("CARGO_PKG_VERSION") => {
                println!(
                    "Audio proxy already running on {}:{} — reusing it",
                    audio_proxy::PROXY_HOST,
                    audio_proxy::PROXY_PORT
                );
                return Ok(());
            }
            other => {
                println!(
                    "Running proxy is stale or foreign (version {:?}) — restarting it...",
                    other
                );
                stop_proxy().await;
                // The old instance drains (up to 5s) while still holding the
                // port: wait for it to actually die before respawning, or the
                // new child fails its bind and we "reuse" a corpse.
                if !audio_proxy::AudioProxy::wait_until_stopped(std::time::Duration::from_secs(8))
                    .await
                {
                    println!("Old proxy still alive — spawning anyway (bind may fail).");
                }
            }
        }
    }
    // Start the FLAC audio proxy as a DETACHED process.
    // This is critical — if we use tokio::spawn, the proxy dies
    // when main() returns. Instead we launch a separate kebabify.exe
    // process with the audio-proxy-only subcommand that lives independently.
    let exe = std::env::current_exe().context("Cannot find kebabify.exe path")?;
    let mut cmd = std::process::Command::new(&exe);
    let (child_out, child_err) = proxy_stdio();
    cmd.arg("audio-proxy-only")
        .stdout(child_out)
        .stderr(child_err);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW — no console window
    let _child = cmd.spawn().context("Failed to start audio proxy process")?;

    // Wait for the proxy to be ready before proceeding.
    if audio_proxy::AudioProxy::wait_until_ready(std::time::Duration::from_secs(5)).await {
        println!(
            "Audio proxy started on {}:{} (FLAC mode, detached)",
            audio_proxy::PROXY_HOST,
            audio_proxy::PROXY_PORT
        );
    } else {
        println!(
            "Audio proxy failed to start within 5s on {}:{} — port may be busy (details in %APPDATA%\\Kebabify\\proxy.log) — Spotify will launch anyway",
            audio_proxy::PROXY_HOST,
            audio_proxy::PROXY_PORT
        );
    }
    Ok(())
}

/// Warns when FLAC cannot work for missing cookies (Saavn still does).
fn warn_no_cookies() {    // Without stored Cloudflare cookies every track fails at the
    // lucida handshake (HTTP 403 → proxy 502): warn now instead
    // of letting the user discover it track by track.
    if !lucida::has_cf_clearance() {
        println!(
            "NOTE: no usable lucida.to cookies stored — FLAC unavailable until you run `kebabify import-cookies` (Saavn 320kbps fallback still works)."
        );
    }
}

/// Warns when the priority #1 source has no login (the chain still works via
/// lucida/saavn, but every track skips Soulseek).
fn warn_no_soulseek() {
    if !soulseek::has_credentials() {
        println!(
            "NOTE: no Soulseek login stored — priority #1 source skipped until you run `kebabify soulseek <user> <pass>`."
        );
    }
}

/// Kills a running Spotify so the freshly patched files actually load.
/// Spotify is single-instance: without this, `launch_spotify_uri()` only
/// wakes the old window and the new JS/CSS never take effect (silent stale
/// client — the exact confusion this fixes). Windows-only; best effort.
#[cfg(target_os = "windows")]
fn restart_spotify_for_patch() {
    let killed = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "Spotify.exe"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if killed {
        println!("Closed the running Spotify so the new patch loads on restart.");
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
}

#[cfg(not(target_os = "windows"))]
fn restart_spotify_for_patch() {}

/// Launches Spotify via its `spotify:` URI (no process handle — fire and
/// forget, used when the exe path is unknown or supervision isn't wanted).
fn launch_spotify_uri() {
    println!("Launching Spotify...");
    match open::that("spotify:") {
        Ok(_) => println!("Spotify launched."),
        Err(e) => {
            println!(
                "Failed to launch Spotify: {}. Please launch it manually.",
                e
            );
        }
    }
}

/// A supervised Spotify launch is a handoff (not a cold start) when our
/// child exits almost immediately: Spotify single-instance wakes the running
/// window and the new process quits within ~a second.
fn was_handoff(elapsed: std::time::Duration) -> bool {
    elapsed < std::time::Duration::from_secs(3)
}

/// Resolves the patcher, pointing at the Spotify download when the install
/// is missing. Single place so every command guides the same way.
fn require_patcher() -> Result<patcher::SpotifyPatcher> {
    patcher::SpotifyPatcher::new()
        .context("kebabify requires Spotify to be installed (https://www.spotify.com/download)")
}

async fn cmd_uninstall() -> Result<()> {
    println!("kebabify — removing patches...");
    stop_proxy().await;
    match require_patcher() {
        Ok(p) => {
            p.uninstall_patches()?;
            println!("Patches removed. Spotify restored to original.");
        }
        Err(e) => return Err(e),
    }
    // The debug log is ours, not the user's: remove it for a clean
    // reinstall. Cookies are kept — they cost a challenge to obtain.
    if let Some(dir) = lucida::cookies_file_path().parent() {
        let log = dir.join("proxy.log");
        if log.exists() {
            let _ = std::fs::remove_file(&log);
            eprintln!("  Removed: proxy log");
        }
    }
    // A failed update may leave a staged kebabify.exe.new behind.
    if let Ok(exe) = std::env::current_exe() {
        let staged = updater::staged_path(&exe);
        if staged.exists() {
            let _ = std::fs::remove_file(&staged);
            eprintln!("  Removed: staged update");
        }
    }
    Ok(())
}

async fn cmd_update_ext() -> Result<()> {
    println!("kebabify — updating extensions...");
    match require_patcher() {
        Ok(p) => {
            p.update_extensions()?;
            println!("Extensions updated.");
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn cmd_status() -> Result<()> {
    match require_patcher() {
        Ok(p) => {
            p.print_status()?;
            print_runtime_state().await;
        }
        Err(e) => {
            // The proxy and cookies live outside the Spotify install —
            // report them anyway, then fail so scripts see it.
            print_runtime_state().await;
            return Err(e);
        }
    }
    Ok(())
}

async fn cmd_cookie(user_agent: String, cookie: String, force: bool) -> Result<()> {
    // Refuse to overwrite a working session with garbage: a paste without
    // cf_clearance "succeeds" but still 403s on every track.
    if !force && !lucida::cookie_has_clearance(&cookie) && lucida::has_cf_clearance() {
        return Err(anyhow::anyhow!(
            "no cf_clearance in there — refusing to overwrite the working session (re-run with --force to override)"
        ));
    }
    lucida::save_cookies(&user_agent, &cookie)?;
    println!("kebabify — Cloudflare cookies stored.");
    println!("The audio proxy will now use them on all lucida.to requests.");
    // A paste without cf_clearance "succeeds" but still 403s — say so
    // now instead of letting every track fail silently.
    if !lucida::cookie_has_clearance(&cookie) {
        println!(
            "WARNING: no cf_clearance in there — the challenge was probably not solved; expect 403s until you re-run `kebabify import-cookies`."
        );
    }
    Ok(())
}

async fn cmd_import_cookies() -> Result<()> {
    println!("kebabify — opening a browser to capture the lucida.to cookies...");
    browser_import::import_from_browser().await
}

/// Stores the Soulseek login. The password is write-only: confirmed by path,
/// never echoed back.
async fn cmd_soulseek(user: String, pass: String) -> Result<()> {
    if user.trim().is_empty() || pass.is_empty() {
        return Err(anyhow::anyhow!("Soulseek username and password must not be empty"));
    }
    let path = soulseek::save_credentials(&user, &pass)?;
    println!("kebabify — Soulseek login stored ({}).", path.display());
    println!("The audio proxy will use it as the priority #1 FLAC source.");
    Ok(())
}

/// Self-update: check, download, stage, hand over to the swap helper, exit.
/// The helper relaunches `apply`, so the new version re-patches immediately.
async fn cmd_update() -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("kebabify/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("Failed to build HTTP client")?;
    println!(
        "kebabify {} — checking for updates...",
        updater::current_version()
    );
    let Some(info) = updater::check_update(&client).await? else {
        println!("Already up to date.");
        return Ok(());
    };
    println!("Found v{} — downloading...", info.version);
    let exe = std::env::current_exe().context("Cannot find kebabify.exe path")?;
    let staged = updater::staged_path(&exe);
    updater::download_release(&client, &info, &staged).await?;
    println!("Downloaded. Swapping binaries — kebabify will restart itself.");
    updater::stage_and_relaunch(&exe, &staged)?;
    // The helper takes it from here; die now so the exe can be replaced.
    std::process::exit(0);
}

/// Manual cookie entry for the interactive menu: prompts for both lines.
/// Offers to force through a clearance-less paste (the CLI `--force`).
async fn cmd_cookie_prompt() -> Result<()> {
    use std::io::{BufRead, Write};
    print!("User-Agent du navigateur : ");
    std::io::stdout().flush().ok();
    let mut ua = String::new();
    if std::io::stdin().lock().read_line(&mut ua).unwrap_or(0) == 0 {
        return Ok(());
    }
    print!("Valeur du header Cookie : ");
    std::io::stdout().flush().ok();
    let mut cookie = String::new();
    if std::io::stdin().lock().read_line(&mut cookie).unwrap_or(0) == 0 {
        return Ok(());
    }
    let ua = ua.trim().to_string();
    let cookie = cookie.trim().to_string();
    if !lucida::cookie_has_clearance(&cookie) {
        print!("Pas de cf_clearance — écraser quand même ? [o/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer).ok();
        if !matches!(
            answer.trim().to_lowercase().as_str(),
            "o" | "oui" | "y" | "yes"
        ) {
            println!("Abandonné — session existante conservée.");
            return Ok(());
        }
        return cmd_cookie(ua, cookie, true).await;
    }
    cmd_cookie(ua, cookie, false).await
}

async fn cmd_apply() -> Result<()> {
    let p = require_patcher()?;
    println!("kebabify — applying patches to Spotify client...");
    p.apply_patches()?;
    println!("Patches applied successfully!");

    ensure_proxy().await?;
    warn_no_soulseek();
    warn_no_cookies();
    restart_spotify_for_patch();
    launch_spotify_uri();
    Ok(())
}

async fn cmd_run() -> Result<()> {
    let p = require_patcher()?;
    println!("kebabify — applying patches to Spotify client...");
    p.apply_patches()?;
    println!("Patches applied successfully!");

    ensure_proxy().await?;
    warn_no_soulseek();
    warn_no_cookies();

    // Supervised mode needs a real child, not a single-instance handoff to
    // an already-running Spotify (which would exit at once and kill the proxy).
    restart_spotify_for_patch();

    let exe = match patcher::spotify_exe_path() {
        Ok(exe) => exe,
        Err(e) => {
            println!("{} — launching Spotify without supervision instead.", e);
            launch_spotify_uri();
            return Ok(());
        }
    };

    // Supervised launch: the proxy lives exactly as long as Spotify.
    // A blocking wait would stall the runtime, so wait on a thread.
    println!("Launching Spotify supervised (proxy stops on exit)...");
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(&exe)
        .spawn()
        .context("Failed to launch Spotify")?;
    let waiter = tokio::task::spawn_blocking(move || child.wait());
    let status = tokio::select! {
        joined = waiter => joined
            .context("Spotify wait task failed")?
            .context("Spotify process failed")?,
        _ = tokio::signal::ctrl_c() => {
            // Never kill the music on Ctrl+C: stop the proxy, leave
            // Spotify to the user, and exit.
            println!("Interrupted — stopping the audio proxy, leaving Spotify running.");
            stop_proxy().await;
            return Ok(());
        }
    };

    if was_handoff(start.elapsed()) {
        // Spotify was already running: our process handed off to it
        // instantly. Nothing supervised — leave the proxy (as apply).
        println!("Spotify was already running — proxy left running (as with apply).");
        return Ok(());
    }
    // A crash (non-zero) must be visible to scripts: surface the code
    // instead of exiting 0 like a clean quit.
    if !status.success() {
        stop_proxy().await;
        return Err(anyhow::anyhow!("Spotify exited with {}", status));
    }
    println!("Spotify exited, stopping the audio proxy (re-run `run` next launch)...");
    stop_proxy().await;
    Ok(())
}

/// Choice offered by the interactive menu (double-click, no PowerShell).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum MenuChoice {
    Apply,
    Run,
    Uninstall,
    Status,
    UpdateExt,
    Update,
    ImportCookies,
    CookieManual,
    Quit,
}

/// Parses one menu line (first letter or full word, case-insensitive,
/// French-friendly). Pure for tests.
fn parse_menu_choice(line: &str) -> Option<MenuChoice> {
    match line.trim().to_lowercase().as_str() {
        "a" | "appliquer" | "apply" => Some(MenuChoice::Apply),
        "r" | "run" | "lancer" => Some(MenuChoice::Run),
        "d" | "desinstaller" | "désinstaller" | "uninstall" => Some(MenuChoice::Uninstall),
        "u" | "update-ext" => Some(MenuChoice::UpdateExt),
        "m" | "mettre" | "maj" | "update" | "upgrade" => Some(MenuChoice::Update),
        "s" | "statut" | "status" => Some(MenuChoice::Status),
        "i" | "importer" | "import" | "import-cookies" => Some(MenuChoice::ImportCookies),
        "c" | "cookie" | "manuel" => Some(MenuChoice::CookieManual),
        "q" | "quitter" | "quit" | "exit" => Some(MenuChoice::Quit),
        _ => None,
    }
}

/// Double-click menu: no PowerShell needed. Loops until Quit/EOF; a failed
/// action prints its error and returns to the menu.
async fn interactive_menu() -> Result<()> {
    use std::io::{BufRead, Write};
    println!("kebabify — pas besoin de PowerShell, choisis une action :");
    loop {
        println!();
        println!("  [A]ppliquer le patch (+ lancer Spotify)");
        println!("  [R]un supervisé (proxy lié à Spotify)");
        println!("  [D]ésinstaller (restaurer Spotify)");
        println!("  [U]pdate extensions");
        println!("  [M]ettre à jour kebabify");
        println!("  [S]tatut");
        println!("  [I]mporter les cookies (Cloudflare)");
        println!("  [C]ookie manuel (coller UA + cookies)");
        println!("  [Q]uitter");
        print!("Choix > ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
        if n == 0 {
            break; // EOF (Ctrl+Z)
        }
        let Some(choice) = parse_menu_choice(&line) else {
            println!("Choix inconnu, réessaie (A/R/D/U/M/S/I/C/Q).");
            continue;
        };
        if choice == MenuChoice::Quit {
            break;
        }
        let result = match choice {
            MenuChoice::Apply => cmd_apply().await,
            MenuChoice::Run => cmd_run().await,
            MenuChoice::Uninstall => cmd_uninstall().await,
            MenuChoice::Status => cmd_status().await,
            MenuChoice::UpdateExt => cmd_update_ext().await,
            MenuChoice::Update => cmd_update().await,
            MenuChoice::ImportCookies => cmd_import_cookies().await,
            MenuChoice::CookieManual => cmd_cookie_prompt().await,
            MenuChoice::Quit => unreachable!(),
        };
        if let Err(e) = result {
            println!("Erreur : {:#}", e);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // Double-click (no args) on a terminal console → interactive menu, no
        // PowerShell needed. Piped/scripted with no args → legacy default.
        None if std::io::stdin().is_terminal() => interactive_menu().await?,
        None => cmd_apply().await?,
        Some(Commands::Apply) => cmd_apply().await?,
        Some(Commands::Run) => cmd_run().await?,
        Some(Commands::Uninstall) => cmd_uninstall().await?,
        Some(Commands::UpdateExt) => cmd_update_ext().await?,
        Some(Commands::Update) => cmd_update().await?,
        Some(Commands::Status) => cmd_status().await?,
        Some(Commands::Cookie {
            user_agent,
            cookie,
            force,
        }) => {
            cmd_cookie(user_agent, cookie, force).await?;
        }
        Some(Commands::ImportCookies) => cmd_import_cookies().await?,
        Some(Commands::Soulseek { user, pass }) => {
            cmd_soulseek(user, pass).await?;
        }
        Some(Commands::AudioProxyOnly) => {
            // Hidden mode: run only the audio proxy as a detached process.
            // This is spawned by the main kebabify.exe process.
            let proxy = audio_proxy::AudioProxy::new(audio_proxy::PROXY_PORT);
            proxy.start().await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_means_instant_exit() {
        assert!(was_handoff(std::time::Duration::from_secs(1)));
        assert!(was_handoff(std::time::Duration::from_millis(2500)));
        assert!(!was_handoff(std::time::Duration::from_secs(3)));
        assert!(!was_handoff(std::time::Duration::from_secs(120)));
    }

    #[test]
    fn menu_choices_parsed() {
        use MenuChoice::*;
        assert_eq!(parse_menu_choice("a"), Some(Apply));
        assert_eq!(parse_menu_choice("A\n"), Some(Apply));
        assert_eq!(parse_menu_choice("appliquer"), Some(Apply));
        assert_eq!(parse_menu_choice("r"), Some(Run));
        assert_eq!(parse_menu_choice("d"), Some(Uninstall));
        assert_eq!(parse_menu_choice("DÉSINSTALLER"), Some(Uninstall));
        assert_eq!(parse_menu_choice("u"), Some(UpdateExt));
        assert_eq!(parse_menu_choice("m"), Some(Update));
        assert_eq!(parse_menu_choice("update"), Some(Update));
        assert_eq!(parse_menu_choice("s"), Some(Status));
        assert_eq!(parse_menu_choice("i"), Some(ImportCookies));
        assert_eq!(parse_menu_choice("c"), Some(CookieManual));
        assert_eq!(parse_menu_choice("q"), Some(Quit));
        assert_eq!(parse_menu_choice("quit"), Some(Quit));
        assert_eq!(parse_menu_choice(""), None);
        assert_eq!(parse_menu_choice("x"), None);
        assert_eq!(parse_menu_choice("apply now please"), None);
    }
}
