//! JioSaavn fallback — 320kbps AAC when lucida.to is unreachable.
//!
//! lucida (lossless FLAC) stays the primary source; this module is the
//! safety net so a track still plays in high quality when lucida is down,
//! Cloudflare-blocked, or cookieless. No auth and no cookies anywhere here.
//!
//! Chain:
//! 1. [`track_meta`]: Spotify embed page (`__NEXT_DATA__`) → title, artist,
//!    duration. Public page, no auth.
//! 2. JioSaavn `search.getResults` → candidates (title, artists, duration,
//!    `320kbps` flag, perma-url token).
//! 3. [`score_candidate`]: exact-title + artist + duration match. Karaoke,
//!    covers and tributes are rejected — a wrong song is worse than none.
//! 4. `webapi.get` by token → `encrypted_media_url`.
//! 5. DES-ECB decrypt (key `38346591`, cf. sumitkolhe/jiosaavn-api
//!    `link.helper.ts`) → `_96` URL → swapped to `_320`.
//! 6. GET the CDN URL (Range forwarded) → streamed by the proxy like lucida.

use anyhow::{anyhow, Context, Result};

/// Metadata identifying a Spotify track for the Saavn search.
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    /// Duration in milliseconds (as reported by the Spotify embed page).
    pub duration_ms: u64,
}

/// A resolved, ready-to-download 320kbps stream from the Saavn CDN.
pub struct SaavnStream {
    /// The HTTP response for the audio download, ready to be streamed.
    pub response: reqwest::Response,
}

/// Timeout for the single-shot metadata calls (embed, search, details).
/// The final CDN download streams the whole body and stays unbounded.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Minimum score (see [`score_candidate`]) to accept a Saavn result.
const ACCEPT_SCORE: i32 = 80;

/// Duration gap (seconds) beyond which a hit is a different recording,
/// however similar the title/artist look (live, extended, radio edit).
/// Without this gate, "Hello – Adele (live, untagged)" scores 85 and plays.
const MAX_DURATION_GAP_S: u64 = 15;

/// Resolved CDN URLs cached per track ID: when lucida is down, every track
/// would otherwise pay embed + search + details round-trips again. Links
/// carry no visible expiry; 10 min TTL stays well clear of rotations.
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Track ID → (CDN URL, resolved-at). Positive hits only; misses surface so
/// catalog changes are picked up on the next try instead of going stale.
static CDN_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Cached CDN URL when fresh, else `None`. Stale entries are evicted on
/// read so the map can't fill with dead weight over a long-running proxy.
fn cached_cdn_url(track_id: &str) -> Option<String> {
    let mut guard = CDN_CACHE.lock().ok()?;
    let (url, at) = guard.get(track_id)?;
    if !cache_is_fresh(*at) {
        guard.remove(track_id);
        return None;
    }
    Some(url.clone())
}

fn cache_is_fresh(at: std::time::Instant) -> bool {
    at.elapsed() < CACHE_TTL
}

fn store_cdn_url(track_id: &str, url: String) {
    if let Ok(mut guard) = CDN_CACHE.lock() {
        // Sweep expired, then hard-cap: unbounded growth over weeks of
        // distinct tracks is a slow leak for a daemon-shaped proxy.
        guard.retain(|_, (_, at)| cache_is_fresh(*at));
        if guard.len() >= 512 {
            guard.clear();
        }
        guard.insert(track_id.to_string(), (url, std::time::Instant::now()));
    }
}

/// Markers that disqualify a result unless the query has them too.
/// Without this, "Cut To The Feeling" happily matches a karaoke cover.
const VARIANT_MARKERS: &[&str] = &[
    "instrumental",
    "karaoke",
    "cover",
    "tribute",
    "remix",
    "live",
    "slowed",
    "spedup",
    "8d",
    "nightcore",
];

