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
    },

    /// Open a browser on lucida.to and import the Cloudflare cookies
    /// automatically — solve the challenge in the window, no copy/paste
    #[command(alias = "import")]
    ImportCookies,

    /// Apply patches, launch Spotify supervised, and stop the audio proxy
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
/// wait up to 5s for readiness. A proxy that never becomes ready is a
/// warning, not a failure — Spotify still plays natively.
async fn ensure_proxy() -> Result<()> {
    if audio_proxy::AudioProxy::is_running().await {
        println!(
            "Audio proxy already running on {}:{} — reusing it",
            audio_proxy::PROXY_HOST,
            audio_proxy::PROXY_PORT
        );
        return Ok(());
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
            "Audio proxy failed to start within 5s on {}:{} — Spotify will launch anyway",
            audio_proxy::PROXY_HOST,
            audio_proxy::PROXY_PORT
        );
    }
    Ok(())
}

/// Warns when FLAC cannot work for missing cookies (Saavn still does).
fn warn_no_cookies() {
    // Without stored Cloudflare cookies every track fails at the
    // lucida handshake (HTTP 403 → proxy 502): warn now instead
    // of letting the user discover it track by track.
    if !lucida::has_cf_clearance() {
        println!(
            "NOTE: no usable lucida.to cookies stored — FLAC unavailable until you run `kebabify import-cookies` (Saavn 320kbps fallback still works)."
        );
    }
}

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

async fn cmd_uninstall() -> Result<()> {
    println!("kebabify — removing patches...");
    stop_proxy().await;
    match patcher::SpotifyPatcher::new() {
        Ok(p) => {
            p.uninstall_patches()?;
            println!("Patches removed. Spotify restored to original.");
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn cmd_update_ext() -> Result<()> {
    println!("kebabify — updating extensions...");
    match patcher::SpotifyPatcher::new() {
        Ok(p) => {
            p.update_extensions()?;
            println!("Extensions updated.");
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn cmd_status() -> Result<()> {
    match patcher::SpotifyPatcher::new() {
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

async fn cmd_cookie(user_agent: String, cookie: String) -> Result<()> {
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
    println!("kebabify — opening Chrome to capture the lucida.to cookies...");
    browser_import::import_from_browser().await
}

async fn cmd_apply() -> Result<()> {
    let p = match patcher::SpotifyPatcher::new() {
        Ok(p) => p,
        Err(e) => {
            return Err(e.context("kebabify requires Spotify to be installed"));
        }
    };
    println!("kebabify — applying patches to Spotify client...");
    p.apply_patches()?;
    println!("Patches applied successfully!");

    ensure_proxy().await?;
    warn_no_cookies();
    launch_spotify_uri();
    Ok(())
}

async fn cmd_run() -> Result<()> {
    let p = match patcher::SpotifyPatcher::new() {
        Ok(p) => p,
        Err(e) => {
            return Err(e.context("kebabify requires Spotify to be installed"));
        }
    };
    println!("kebabify — applying patches to Spotify client...");
    p.apply_patches()?;
    println!("Patches applied successfully!");

    ensure_proxy().await?;
    warn_no_cookies();

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
    println!("Spotify exited ({}), stopping the audio proxy...", status);
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
    Quit,
}

/// Parses one menu line (first letter or full word, case-insensitive,
/// French-friendly). Pure for tests.
fn parse_menu_choice(line: &str) -> Option<MenuChoice> {
    match line.trim().to_lowercase().as_str() {
        "a" | "appliquer" | "apply" => Some(MenuChoice::Apply),
        "r" | "run" | "lancer" => Some(MenuChoice::Run),
        "d" | "desinstaller" | "désinstaller" | "uninstall" => Some(MenuChoice::Uninstall),
        "u" | "update" => Some(MenuChoice::UpdateExt),
        "s" | "statut" | "status" => Some(MenuChoice::Status),
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
        println!("  [S]tatut");
        println!("  [Q]uitter");
        print!("Choix > ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
        if n == 0 {
            break; // EOF (Ctrl+Z)
        }
        let Some(choice) = parse_menu_choice(&line) else {
            println!("Choix inconnu, réessaie (A/R/D/U/S/Q).");
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
            MenuChoice::Quit => unreachable!(),
        };
        if let Err(e) = result {
            println!("Error: {:#}", e);
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
        Some(Commands::Status) => cmd_status().await?,
        Some(Commands::Cookie { user_agent, cookie }) => {
            cmd_cookie(user_agent, cookie).await?;
        }
        Some(Commands::ImportCookies) => cmd_import_cookies().await?,
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
        assert_eq!(parse_menu_choice("s"), Some(Status));
        assert_eq!(parse_menu_choice("q"), Some(Quit));
        assert_eq!(parse_menu_choice("quit"), Some(Quit));
        assert_eq!(parse_menu_choice(""), None);
        assert_eq!(parse_menu_choice("x"), None);
        assert_eq!(parse_menu_choice("apply now please"), None);
    }
}
