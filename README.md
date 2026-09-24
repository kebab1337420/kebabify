# kebabify

Spicetify-like patcher for the Spotify desktop client: ad-free playback, lossless (FLAC) audio via Soulseek P2P and the [lucida.to](https://lucida.to) API with Saavn fallback, and a small theme/extension bundle.

**No PowerShell needed**: double-click `kebabify.exe` and pick an action from the interactive menu (French).

## Requirements

- Spotify desktop app (Windows)
- A supported browser, only for `import-cookies` (Chromium: Chrome, Edge, Brave, Vivaldi, Opera, Arc — Firefox family: Firefox, Zen, LibreWolf, Waterfox)
- The external `sockseek.exe` binary for the optional Soulseek source, placed in `%APPDATA%\Kebabify\bin\` or available on `PATH`

## Install & use

```powershell
kebabify.exe              # menu interactif (double-clic, pas de PowerShell)
kebabify.exe apply        # patch + launch (aliases: patch, install)
kebabify.exe run          # apply, launch Spotify supervised, stop the proxy when Spotify exits
kebabify.exe status       # show patch status
kebabify.exe update-ext   # re-inject theme + extension after an edit
kebabify.exe update       # self-update the binary (alias: upgrade)
kebabify.exe soulseek <user> <pass>  # store Soulseek P2P credentials (alias: slsk)
kebabify.exe cookie UA COOKIES [--force]  # store Cloudflare cookies (alias: cookies)
kebabify.exe uninstall    # restore Spotify originals (aliases: restore, remove)
```

`audio-proxy-only` also exists but is internal (spawned detached by `apply`/`run`).

Pin `kebabify.exe run` instead of Spotify if you want the proxy to live exactly as long as Spotify (no lingering process, fresh proxy on every start). Ctrl+C stops the proxy but leaves Spotify playing. Exception: if Spotify was already running, `run` hands off to it and behaves like `apply` (proxy left running).

## Self-update

`kebabify.exe update` (alias: `upgrade`, menu `M`) checks GitHub releases, downloads the new exe, and stages it next to the running binary. The audio proxy replies to the update request, then a detached helper waits briefly, replaces the binary, and relaunches `apply`. Spotify itself keeps running with the old injected extension, so restart Spotify after the update to load the new extension. The same lifecycle applies from the green pill inside Spotify (checked at start, then every 30 min; French labels).

## Audio sources and cookies

Soulseek is the first source when both its credentials and the external `sockseek.exe` downloader are available. Install `sockseek.exe` in `%APPDATA%\Kebabify\bin\` (or make it available on `PATH`), then configure the login with:

```powershell
kebabify.exe soulseek <user> <pass>
```

The proxy skips Soulseek when either the binary or credentials are missing. A Soulseek track that is not already cached is downloaded before playback starts, so the first play can buffer; later plays use the local cache.

lucida.to sits behind a Cloudflare challenge, so the audio proxy needs browser cookies for that source. Either:

```powershell
kebabify.exe import-cookies            # opens your browser on lucida.to — solve the challenge, cookies are captured automatically (alias: import)
kebabify.exe cookie "<user-agent>" "cf_clearance=...; __cf_bm=..."   # manual paste (alias: cookies)
```

`import-cookies` supports Chrome, Edge, Brave, Vivaldi, Opera, Arc (via DevTools) as well as Firefox, Zen, LibreWolf and Waterfox (via their cookie store) — your default browser first, then first found.

Cookies are stored under `%APPDATA%\Kebabify\cookies.txt` and replayed on every lucida request. Override path with `KEBABIFY_COOKIES_PATH`. Note: that file holds live Cloudflare session cookies in plaintext — same exposure class as browser cookie jars, so don't share or commit it.

While Spotify plays, the extension redirects audio requests to a local proxy on `http://127.0.0.1:18900` (loopback only). The proxy tries **Soulseek P2P (lossless FLAC)** first, then **lucida.to (lossless FLAC)**, and finally **Saavn (320kbps AAC)**. The fallback sources are matched by a title + artist + duration gate that rejects karaoke/covers/tributes (heuristic, not proof). If all sources fail, the extension retries the native Spotify stream, so the music never goes silent. The `KB` badge in the playbar shows the state (green ✓ = FLAC verified, green speaker = proxy armed, amber = proxy down, grey = FLAC off — click to toggle; checked live, not cached).

## Uninstall

```powershell
kebabify.exe uninstall   # stops the proxy, restores originals, deletes backup dirs + proxy log (clean-reinstall ready; cookies are kept)
```

## Develop

```powershell
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

`cargo test --locked` and the same three checks run in CI on every push/PR.
The Firefox cookie import is behind the `firefox-import` feature (bundled
SQLite, on by default): `cargo test --no-default-features --locked` proves
the lean build compiles too. Windows binaries embed `icons/icon.ico` at
build time (winres).

## Shutdown token

`/shutdown` on the audio proxy needs a per-boot token (an Origin header is
forgeable by any local process). The proxy mints `%APPDATA%\Kebabify\shutdown.token`
on start; the CLI attaches it automatically, so nothing changes for the
user. The token is deleted on `uninstall`.

## Troubleshooting

- **403 / "Just a moment" from lucida** → re-run `kebabify.exe import-cookies` (Cloudflare cookies expire). `status` shows `partial` if the stored cookies lack `cf_clearance` — same fix.
- **Proxy already running** → `apply` reuses the instance on port 18900 when versions match, else restarts it (stale/foreign proxies are replaced, with an 8 s drain wait).
- **`spicetify apply` wiped the badge** → run `kebabify.exe update-ext`; kebabify registers its extension in Spicetify's config to survive applies.
- **Spotify updated itself and kebabify is gone** → re-run `kebabify.exe apply`; pristine backups are kept, so re-patching is safe.
