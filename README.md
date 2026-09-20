# kebabify

Spicetify-like patcher for the Spotify desktop client: ad-free playback, lossless (FLAC) audio via the [lucida.to](https://lucida.to) API, and a small theme/extension bundle.

## Requirements

- Spotify desktop app (Windows)
- Chrome or Edge (only needed for `import-cookies`, to solve the lucida.to Cloudflare challenge once)

## Install & use

```powershell
kebabify.exe              # apply patches + launch Spotify (default)
kebabify.exe apply        # same thing (aliases: patch, install)
kebabify.exe status       # show patch status
kebabify.exe update-ext   # re-inject theme + extension after an edit
kebabify.exe uninstall    # restore Spotify originals (aliases: restore, remove)
```

## Unlock lossless audio (lucida.to)

lucida.to sits behind a Cloudflare challenge, so the audio proxy needs browser cookies. Either:

```powershell
kebabify.exe import-cookies            # opens Chrome on lucida.to — solve the challenge, cookies are captured automatically (alias: import)
kebabify.exe cookie "<user-agent>" "cf_clearance=...; __cf_bm=..."   # manual paste (alias: cookies)
```

Cookies are stored under `%APPDATA%\Kebabify\cookies.txt` and replayed on every lucida request. Override path with `KEBABIFY_COOKIES_PATH`. Note: that file holds live Cloudflare session cookies in plaintext — same exposure class as browser cookie jars, so don't share or commit it.

While Spotify plays, the extension redirects audio requests to a local proxy on `http://127.0.0.1:18900` (loopback only), which streams FLAC back to the player. The `KB` badge in the playbar shows the state (green = FLAC verified, amber = proxy down, grey = FLAC off — click to toggle).

## Uninstall

```powershell
kebabify.exe uninstall   # stops the proxy, restores user.css/index.html from backup
```

## Develop

```powershell
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

`cargo test --locked` and the same three checks run in CI on every push/PR.

## Troubleshooting

- **403 / "Just a moment" from lucida** → re-run `kebabify.exe import-cookies` (Cloudflare cookies expire).
- **Proxy already running** → `apply` reuses the instance on port 18900 instead of spawning a duplicate.
- **`spicetify apply` wiped the badge** → run `kebabify.exe update-ext`; kebabify registers its extension in Spicetify's config to survive applies.
- **Spotify updated itself and kebabify is gone** → re-run `kebabify.exe apply`; pristine backups are kept, so re-patching is safe.
