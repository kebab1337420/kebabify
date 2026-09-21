# kebabify

Spicetify-like patcher for the Spotify desktop client: ad-free playback, lossless (FLAC) audio via the [lucida.to](https://lucida.to) API, and a small theme/extension bundle.

**No PowerShell needed**: double-click `kebabify.exe` and pick an action from the menu (apply, supervised run, uninstall, status…).

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
kebabify.exe uninstall    # restore Spotify originals (aliases: restore, remove)
kebabify.exe import-cookies  # import Cloudflare cookies (alias: import)
```

Pin `kebabify.exe run` instead of Spotify if you want the proxy to live exactly as long as Spotify (no lingering process, fresh proxy on every start). Ctrl+C stops the proxy but leaves Spotify playing.

## Unlock lossless audio (lucida.to)

lucida.to sits behind a Cloudflare challenge, so the audio proxy needs browser cookies. Either:

```powershell
kebabify.exe import-cookies            # opens your browser on lucida.to — solve the challenge, cookies are captured automatically (alias: import)
kebabify.exe cookie "<user-agent>" "cf_clearance=...; __cf_bm=..."   # manual paste (alias: cookies)
```

`import-cookies` supports Chrome, Edge, Brave, Vivaldi, Opera, Arc (via DevTools) as well as Firefox, Zen, LibreWolf and Waterfox (via their cookie store) — first one found wins.

Cookies are stored under `%APPDATA%\Kebabify\cookies.txt` and replayed on every lucida request. Override path with `KEBABIFY_COOKIES_PATH`. Note: that file holds live Cloudflare session cookies in plaintext — same exposure class as browser cookie jars, so don't share or commit it.

While Spotify plays, the extension redirects audio requests to a local proxy on `http://127.0.0.1:18900` (loopback only). The proxy tries **lucida.to (lossless FLAC)** first, then falls back to **JioSaavn (320kbps AAC)** — matched strictly by title + artist + duration, so karaoke/covers never play instead of the real track. If both fail, the extension retries the native Spotify stream, so the music never goes silent. The `KB` badge in the playbar shows the state (green = FLAC verified, amber = proxy down, grey = FLAC off — click to toggle).

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
- **Proxy already running** → `apply` reuses the instance on port 18900 instead of spawning a duplicate.
- **`spicetify apply` wiped the badge** → run `kebabify.exe update-ext`; kebabify registers its extension in Spicetify's config to survive applies.
- **Spotify updated itself and kebabify is gone** → re-run `kebabify.exe apply`; pristine backups are kept, so re-patching is safe.