/// Resolves a Spotify track ID to a streaming Saavn response.
///
/// `range` is forwarded like in [`crate::lucida`] so seeking keeps working.
pub async fn open_stream(
    client: &reqwest::Client,
    track_id: &str,
    range: Option<&str>,
    if_range: Option<&str>,
) -> Result<SaavnStream> {
    if let Some(url) = cached_cdn_url(track_id) {
        return stream_cdn(client, &url, range, if_range).await;
    }
    let meta = track_meta(client, track_id).await?;
    let query = format!("{} {}", meta.title, meta.artist);
    let candidates = search_candidates(client, &query).await?;
    let picked = candidates
        .iter()
        .map(|c| (score_candidate(&meta, c), c))
        .filter(|(s, _)| *s >= ACCEPT_SCORE)
        .max_by_key(|(s, _)| *s)
        .map(|(_, c)| c)
        .ok_or_else(|| {
            anyhow!(
                "Saavn: no good match for '{} – {}' ({} candidates)",
                meta.title,
                meta.artist,
                candidates.len()
            )
        })?;

    // Search hits already carry `encrypted_media_url` (byte-identical to the
    // details endpoint): decrypt straight away, keep `details` for hits
    // that lack it or carry a corrupt one. Saves one HTTPS round-trip per
    // uncached track.
    let cdn_url = match picked.enc_url.as_deref() {
        Some(enc) => match decrypt_media_url(enc) {
            Ok(url) => url,
            Err(_) => details_media_url(client, &picked.token).await?,
        },
        None => details_media_url(client, &picked.token).await?,
    };
    store_cdn_url(track_id, cdn_url.clone());
    stream_cdn(client, &cdn_url, range, if_range).await
}

/// GETs a resolved CDN URL (Range/If-Range forwarded, body unbounded like
/// every other download in this codebase).
async fn stream_cdn(
    client: &reqwest::Client,
    cdn_url: &str,
    range: Option<&str>,
    if_range: Option<&str>,
) -> Result<SaavnStream> {
    let mut req = client.get(cdn_url);
    if let Some(r) = range {
        req = req.header("Range", r);
    }
    if let Some(v) = if_range {
        req = req.header("If-Range", v);
    }
    let resp = req.send().await.context("Saavn: CDN download failed")?;
    if !resp.status().is_success() {
        return Err(anyhow!("Saavn: CDN returned HTTP {}", resp.status()));
    }
    Ok(SaavnStream { response: resp })
}

/// Fetches title/artist/duration from the public Spotify embed page.
async fn track_meta(client: &reqwest::Client, track_id: &str) -> Result<TrackMeta> {
    let url = format!("https://open.spotify.com/embed/track/{}", track_id);
    let html = client
        .get(&url)
        .timeout(REQUEST_TIMEOUT)
        .header("User-Agent", super::lucida::STOCK_UA)
        .send()
        .await
        .context("Spotify embed page request failed")?
        .error_for_status()
        .context("Spotify embed page returned an error")?
        .text()
        .await
        .context("Failed to read Spotify embed page")?;
    parse_embed_meta(&html).ok_or_else(|| {
        anyhow!("Could not parse Spotify embed metadata — page structure may have changed")
    })
}

