//! kebabify — Spotify client extension/patcher.
//!
//! Modifies the Spotify desktop app to be ad-free, unlocks lossless
//! audio via the lucida.to API, and enables hidden features.
//!
//! Usage:
//!   kebabify.exe              # Apply patches + launch Spotify (default)
//!   kebabify.exe apply        # Apply patches and launch
//!   kebabify.exe update-ext   # Update embedded extensions
//!   kebabify.exe uninstall    # Remove patches (restore original)
//!   kebabify.exe status       # Show patch status
//!   kebabify.exe cookie "<user-agent>" "<cf_clearance=…>"  # Unlock lucida.to
//!   kebabify.exe import-cookies  # Open Chrome, solve challenge, auto-store

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::process::Stdio;

mod audio_proxy;
mod browser_import;
mod lucida;
mod patcher;
mod saavn;

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

    /// Open Chrome on lucida.to and import the Cloudflare cookies
    /// automatically — solve the challenge in the window, no copy/paste
    #[command(alias = "import")]
    ImportCookies,

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let patcher = patcher::SpotifyPatcher::new();

    match cli.command {
        Some(Commands::Uninstall) => {
            println!("kebabify — removing patches...");
            // Stop the detached audio proxy first.
            match audio_proxy::AudioProxy::request_shutdown().await {
                Ok(_) => println!("Audio proxy stopped."),
                Err(_) => {
                    if audio_proxy::AudioProxy::is_running().await {
                        println!("Warning: could not stop the audio proxy — it may still be running on port 18900.");
                    } else {
                        println!("Audio proxy not running (nothing to stop).");
                    }
                }
            }
            match patcher {
                Ok(p) => {
                    p.uninstall_patches()?;
                    println!("Patches removed. Spotify restored to original.");
                }
                // Fail the process (exit 1) instead of swallowing: scripts
                // must be able to tell a failed uninstall from a clean one.
                Err(e) => return Err(e),
            }
        }
        Some(Commands::UpdateExt) => {
            println!("kebabify — updating extensions...");
            match patcher {
                Ok(p) => {
                    p.update_extensions()?;
                    println!("Extensions updated.");
                }
                Err(e) => return Err(e),
            }
        }
        Some(Commands::Status) => match patcher {
            Ok(p) => {
                p.print_status()?;
                print_runtime_state().await;
            }
            Err(e) => {
                // The proxy and cookies live outside the Spotify install —
                // report them anyway (e.g. proxy left running after uninstall),
                // then fail the process so scripts see the missing install.
                print_runtime_state().await;
                return Err(e);
            }
        },
        Some(Commands::Cookie { user_agent, cookie }) => {
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
        }
        Some(Commands::ImportCookies) => {
            println!("kebabify — opening Chrome to capture the lucida.to cookies...");
            browser_import::import_from_browser().await?;
        }
        Some(Commands::AudioProxyOnly) => {
            // Hidden mode: run only the audio proxy as a detached process.
            // This is spawned by the main kebabify.exe process.
            let proxy = audio_proxy::AudioProxy::new(audio_proxy::PROXY_PORT);
            proxy.start().await?;
        }
        Some(Commands::Apply) | None => {
            // Default: apply patches and launch Spotify
            match patcher {
                Ok(p) => {
                    println!("kebabify — applying patches to Spotify client...");
                    p.apply_patches()?;
                    println!("Patches applied successfully!");

                    // Probe first: if a proxy is already running (previous apply,
                    // or Spotify restarted while it stayed up) reuse it instead of
                    // spawning a duplicate that would fail the port bind.
                    let already_running = audio_proxy::AudioProxy::is_running().await;

                    if already_running {
                        println!(
                            "Audio proxy already running on {}:{} — reusing it",
                            audio_proxy::PROXY_HOST,
                            audio_proxy::PROXY_PORT
                        );
                    } else {
                        // Start the FLAC audio proxy as a DETACHED process.
                        // This is critical — if we use tokio::spawn, the proxy dies
                        // when main() returns. Instead we launch a separate kebabify.exe
                        // process with the audio-proxy-only subcommand that lives independently.
                        let exe =
                            std::env::current_exe().context("Cannot find kebabify.exe path")?;
                        let mut cmd = std::process::Command::new(&exe);
                        cmd.arg("audio-proxy-only")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());
                        #[cfg(target_os = "windows")]
                        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW — no console window
                        let _child = cmd.spawn().context("Failed to start audio proxy process")?;

                        // Wait for the proxy to be ready before proceeding.
                        let proxy_ready = audio_proxy::AudioProxy::wait_until_ready(
                            std::time::Duration::from_secs(5),
                        )
                        .await;
                        if proxy_ready {
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
                    }

                    // Without stored Cloudflare cookies every track fails at the
                    // lucida handshake (HTTP 403 → proxy 502): warn now instead
                    // of letting the user discover it track by track.
                    if !lucida::has_cf_clearance() {
                        println!(
                            "NOTE: no usable lucida.to cookies stored — FLAC unavailable until you run `kebabify import-cookies` (Saavn 320kbps fallback still works)."
                        );
                    }

                    // Launch the patched Spotify client (uses Spotify's own UI)
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
                Err(e) => {
                    // Spotify not found — fail the process (exit 1) instead of
                    // printing success-looking lines.
                    return Err(e.context("kebabify requires Spotify to be installed"));
                }
            }
        }
    }

    Ok(())
}
