// kebabify_ext.js — Extensions for Spotify client
// Intercepte les requêtes audio de Spotify et redirige vers lucida.to pour du FLAC.

(function() {
    'use strict';

    // Single-instance guard: if a previous copy of this script (or the legacy
    // kebaccify script) already initialized, remove any legacy badge and stop.
    // The legacy kebaccify inline block uses id 'kebaccify-badge' — always
    // remove it so only one badge ever shows.
    function removeLegacyBadge() {
        var legacy = document.getElementById('kebaccify-badge');
        if (legacy) legacy.remove();
    }
    if (window.__kebabifyLoaded) {
        removeLegacyBadge();
        return;
    }
    window.__kebabifyLoaded = true;
    removeLegacyBadge();

    // ===== State =====
    var flacPriority = true;
    var lastTrackId = null;
    var currentSpotifyTrackId = null;
    var playbarButton = null;
    var playbarInterval = null;
    var flacVerified = false;
    var proxyAlive = false;
    // Per-track chatter, off unless explicitly enabled in devtools:
    // window.__kebabifyDebug = true
    function debugLog() {
        if (window.__kebabifyDebug) console.log.apply(console, arguments);
    }

    // Module-level guards so observers/wrappers are installed only once.
    var fetchPatched = false;
    var xhrPatched = false;
    var globalAudioObserver = null;
    var globalAdObserver = null;

    // SVG icons
    var svgOff = '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" style="margin-right:4px"><circle cx="12" cy="12" r="10"/><line x1="12" y1="12" x2="12" y2="16"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>';
    var svgSpeaker = '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="currentColor" style="margin-right:4px"><path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02z"/></svg>';
    var svgCheck = '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="currentColor" style="margin-right:4px"><path d="M9 16.17L4.83 12l-1.42 1.41L9 19 21 7l-1.41-1.41z"/></svg>';
    var svgMuted = '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" style="margin-right:4px"><path d="M11 5L6 9H2v6h4l5 4V5z"/><line x1="23" y1="9" x2="17" y2="15"/><line x1="17" y1="9" x2="23" y2="15"/></svg>';

    // ===== Proxy Health Check =====
    function checkProxyHealth() {
        // Bound the probe: a hung (not dead) proxy must not pile up
        // unresolved health checks every 3 s.
        var controller = null;
        var timer = null;
        try {
            if (typeof AbortController !== 'undefined') {
                controller = new AbortController();
                timer = setTimeout(function() { try { controller.abort(); } catch(e) {} }, 5000);
            }
        } catch(e) { controller = null; timer = null; }
        fetch(PROXY_BASE + 'health', controller ? { signal: controller.signal } : undefined)
            .then(function(resp) {
                if (timer) { clearTimeout(timer); timer = null; }
                var alive = resp.ok;
                var healthy = alive;
                resp.json().then(function(data) {
                    if (alive && data && data.flac) healthy = true;
                    applyProxyHealth(healthy);
                }).catch(function() {
                    applyProxyHealth(alive);
                });
            })
            .catch(function() {
                if (timer) { clearTimeout(timer); timer = null; }
                // CSP or network error — never assume a leftover proxified src
                // proves the proxy is alive. Honest "down" beats a green lie.
                applyProxyHealth(false);
            });
    }

    function applyProxyHealth(alive) {
        if (alive === proxyAlive) return;
        proxyAlive = alive;
        if (!alive) {
            flacVerified = false;
        }
        console.log('[kebabify] Proxy ' + (alive ? 'alive' : 'unreachable'));
        refreshBadge();
    }

    // ===== Self-update badge (top-right) =====
    // Polls the proxy for a newer release; a click triggers the update and
    // the badge then asks for a Spotify restart (new JS loads on restart).
    var updatingNow = false;

    function checkForUpdates() {
        if (updatingNow) return;
        fetch(PROXY_BASE + 'update/check')
            .then(function(resp) {
                if (!resp.ok) return null;
                return resp.json();
            })
            .then(function(data) {
                if (data && data.update_available && data.latest) {
                    showUpdateBadge(data.latest);
                } else {
                    hideUpdateBadge();
                }
            })
            .catch(function() {});
    }

    function showUpdateBadge(latest) {
        var badge = document.getElementById('kebabify-update-badge');
        if (!badge) {
            badge = document.createElement('button');
            badge.id = 'kebabify-update-badge';
            badge.type = 'button';
            badge.onclick = function(e) {
                e.stopPropagation(); e.preventDefault();
                triggerUpdate(badge);
                return false;
            };
            (document.body || document.documentElement).appendChild(badge);
        }
        badge.textContent = '\u2193 v' + latest.replace(/^v/, '') + ' — mettre à jour';
        badge.setAttribute('data-kebabify-update', 'available');
    }

    function hideUpdateBadge() {
        var badge = document.getElementById('kebabify-update-badge');
        if (badge) badge.remove();
    }

    function triggerUpdate(badge) {
        if (updatingNow) return;
        updatingNow = true;
        badge.textContent = 'Mise à jour…';
        badge.setAttribute('data-kebabify-update', 'updating');
        badge.onclick = function(e) { e.stopPropagation(); e.preventDefault(); return false; };
        fetch(PROXY_BASE + 'update/apply', { method: 'POST' })
            .then(function(resp) { return resp.ok ? resp.json() : null; })
            .then(function(data) {
                if (data && data.status === 'updating') {
                    badge.textContent = 'Mis à jour — redémarre Spotify';
                    badge.setAttribute('data-kebabify-update', 'done');
                } else {
                    updatingNow = false;
                    hideUpdateBadge();
                    checkForUpdates();
                }
            })
            .catch(function() {
                updatingNow = false;
                hideUpdateBadge();
            });
    }

    // ===== FLAC Mode Toggle =====
    window.__kebabifyToggleFlacMode = function() {
        flacPriority = !flacPriority;
        console.log('[kebabify] FLAC priority mode:', flacPriority ? 'ON' : 'OFF');
        localStorage.setItem('kebabify-flac-priority', flacPriority ? 'on' : 'off');
        localStorage.setItem('kebaccify-flac-priority', flacPriority ? 'on' : 'off');
        refreshBadge();
        reapplyPatchesNow();
        if (!flacPriority) {
            flacVerified = false;
        }
    };

    window.__kebaccifyToggleFlacMode = window.__kebabifyToggleFlacMode;

    var saved = localStorage.getItem('kebabify-flac-priority');
    if (saved === null) saved = localStorage.getItem('kebaccify-flac-priority');
    if (saved === 'off') flacPriority = false;

    // ===== Local proxy for FLAC audio streaming =====
    // When the kebabify Rust binary runs, it starts a local HTTP proxy on
    // 127.0.0.1:18900 that proxies Spotify audio URLs through lucida.to (FLAC).
    var PROXY_HOST = '127.0.0.1';
    var PROXY_PORT = 18900;
    var PROXY_BASE = 'http://' + PROXY_HOST + ':' + PROXY_PORT + '/';
    var PROXY_AUTH = PROXY_HOST + ':' + PROXY_PORT;

    // ===== Intercept fetch + XHR for FLAC redirect to local proxy =====
    function patchAudioUrls() {
        if (!flacPriority) return;

        try {
            if (!fetchPatched) {
                fetchPatched = true;
                var originalFetch = window.fetch;
                window.fetch = function(input, init) {
                    if (!flacPriority) return originalFetch.call(this, input, init);
                    var self = this;
                    var url = typeof input === 'string' ? input : (input && input.url) || '';
                    var originalInput = input;
                    var originalUrl = url;
                    if (isSpotifyAudioRequest(url)) {
                        var redirected = tryRedirectToProxy(url);
                        if (redirected) {
                            // Preserve Request semantics (Range headers, mode,
                            // credentials): a bare string would drop them and
                            // the upstream would answer 200 instead of 206.
                            if (typeof Request !== 'undefined' && input instanceof Request) {
                                try { input = new Request(redirected, input); }
                                catch(e) { input = redirected; }
                            } else {
                                input = redirected;
                            }
                        }
                    }
                    var wasProxified = isProxifiedUrl(input);
                    return originalFetch.call(self, input, init).then(function(resp) {
                        // Proxy failure (lucida down, missing cookies…) → retry
                        // the native input once so the track still plays, in
                        // Spotify quality, instead of erroring out.
                        if (flacPriority && resp && resp.status === 502 && wasProxified && originalUrl && originalUrl !== urlString(input)) {
                            console.warn('[kebabify] Proxy 502, falling back to native audio');
                            flacVerified = false;
                            refreshBadge();
                            return originalFetch.call(self, originalInput, init);
                        }
                        return resp;
                    }, function(err) {
                        // Network-level failure on a proxified URL → same fallback.
                        if (flacPriority && wasProxified && originalUrl && originalUrl !== urlString(input)) {
                            console.warn('[kebabify] Proxy unreachable, falling back to native audio');
                            flacVerified = false;
                            refreshBadge();
                            return originalFetch.call(self, originalInput, init);
                        }
                        throw err;
                    });
                };
            }

            if (!xhrPatched) {
                xhrPatched = true;
                var originalXHROpen = XMLHttpRequest.prototype.open;
                XMLHttpRequest.prototype.open = function(method, url, ...rest) {
                    if (flacPriority && isSpotifyAudioRequest(url)) {
                        var redirected = tryRedirectToProxy(url);
                        if (redirected) url = redirected;
                    }
                    return originalXHROpen.apply(this, [method, url, ...rest]);
                };
            }
        } catch(e) {
            console.warn('[kebabify] Audio URL patching failed:', e);
        }
    }

    function isSpotifyAudioRequest(url) {
        if (!url) return false;
        return url.includes('audio-spclient') ||
               url.includes('spclient.wg.spotify.com') ||
               url.includes('streaming.spotify.com') ||
               (url.includes('spotify.com') && (url.includes('audio') || url.includes('stream')));
    }

    // Redirect Spotify audio URL to the local proxy (which fetches FLAC via lucida.to)
    function tryRedirectToProxy(url) {
        var trackId = extractTrackId(url);
        if ((!trackId || !isValidSpotifyTrackId(trackId)) &&
            typeof window.Spicetify !== 'undefined' && window.Spicetify.Player && window.Spicetify.Player.trackID) {
            trackId = window.Spicetify.Player.trackID;
            if (trackId && trackId.indexOf('spotify:track:') === 0) {
                trackId = trackId.replace('spotify:track:', '');
            }
        }
        // The audio URL carries a file UUID, not the Spotify track ID — prefer
        // the now-playing track (read from the DOM) over any UUID extracted
        // from the media URL.
        if (!isValidSpotifyTrackId(trackId)) trackId = currentSpotifyTrackId;
        if (!isValidSpotifyTrackId(trackId)) return null;

        debugLog('[kebabify] FLAC: proxying to local for track', trackId);
        return PROXY_BASE + '?track=' + trackId;
    }

    function isValidSpotifyTrackId(id) {
        return typeof id === 'string' && /^[A-Za-z0-9]{22}$/.test(id);
    }

    function urlString(input) {
        return typeof input === 'string' ? input : (input && input.url) || '';
    }

    function isProxifiedUrl(input) {
        var u = urlString(input);
        return u !== '' && u.indexOf(PROXY_AUTH) !== -1;
    }

    // Reads the current track ID from the now-playing widget of the Spotify UI.
    // Works without Spicetify: the widget contains a link like
    // spotify:track:11dF... (the audio element's URL only holds a file UUID).
    function readNowPlayingTrackId() {
        var el = document.querySelector('[data-testid="now-playing-widget"] a[href*="spotify:track:"], a[href^="spotify:track:"]');
        if (!el) return null;
        var href = el.getAttribute('href') || el.href || '';
        var m = href.match(/spotify:track:([A-Za-z0-9]{22})/);
        return m ? m[1] : null;
    }

    // Extract a *valid* Spotify track ID from a URL.
    // Same key set as the Rust proxy (`tracks/`, `track_id=`) so both agree.
    function extractTrackId(url) {
        if (!url) return null;
        return matchTrackId(url, /tracks?\/([A-Za-z0-9]{22})/)
            || matchTrackId(url, /[?&](?:track|track_id|id|cid)=([A-Za-z0-9]{22})/)
            || matchTrackId(url, /spotify:track:([A-Za-z0-9]{22})/);
    }

    // Single regex match with a right-boundary check: the 22 chars must not
    // be the prefix of a longer alnum run, otherwise a 32-hex file UUID in
    // /track/{uuid} would return its first 22 chars as a bogus track ID.
    function matchTrackId(url, re) {
        var m = url.match(re);
        if (!m || !isValidSpotifyTrackId(m[1])) return null;
        var after = url.charAt(m.index + m[0].length);
        if (after && /[A-Za-z0-9]/.test(after)) return null;
        return m[1];
    }

    // Media elements ever patched, for observer cleanup: a removed node never
    // appears in querySelectorAll, so only a stored registry can release it.
    var trackedMedia = [];

    function trackMedia(media) {
        if (trackedMedia.indexOf(media) === -1) trackedMedia.push(media);
    }

    function untrackDisconnectedMedia() {
        for (var i = trackedMedia.length - 1; i >= 0; i--) {
            var m = trackedMedia[i];
            if (!m.isConnected) {
                try { if (m._kebabifyObserver) m._kebabifyObserver.disconnect(); } catch(e) {}
                m._kebabifyObserver = null;
                m._kebabifyPatched = false;
                trackedMedia.splice(i, 1);
            }
        }
    }

    // ===== Audio element interception =====
    function interceptAudioElements() {
        if (globalAudioObserver) return;

        globalAudioObserver = new MutationObserver(function() {
            try {
                // Release observers of elements that left the DOM via the
                // registry (a removed node never shows up in querySelectorAll,
                // so scanning the live list alone can never find it).
                untrackDisconnectedMedia();
                var all = document.querySelectorAll('audio, video');
                    all.forEach(function(media) {
                        if (!media._kebabifyPatched) {
                            media._kebabifyPatched = true;
                            patchAudioElement(media);
                        }
                    });
            } catch(e) {}
        });
        if (document.body) globalAudioObserver.observe(document.body, { childList: true, subtree: true });

        setTimeout(function() {
            var all = document.querySelectorAll('audio, video');
            all.forEach(function(media) {
                if (!media._kebabifyPatched) {
                    media._kebabifyPatched = true;
                    patchAudioElement(media);
                }
            });
        }, 1000);
    }

    function patchAudioElement(media) {
        try {
            trackMedia(media);
            // Ahead of the observer: a <audio> inserted by React already
            // carries its src — no attribute mutation will follow, so the
            // current (Spotify) URL would never be redirected. Handle it now.
            if (flacPriority && media.src && isSpotifyAudioRequest(media.src)) {
                redirectMediaWithFallback(media);
            }

            // Watch for src changes and redirect Spotify audio URLs to the proxy.
            var innerObserver = new MutationObserver(function() {
                try {
                    // Element was removed from the document — release the observer
                    // and let it be re-patched if it ever comes back.
                    if (!media.isConnected) {
                        innerObserver.disconnect();
                        media._kebabifyObserver = null;
                        media._kebabifyPatched = false;
                        return;
                    }
                    if (flacPriority && media.src && isSpotifyAudioRequest(media.src)) {
                        redirectMediaWithFallback(media);
                    }
                } catch(e) {}
            });
            innerObserver.observe(media, { attributes: true, childList: true, subtree: true });
            media._kebabifyObserver = innerObserver;
        } catch(e) {}
    }

    // Redirect with a safety net: remember the native URL so a proxy failure
    // restores Spotify audio instead of leaving silence. After a fallback the
    // element is left alone for 30 s *for that track* (else error → restore →
    // redirect would hot-loop); a new track retries FLAC immediately.
    function redirectMediaWithFallback(media) {
        if (media._kebabifyFallbackAt && media._kebabifyFallbackTrack === currentSpotifyTrackId &&
            Date.now() - media._kebabifyFallbackAt < 30000) return;
        var newSrc = tryRedirectToProxy(media.src);
        if (!newSrc || newSrc === media.src) return;
        if (!media._kebabifyOriginalSrc) media._kebabifyOriginalSrc = media.src;
        attachAudioFallback(media);
        media.src = newSrc;
    }

    function attachAudioFallback(media) {
        if (media._kebabifyFallbackAttached) return;
        media._kebabifyFallbackAttached = true;
        media.addEventListener('error', function() {
            if (media._kebabifyOriginalSrc && media.src && media.src.indexOf(PROXY_AUTH) !== -1) {
                console.warn('[kebabify] Proxy media failed, falling back to native audio');
                flacVerified = false;
                refreshBadge();
                media._kebabifyFallbackAt = Date.now();
                media._kebabifyFallbackTrack = currentSpotifyTrackId;
                media.src = media._kebabifyOriginalSrc;
                media._kebabifyOriginalSrc = null;
            }
        });
    }

    // ===== Playbar Button using Spicetify =====
    function injectPlaybarButton() {
        if (playbarInterval) {
            try { clearInterval(playbarInterval); } catch(e) {}
            playbarInterval = null;
        }
        if (playbarButton) {
            try { playbarButton.deregister(); } catch(e) {}
            playbarButton = null;
        }

        if (typeof window.Spicetify === 'undefined' || !window.Spicetify.Playbar || !window.Spicetify.Playbar.Button) {
            injectFallbackBadge();
            return;
        }

        try {
            var label = flacPriority ? 'kebabify FLAC: Actif' : 'kebabify FLAC: Inactif';
            var initialIcon;
            if (!flacPriority) initialIcon = svgMuted;
            else if (flacVerified) initialIcon = svgCheck;
            else if (proxyAlive) initialIcon = svgSpeaker;
            else initialIcon = svgOff;
            var iconText = initialIcon + 'KB';

            playbarButton = new window.Spicetify.Playbar.Button(
                label,
                iconText,
                function() { window.__kebabifyToggleFlacMode(); }
            );

            if (playbarButton.element) {
                var btn = playbarButton.element;
                var span = btn.querySelector('span');

                btn.setAttribute('data-kebabify-flac', flacPriority ? 'on' : 'off');
                btn.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
                btn.style.fontSize = '11px';
                btn.style.fontWeight = '700';
                btn.style.padding = '2px 6px';
                btn.style.minWidth = '44px';
                btn.style.height = '32px';
                btn.style.display = 'inline-flex';
                btn.style.alignItems = 'center';
                btn.style.justifyContent = 'center';
            }

            playbarInterval = setInterval(function() {
                if (playbarButton && playbarButton.element) {
                    var btn = playbarButton.element;
                    var iconSpan = btn.querySelector('span');
                    if (iconSpan) {
                        var ic;
                        if (!flacPriority) ic = svgMuted;
                        else if (flacVerified) ic = svgCheck;
                        else if (proxyAlive) ic = svgSpeaker;
                        else ic = svgOff;
                        // Skip the DOM write when nothing changed: rewriting
                        // innerHTML every second churns layout for no benefit.
                        var html = ic + 'KB';
                        if (iconSpan._kebabifyHtml !== html) {
                            iconSpan._kebabifyHtml = html;
                            iconSpan.innerHTML = html;
                        }
                    }
                    btn.setAttribute('data-kebabify-flac', flacPriority ? 'on' : 'off');
                    btn.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
                }
            }, 1000);

        } catch(e) {
            console.warn('[kebabify] Playbar button failed:', e);
            playbarButton = null;
            injectFallbackBadge();
        }
    }

    // ===== Fallback Badge (clickable button) =====
    function injectFallbackBadge() {
        removeLegacyBadge();
        var existing = document.getElementById('kebabify-badge');
        if (existing) { existing.onclick = null; existing.remove(); }

        var badge = document.createElement('button');
        badge.id = 'kebabify-badge';
        badge.type = 'button';
        badge.setAttribute('data-kebabify', 'true');
        badge.setAttribute('aria-label', 'kebabify mode FLAC');
        badge.setAttribute('data-kebabify-flac', flacPriority ? 'on' : 'off');
        badge.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
        badge.setAttribute('data-kebabify-proxy', proxyAlive ? 'true' : 'false');
        updateBadgeContent(badge);

        badge.onclick = function(e) {
            e.stopPropagation(); e.preventDefault();
            window.__kebabifyToggleFlacMode();
            return false;
        };

        var playbar = findElement(['.main-nowPlayingBar', '.main-nowPlayingBar-right', '[data-testid="now-playing"]', '.g48hdQBsdIDHMBpt']);
        if (playbar && playbar.parentNode) {
            playbar.parentNode.insertBefore(badge, playbar.nextSibling);
        } else if (document.body) {
            document.body.appendChild(badge);
        }
    }

    function updateBadgeContent(el) {
        if (!el) return;
        var icon, label, title;
        if (flacPriority && proxyAlive && flacVerified) {
            icon = svgCheck;
            label = 'KB';
            title = '\u2713 FLAC \u2014 kebabify';
            el.className = 'flac-on verified';
        } else if (flacPriority && proxyAlive) {
            icon = svgSpeaker;
            label = 'KB';
            title = 'FLAC en attente de vérification';
            el.className = 'flac-on';
        } else if (flacPriority) {
            icon = svgOff;
            label = 'KB';
            title = 'FLAC mode actif \u2014 proxy indisponible';
            el.className = 'flac-on proxy-down';
        } else {
            icon = svgMuted;
            label = 'KB';
            title = 'kebabify: FLAC d\u00e9sactiv\u00e9 (cliquer pour activer)';
            el.className = 'flac-off';
        }
        el.innerHTML = '<span class="badge-icon">' + icon + '</span><span class="badge-text">' + label + '</span>';
        el.title = title;
    }

    function refreshBadge() {
        removeLegacyBadge();
        // Update playbar button
        if (playbarButton && playbarButton.element) {
            var btn = playbarButton.element;
            btn.setAttribute('data-kebabify-flac', flacPriority ? 'on' : 'off');
            btn.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
            btn.setAttribute('data-kebabify-proxy', proxyAlive ? 'true' : 'false');
            var iconSpan = btn.querySelector('span');
            if (iconSpan) {
                var icon;
                if (!flacPriority) icon = svgMuted;
                else if (flacVerified) icon = svgCheck;
                else if (proxyAlive) icon = svgSpeaker;
                else icon = svgOff;
                iconSpan.innerHTML = icon + 'KB';
            }
        }
        // Update fallback badge
        var badge = document.getElementById('kebabify-badge');
        if (badge) {
            badge.setAttribute('data-kebabify-flac', flacPriority ? 'on' : 'off');
            badge.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
            badge.setAttribute('data-kebabify-proxy', proxyAlive ? 'true' : 'false');
            updateBadgeContent(badge);
        }
        updateVerifiedIndicator();
    }

    function updateVerifiedIndicator() {
        var badge = document.getElementById('kebabify-badge');
        if (badge) {
            badge.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
        }
        if (playbarButton && playbarButton.element) {
            playbarButton.element.setAttribute('data-kebabify-verified', flacVerified ? 'true' : 'false');
        }
    }

    function findElement(selectors) {
        for (var i = 0; i < selectors.length; i++) {
            var el = document.querySelector(selectors[i]);
            if (el) return el;
        }
        return null;
    }

    function reapplyPatchesNow() {
        // Re-apply audio URL interception
        patchAudioUrls();
        if (flacPriority) {
            // Re-inject badge with new state
            if (typeof window.Spicetify !== 'undefined' && window.Spicetify.Playbar && window.Spicetify.Playbar.Button) {
                injectPlaybarButton();
            } else {
                injectFallbackBadge();
            }
            flacVerified = false;
        } else {
            flacVerified = false;
        }
    }

    // ===== Audio Monitoring =====
    function monitorPlayback() {
        setInterval(function() {
            try {
                var nowId = readNowPlayingTrackId();
                if (nowId && nowId !== currentSpotifyTrackId) {
                    currentSpotifyTrackId = nowId;
                    // New track, new verdict: track A's ✓ must not linger
                    // while track B is still buffering (native or proxy).
                    if (flacVerified) {
                        flacVerified = false;
                        updateVerifiedIndicator();
                    }
                    debugLog('[kebabify] Now playing track:', nowId);
                }
                var anyPlaying = false;
                var proxifiedPlaying = false;
                var audioElements = document.querySelectorAll('audio');
                for (var i = 0; i < audioElements.length; i++) {
                    var a = audioElements[i];
                    if (!a.paused && !a.ended && a.readyState >= 2 && a.currentTime > 0) {
                        anyPlaying = true;
                        if (a.src && a.src.indexOf(PROXY_AUTH) !== -1) proxifiedPlaying = true;
                    }
                }
                var videos = document.querySelectorAll('video');
                for (var j = 0; j < videos.length; j++) {
                    var v = videos[j];
                    if (!v.paused && !v.ended && v.readyState >= 2 && v.currentTime > 0) {
                        anyPlaying = true;
                        if (v.src && v.src.indexOf(PROXY_AUTH) !== -1) proxifiedPlaying = true;
                    }
                }
                if (!anyPlaying && typeof window.Spicetify !== 'undefined' && window.Spicetify.Player) {
                    try {
                        var s = window.Spicetify.Player;
                        var isPlaying = !!(s._state && !s._state.isPaused);
                        if (!isPlaying && typeof s.isPlaying === 'function') isPlaying = s.isPlaying();
                        if (isPlaying && s.trackID) {
                            if (s.trackID !== lastTrackId) lastTrackId = s.trackID;
                            anyPlaying = true;
                        }
                    } catch(e) {}
                }
                // Verdict: a proxified stream in flight always wins — a
                // secondary non-proxified element (preview, ad) must not
                // flash the badge off while FLAC is actually playing.
                if (proxifiedPlaying) {
                    if (flacPriority && !flacVerified) {
                        flacVerified = true;
                        updateVerifiedIndicator();
                    }
                } else if (anyPlaying && flacPriority && flacVerified) {
                    flacVerified = false;
                    updateVerifiedIndicator();
                }
                if (!anyPlaying) lastTrackId = null;
            } catch(e) {}
        }, 1000);
    }

    // ===== Other Features =====
    function blockAds() {
        if (globalAdObserver) return;

        globalAdObserver = new MutationObserver(function(mutations) {
            mutations.forEach(function(m) {
                m.addedNodes.forEach(function(node) {
                    if (node.nodeType !== 1) return;
                    var selectors = ['[data-testid="ad"]','.ad-container','.ad-showing','.sponsor','.google-ads','.adsbygoogle'];
                    selectors.forEach(function(sel) {
                        if (node.matches && node.matches(sel)) hideAd(node);
                        if (!node.querySelectorAll) return;
                        var matches = node.querySelectorAll(sel);
                        for (var i = 0; i < matches.length; i++) hideAd(matches[i]);
                    });
                });
            });
        });
        if (document.body) globalAdObserver.observe(document.body, { childList: true, subtree: true });
    }

    // Hiding is not enough: a display:none <audio>/<video> keeps playing the
    // ad with no UI. Pause any media inside ad nodes too.
    function hideAd(el) {
        try {
            el.style.display = 'none';
            var media = el.querySelectorAll ? el.querySelectorAll('audio, video') : [];
            for (var i = 0; i < media.length; i++) {
                try { media[i].pause(); } catch(e) {}
                try { media[i].muted = true; } catch(e2) {}
            }
            if (el.tagName === 'AUDIO' || el.tagName === 'VIDEO') {
                try { el.pause(); } catch(e3) {}
            }
        } catch(e) {}
    }

    // ===== Initialization =====
    function doInit() {
        console.log('[kebabify] Initializing... FLAC priority:', flacPriority ? 'ON' : 'OFF');

        var attempts = 0;
        var interval = setInterval(function() {
            attempts++;
            if (typeof window.Spicetify !== 'undefined' && window.Spicetify.Playbar && window.Spicetify.Playbar.Button) {
                clearInterval(interval);
                injectPlaybarButton();
                return;
            }
            if (attempts >= 100) {
                clearInterval(interval);
                injectFallbackBadge();
            }
        }, 250);

        setInterval(function() {
            if (!document.getElementById('kebabify-badge') && !playbarButton) {
                if (typeof window.Spicetify !== 'undefined' && window.Spicetify.Playbar && window.Spicetify.Playbar.Button) {
                    injectPlaybarButton();
                } else {
                    injectFallbackBadge();
                }
            }
        }, 2000);

        patchAudioUrls();
        interceptAudioElements();
        blockAds();
        monitorPlayback();

        // Start proxy health check every 3 seconds
        checkProxyHealth();
        setInterval(checkProxyHealth, 3000);

        // Self-update check on start, then every 30 minutes
        checkForUpdates();
        setInterval(checkForUpdates, 30 * 60 * 1000);

        var lastPath = location.pathname;
        setInterval(function() {
            if (location.pathname !== lastPath) {
                lastPath = location.pathname;
                if (typeof window.Spicetify !== 'undefined' && window.Spicetify.Playbar && window.Spicetify.Playbar.Button) {
                    injectPlaybarButton();
                } else {
                    injectFallbackBadge();
                }
                blockAds();
                patchAudioUrls();
                interceptAudioElements();
            }
        }, 1000);
    }

    var initialized = false;
    function init() {
        if (initialized) return;
        initialized = true;
        doInit();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function() { setTimeout(init, 500); });
    } else {
        setTimeout(init, 500);
    }
    var bodyPoll = setInterval(function() {
        if (document.body) {
            clearInterval(bodyPoll);
            setTimeout(function() { if (!initialized) init(); }, 500);
        }
    }, 100);
})();