/// Extracts [`TrackMeta`] from embed-page HTML. Pure for tests.
fn parse_embed_meta(html: &str) -> Option<TrackMeta> {
    let blob = next_data_blob(html)?;
    let json: serde_json::Value = serde_json::from_str(blob).ok()?;
    let entity = json.pointer("/props/pageProps/state/data/entity")?;
    let title = entity
        .get("title")
        .or_else(|| entity.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let artist = entity
        .get("artists")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let duration_ms = entity.get("duration").and_then(|v| v.as_u64())?;
    Some(TrackMeta {
        title: title.to_string(),
        artist: artist.to_string(),
        duration_ms,
    })
}

/// Extracts the `__NEXT_DATA__` JSON blob from embed-page HTML.
fn next_data_blob(html: &str) -> Option<&str> {
    let marker = "<script id=\"__NEXT_DATA__\"";
    let start = html.find(marker)?;
    let tag_end = html[start..].find('>')?;
    let content_start = start + tag_end + 1;
    let content_end = html[content_start..].find("</script>")?;
    Some(&html[content_start..content_start + content_end])
}

/// Searches JioSaavn for `query` ("title artist").
async fn search_candidates(client: &reqwest::Client, query: &str) -> Result<Vec<Candidate>> {
    let encoded = super::lucida::urlencoding::encode(query);
    let url = format!(
        "https://www.jiosaavn.com/api.php?__call=search.getResults&p=1&q={}&n_song=10&n_album=0&n_artist=0&n_playlist=0&api_version=4&_format=json&_marker=0&ctx=web6dot0",
        encoded
    );
    let json: serde_json::Value = client
        .get(&url)
        .timeout(REQUEST_TIMEOUT)
        .header("User-Agent", super::lucida::STOCK_UA)
        .send()
        .await
        .context("Saavn search request failed")?
        .error_for_status()
        .context("Saavn search returned an error")?
        .json()
        .await
        .context("Failed to parse Saavn search response")?;

    Ok(parse_search_candidates(&json))
}

/// Raw Saavn search hit used for scoring.
struct Candidate {
    token: String,
    title: String,
    artists: Vec<String>,
    duration_s: u64,
    has_320: bool,
    /// `encrypted_media_url` straight from the search hit: byte-identical to
    /// what `webapi.get` returns, so `details` is only a fallback. Pure parse.
    enc_url: Option<String>,
}

/// Parses `search.getResults` hits. Pure for tests.
fn parse_search_candidates(json: &serde_json::Value) -> Vec<Candidate> {
    let mut out = Vec::new();
    let empty = Vec::new();
    for item in json
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
    {
        let more = item.get("more_info");
        let token = item
            .get("perma_url")
            .and_then(|v| v.as_str())
            // Tokens ride as the last path segment: drop ?query/#fragment
            // first, then a trailing slash, so `details` gets a clean token.
            .and_then(|u| u.split(['?', '#']).next())
            .map(|u| u.trim_end_matches('/'))
            .and_then(|u| u.rsplit('/').next())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let Some(token) = token else { continue };
        let artists = more
            .and_then(|m| m.get("artistMap"))
            .and_then(|m| m.get("primary_artists"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name"))
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.push(Candidate {
            token,
            title: item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artists,
            duration_s: more
                .and_then(|m| m.get("duration"))
                .map(parse_saavn_duration)
                .unwrap_or(0),
            has_320: more.and_then(|m| m.get("320kbps")).and_then(|v| v.as_str()) == Some("true"),
            enc_url: more
                .and_then(|m| m.get("encrypted_media_url"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    out
}

/// Scores a Saavn hit against the Spotify metadata. Pure for tests.
fn score_candidate(meta: &TrackMeta, c: &Candidate) -> i32 {
    score_candidate_inner(meta, c, &normalize(&meta.artist))
}

/// Duration as seconds: Saavn usually sends `"206"`, but a JSON number or a
/// float string must not silently become 0 (which would forfeit 20 points).
fn parse_saavn_duration(v: &serde_json::Value) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(n) = v.as_f64() {
        return n as u64;
    }
    v.as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

fn score_candidate_inner(meta: &TrackMeta, c: &Candidate, want_artist: &str) -> i32 {
    let want_title = normalize(&meta.title);
    let got_title = normalize(&c.title);
    if want_title.is_empty() || got_title.is_empty() {
        return 0;
    }

    // Duration gate first: >15s off means a different recording (live,
    // extended, radio edit), however similar the names look.
    let want_s = meta.duration_ms / 1000;
    if want_s.abs_diff(c.duration_s) > MAX_DURATION_GAP_S {
        return 0;
    }

    let mut score = 0;

    // Title: exact ≫ contains ≫ nothing.
    if got_title == want_title {
        score += 50;
    } else if got_title.contains(&want_title) || want_title.contains(&got_title) {
        score += 25;
    } else {
        return 0;
    }

    // Artist must appear on either side.
    let artist_hit = c
        .artists
        .iter()
        .map(|a| normalize(a))
        .any(|a| !a.is_empty() && (a.contains(want_artist) || want_artist.contains(&a)));
    if !artist_hit {
        return 0;
    }
    score += 30;

    // Duration within tolerance (embed reports ms, Saavn seconds).
    let gap = want_s.abs_diff(c.duration_s);
    if gap <= 5 {
        score += 20;
    } else if gap <= 10 {
        score += 10;
    }

    if c.has_320 {
        score += 5;
    }

    // Variant penalty: a karaoke/cover/live result for a plain query.
    let query_has_marker = VARIANT_MARKERS.iter().any(|m| want_title.contains(m));
    if !query_has_marker && VARIANT_MARKERS.iter().any(|m| got_title.contains(m)) {
        score -= 40;
    }

    score
}

/// Lowercase alphanumeric only — "Cut To The Feeling" → "cuttothefeeling".
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Resolves a Saavn perma-url token to a 320kbps CDN URL.
async fn details_media_url(client: &reqwest::Client, token: &str) -> Result<String> {
    let encoded = super::lucida::urlencoding::encode(token);
    let url = format!(
        "https://www.jiosaavn.com/api.php?__call=webapi.get&token={}&type=song&api_version=4&_format=json&_marker=0&ctx=web6dot0&include_meta_tags=0",
        encoded
    );
    let json: serde_json::Value = client
        .get(&url)
        .timeout(REQUEST_TIMEOUT)
        .header("User-Agent", super::lucida::STOCK_UA)
        .send()
        .await
        .context("Saavn details request failed")?
        .error_for_status()
        .context("Saavn details returned an error")?
        .json()
        .await
        .context("Failed to parse Saavn details response")?;
    let enc = json
        .pointer("/songs/0/more_info/encrypted_media_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Saavn: no encrypted media URL in details"))?;
    decrypt_media_url(enc)
}

/// Decrypts `encrypted_media_url` and upgrades the quality marker to 320.
/// DES-ECB, key `38346591` (cf. sumitkolhe/jiosaavn-api `link.helper.ts`).
fn decrypt_media_url(enc: &str) -> Result<String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
    use des::Des;

    let mut data = STANDARD
        .decode(enc.trim())
        .context("Saavn: media URL is not valid base64")?;
    if data.len() % 8 != 0 || data.is_empty() {
        return Err(anyhow!("Saavn: malformed encrypted media URL"));
    }
    let cipher =
        Des::new_from_slice(b"38346591").map_err(|e| anyhow!("Saavn: bad DES key: {}", e))?;
    let (blocks, _) = data.as_chunks_mut::<8>();
    for chunk in blocks {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }
    // PKCS#7 unpad.
    let pad = *data.last().unwrap() as usize;
    if pad == 0
        || pad > 8
        || data.len() < pad
        || !data[data.len() - pad..].iter().all(|&b| b as usize == pad)
    {
        return Err(anyhow!("Saavn: bad PKCS#7 padding — key or data changed"));
    }
    data.truncate(data.len() - pad);
    let url = String::from_utf8(data).context("Saavn: decrypted URL is not UTF-8")?;
    Ok(upgrade_quality(&url))
}

/// Swaps a trailing low-quality marker (`…_96.mp4`) for `_320`. Trailing-only:
/// a blind `replace` would also rewrite a hash/path segment that happens to
/// contain `_96`. Without a known marker the URL is returned as-is (still
/// playable, just not 320). Pure for tests.
fn upgrade_quality(url: &str) -> String {
    match url.rfind("_96.") {
        Some(pos) => format!("{}_320.{}", &url[..pos], &url[pos + 4..]),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live-captured vector (Carly Rae Jepsen karaoke cover — the crypto math
    // is identical whatever the song).
    const ENC: &str = "ID2ieOjCrwfgWvL5sXl4B1ImC5QfbsDyYqTeQhwXaYSdItBd3yyPjx9EXyrmCoUJ5JZ80nD2NlMvQdU26Ke9GBw7tS9a8Gtq";

    fn meta() -> TrackMeta {
        TrackMeta {
            title: "Cut To The Feeling".to_string(),
            artist: "Carly Rae Jepsen".to_string(),
            duration_ms: 207959,
        }
    }

    fn cand(title: &str, artists: &[&str], duration_s: u64) -> Candidate {
        Candidate {
            token: "tok".to_string(),
            title: title.to_string(),
            artists: artists.iter().map(|s| s.to_string()).collect(),
            duration_s,
            has_320: true,
            enc_url: None,
        }
    }

    #[test]
    fn decrypts_known_vector_and_upgrades_quality() {
        let url = decrypt_media_url(ENC).unwrap();
        assert_eq!(
            url,
            "https://aac.saavncdn.com/031/2a333cdb53818e9c18a075ffe8f52e42_320.mp4"
        );
    }

    #[test]
    fn decrypt_rejects_garbage() {
        assert!(decrypt_media_url("!!!not-base64!!!").is_err());
        assert!(decrypt_media_url("aGVsbG8=").is_err()); // valid b64, bad blocks
    }

    #[test]
    fn exact_match_accepted() {
        let c = cand("Cut To The Feeling", &["Carly Rae Jepsen"], 208);
        assert!(score_candidate(&meta(), &c) >= ACCEPT_SCORE);
    }

    #[test]
    fn karaoke_cover_rejected() {
        let c = cand(
            "Cut to the Feeling (Originally Performed by Carly Rae Jepsen) [Instrumental]",
            &["Covered Up"],
            206,
        );
        assert!(score_candidate(&meta(), &c) < ACCEPT_SCORE);
    }

    #[test]
    fn wrong_artist_rejected() {
        let c = cand("Cut To The Feeling", &["Someone Else"], 208);
        assert_eq!(score_candidate(&meta(), &c), 0);
    }

    #[test]
    fn far_duration_rejected_as_different_recording() {
        // Same title+artist but 30s off (live/extended, untagged): gated.
        let c = cand("Cut To The Feeling", &["Carly Rae Jepsen"], 238);
        assert_eq!(score_candidate(&meta(), &c), 0);
    }

    #[test]
    fn near_duration_accepted_without_bonus() {
        // 12s off: no duration points, title+artist still carry it.
        let c = cand("Cut To The Feeling", &["Carly Rae Jepsen"], 220);
        assert!(score_candidate(&meta(), &c) >= ACCEPT_SCORE);
    }

    #[test]
    fn quality_swap_is_trailing_only() {
        assert_eq!(
            upgrade_quality("https://x/_96abc_96.mp4"),
            "https://x/_96abc_320.mp4"
        );
        assert_eq!(
            upgrade_quality("https://x/song_320.mp4"),
            "https://x/song_320.mp4"
        );
    }

    #[test]
    fn durations_parse_numbers_and_floats() {
        assert_eq!(parse_saavn_duration(&serde_json::json!(208)), 208);
        assert_eq!(parse_saavn_duration(&serde_json::json!(207.9)), 207);
        assert_eq!(parse_saavn_duration(&serde_json::json!("206")), 206);
        assert_eq!(parse_saavn_duration(&serde_json::json!("n/a")), 0);
        assert_eq!(parse_saavn_duration(&serde_json::Value::Null), 0);
    }

    #[test]
    fn cache_freshness() {
        assert!(cache_is_fresh(std::time::Instant::now()));
        assert!(!cache_is_fresh(
            std::time::Instant::now() - CACHE_TTL - std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn normalize_strips_case_and_punctuation() {
        assert_eq!(normalize("Cut To The Feeling!"), "cuttothefeeling");
        assert_eq!(normalize("8D Audio"), "8daudio");
    }

    #[test]
    fn embed_meta_parsed() {
        let html = r#"<html><head><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"state":{"data":{"entity":{"id":"11dFghVXANMlKmJXsNCbNl","title":"Cut To The Feeling","artists":[{"name":"Carly Rae Jepsen"}],"duration":207959}}}}},"query":{}}</script></head></html>"#;
        let m = parse_embed_meta(html).unwrap();
        assert_eq!(m.title, "Cut To The Feeling");
        assert_eq!(m.artist, "Carly Rae Jepsen");
        assert_eq!(m.duration_ms, 207959);
    }

    #[test]
    fn embed_meta_missing_is_none() {
        assert!(parse_embed_meta("<html></html>").is_none());
    }

    #[test]
    fn search_hits_parsed_with_tokens_and_media_urls() {
        // Shape live-captured from search.getResults (trimmed).
        let json = serde_json::json!({
            "total": 2,
            "results": [
                {
                    "id": "Xv4rC9HK",
                    "title": "Some Song",
                    "perma_url": "https://www.jiosaavn.com/song/some-song/ABC123xyz",
                    "more_info": {
                        "duration": "206",
                        "320kbps": "true",
                        "encrypted_media_url": "aGVsbG8td29ybGQ=",
                        "artistMap": {"primary_artists": [{"name": "Some Artist"}]},
                    },
                },
                {
                    "id": "deadbeef",
                    "title": "Trailing Slash",
                    "perma_url": "https://www.jiosaavn.com/song/trailing-slash/TOK456/?autoplay=1",
                    "more_info": {
                        "duration": 208,
                        "artistMap": {"primary_artists": [{"name": "Other"}]},
                    },
                },
                {
                    "id": "nope",
                    "title": "No Perma",
                    "more_info": {"duration": "200"},
                },
            ],
        });
        let out = parse_search_candidates(&json);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].token, "ABC123xyz");
        assert_eq!(out[0].duration_s, 206);
        assert!(out[0].has_320);
        assert_eq!(out[0].enc_url.as_deref(), Some("aGVsbG8td29ybGQ="));
        assert_eq!(out[0].artists, vec!["Some Artist".to_string()]);
        assert_eq!(out[1].token, "TOK456");
        assert_eq!(out[1].duration_s, 208);
        assert!(!out[1].has_320);
        assert_eq!(out[1].enc_url, None);
    }
}
