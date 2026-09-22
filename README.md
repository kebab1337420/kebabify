# kebabify

Spicetify-like patcher for the Spotify desktop client: ad-free playback, lossless (FLAC) audio via the [lucida.to](https://lucida.to) API with JioSaavn fallback, and a small theme/extension bundle.

**No PowerShell needed**: double-click `kebabify.exe` and pick an action from the interactive menu (French).

## Requirements

- Spotify desktop app (Windows)
- A supported browser, only for `import-cookies` (Chromium: Chrome, Edge, Brave, Vivaldi, Opera, Arc — Firefox family: Firefox, Zen, LibreWolf, Waterfox)

## Install & use

```powershell
kebabify.exe              # menu interactif (double-clic, pas de PowerShell)
kebabify.exe apply        # patch + launch (aliases: patch, install)
kebabify.exe run          # apply, launch Spotify supervised, stop the proxy when Spotify exits
kebabify.exe status       # show patch status
kebabify.exe update-ext   # re-inject theme + extension after an edit
kebabify.exe update       # self-update the binary (alias: upgrade)
kebabify.exe cookie UA COOKIES [--force]  # store Cloudflare cookies (alias: cookies)
kebabify.exe uninstall    # restore Spotify originals (aliases: restore, remove)
```

`audio-proxy-only` also exists but is internal (spawned detached by `apply`/`run`).

Pin `kebabify.exe run` instead of Spotify if you want the proxy to live exactly as long as Spotify (no lingering process, fresh proxy on every start). Ctrl+C stops the proxy but leaves Spotify playing. Exception: if Spotify was already running, `run` hands off to it and behaves like `apply` (proxy left running).

## Self-update

`kebabify.exe update` (alias: `upgrade`, menu `M`) checks GitHub releases, downloads the new exe, swaps it and re-applies. Inside Spotify, a green pill appears top-right when an update is out (checked at start, then every 30 min; French labels) — one click updates, then restart Spotify to load the new extension.

## Unlock lossless audio (lucida.to)

lucida.to sits behind a Cloudflare challenge, so the audio proxy needs browser cookies. Either:

```powershell
kebabify.exe import-cookies            # opens your browser on lucida.to — solve the challenge, cookies are captured automatically (alias: import)
kebabify.exe cookie "<user-agent>" "cf_clearance=...; __cf_bm=..."   # manual paste (alias: cookies)
```

`import-cookies` supports Chrome, Edge, Brave, Vivaldi, Opera, Arc (via DevTools) as well as Firefox, Zen, LibreWolf and Waterfox (via their cookie store) — your default browser first, then first found.

Cookies are stored under `%APPDATA%\Kebabify\cookies.txt` and replayed on every lucida request. Override path with `KEBABIFY_COOKIES_PATH`. Note: that file holds live Cloudflare session cookies in plaintext — same exposure class as browser cookie jars, so don't share or commit it.

While Spotify plays, the extension redirects audio requests to a local proxy on `http://127.0.0.1:18900` (loopback only). The proxy tries **lucida.to (lossless FLAC)** first, then falls back to **JioSaavn (320kbps AAC)** — matched by a title + artist + duration gate that rejects karaoke/covers/tributes (heuristic, not proof). If both fail, the extension retries the native Spotify stream, so the music never goes silent. The `KB` badge in the playbar shows the state (green ✓ = FLAC verified, green speaker = proxy armed, amber = proxy down, grey = FLAC off — click to toggle; checked live, not cached).

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

## Troubleshooting

- **403 / "Just a moment" from lucida** → re-run `kebabify.exe import-cookies` (Cloudflare cookies expire). `status` shows `partial` if the stored cookies lack `cf_clearance` — same fix.
- **Proxy already running** → `apply` reuses the instance on port 18900 when versions match, else restarts it (stale/foreign proxies are replaced, with an 8 s drain wait).
- **`spicetify apply` wiped the badge** → run `kebabify.exe update-ext`; kebabify registers its extension in Spicetify's config to survive applies.
- **Spotify updated itself and kebabify is gone** → re-run `kebabify.exe apply`; pristine backups are kept, so re-patching is safe.
