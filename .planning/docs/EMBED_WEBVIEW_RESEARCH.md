# Embed & Webview Research

> Research conducted 2026-03-27. Covers link preview generation, platform oEmbed APIs,
> embeddable browser engines, and render-to-texture approaches for inline web content.

## 1. Platform oEmbed / Metadata API Testing

Each platform was tested from a client-side Rust app (no server proxy, no auth tokens).

### oEmbed APIs (structured JSON)

| Platform | Endpoint | Title | Thumbnail | Author | Status |
|----------|----------|:-----:|:---------:|:------:|--------|
| YouTube | `youtube.com/oembed?url=...&format=json` | Yes | Yes (`i.ytimg.com/vi/{id}/hqdefault.jpg`) | Yes | **Full support** |
| TikTok | `tiktok.com/oembed?url=...` | Yes | Yes (signed CDN, expires) | Yes | **Full support** |
| Spotify | `open.spotify.com/oembed?url=...` | Yes | Yes (`spotifycdn.com` album art) | No | **Full support** — also returns `iframe_url` |
| Twitter/X | `publish.twitter.com/oembed?url=...` | No | **No thumbnail** | Yes | **Partial** — returns HTML blockquote + author name only |
| Instagram | `api.instagram.com/oembed?url=...` | — | — | — | **Dead** — HTTP 500, requires Facebook Graph API token since 2020 |

### OpenGraph Scraping (parse HTML `<head>`)

| Site | og:title | og:image | og:description | Status |
|------|:--------:|:--------:|:--------------:|--------|
| GitHub | Yes | Yes (auto-generated) | Yes | **Works** |
| Wikipedia | Yes | Yes | Yes | **Works** |
| Most blogs/news | Yes | Yes | Yes | **Works** |
| Twitter/X | — | — | — | **Blocked** — returns JS SPA, no OG tags in HTML |
| Instagram | — | — | — | **Blocked** — returns JS SPA login wall |
| Reddit | — | — | — | **Blocked** — returns JS SPA |

Tested User-Agents: `facebookexternalhit/1.1`, `Googlebot/2.1`, Chrome desktop, `Twitterbot/1.0`. All blocked by Twitter, Instagram, Reddit.

### Embed URLs (for webview playback)

| Platform | Embed URL pattern | HTTP status | JS required |
|----------|-------------------|:-----------:|:-----------:|
| YouTube | `youtube.com/embed/{video_id}` | 200 | Yes |
| TikTok | `tiktok.com/embed/v2/{video_id}` | 200 | Yes |
| Spotify | `open.spotify.com/embed/track/{id}` | 200 | Yes |
| Twitter/X | oEmbed HTML blockquote + `platform.twitter.com/widgets.js` | — | Yes |
| Instagram | `instagram.com/p/{id}/embed/` | 200 | Yes (fragile) |

### Recommended Strategy Per Platform

| Platform | Preview card source | On click |
|----------|-------------------|----------|
| YouTube | oEmbed → thumbnail + title + duration + channel | Webview `youtube.com/embed/{id}` |
| TikTok | oEmbed → thumbnail + title + author | Webview `tiktok.com/embed/v2/{id}` |
| Spotify | oEmbed → album art + title | Webview `spotify.com/embed/track/{id}` |
| Twitter/X | oEmbed → author name + tweet text (no image) | Webview renders full tweet with JS widget |
| Instagram | Branded placeholder (icon + "Instagram" + URL) | Webview `instagram.com/p/{id}/embed/` |
| GitHub | OpenGraph → title + description + preview image | Webview direct URL |
| Reddit | Branded placeholder (icon + "Reddit" + URL) | Webview direct URL |
| Other | OpenGraph → title + image + description | Webview direct URL |

### How Other Apps Handle This

- **Signal Desktop**: OpenGraph scraping only. No special handling for Instagram/Twitter. Shows no preview when scraping fails.
- **Discord/Slack/Telegram**: Server-side unfurling with headless browsers or privileged bot user-agents. Not client-side.
- **WhatsApp**: Server-side metadata extraction.

**Conclusion**: For platforms that block client-side scraping (Instagram, Twitter, Reddit), the industry standard is server-side unfurling. A server endpoint that fetches metadata with bot user-agents solves all platforms.

---

## 2. Embeddable Browser Engines for Rust

### Full Comparison Matrix

| Engine | Render to texture | YouTube plays | JS support | Rust-native | Maturity | Binary size impact | Platforms |
|--------|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| **[wew](https://github.com/mycrl/wew)** (CEF) | Yes (offscreen) | Yes | Full (Chromium) | Bindings | 38 stars, active | **+1 GB** | Linux(x11), macOS, Windows |
| **[tauri-apps/cef-rs](https://github.com/tauri-apps/cef-rs)** | API-level (no offscreen docs yet) | Yes | Full (Chromium) | Bindings | 317 stars, very active (Mar 2026) | **+1 GB** | Linux, macOS, Windows |
| **[wef](https://github.com/longbridge/wef)** (CEF) | Yes (offscreen) | Yes | Full (Chromium) | Bindings | 30 stars, **abandoned** | **+1 GB** | Linux, macOS, Windows |
| **[Servo](https://servo.org/)** | Yes (WGPU texture) | **No** (no MSE) | Partial | 100% Rust | v0.0.5, experimental | ~50 MB | Linux, macOS, Windows, Android |
| **[Blitz](https://github.com/DioxusLabs/blitz)** (Dioxus) | Possible (WGPU) | **No** | **No JS** | 100% Rust | Pre-alpha, 3.4k stars | Small | Desktop |
| **[wry](https://github.com/tauri-apps/wry)** | **No** (separate window) | Yes | Full (system engine) | Bindings | Production, Tauri core | Tiny (uses OS) | All platforms |
| **[Ultralight](https://ultralig.ht/)** | Yes (GPU/CPU) | Partial (WebKit) | Yes (JavaScriptCore) | C++ with bindings | Commercial, licensed | ~30 MB | Desktop |

### Detailed Notes

#### wew (CEF offscreen rendering)
- **Best option for inline render-to-texture with full web compatibility**
- Uses CEF 137 (Chromium 137)
- Supports: mouse, keyboard, touch, IME, off-screen rendering, JS bridge
- Limitation: CEF cannot integrate with Cargo (manual setup/scripts required)
- Ships ~1 GB of Chromium binaries (same as Discord, Spotify, Steam)
- GitHub: https://github.com/mycrl/wew

#### tauri-apps/cef-rs
- **Most actively maintained CEF Rust bindings** (317 stars, 1009 commits, 254 releases)
- Updated March 24, 2026 (CEF v146)
- Offscreen rendering API not yet documented but CEF supports it
- Likely to become the standard Rust CEF solution
- "Very experimental" per Tauri team — no ETA for production
- GitHub: https://github.com/tauri-apps/cef-rs

#### Servo
- **Only 100% Rust browser engine**
- Slint has an [official example](https://github.com/slint-ui/slint/tree/master/examples/servo) rendering Servo to WGPU texture
- **Critical limitation**: No Media Source Extensions (MSE) — YouTube won't play
- "Tons of websites' rendering broken, some crashing" — [OSnews review](https://www.osnews.com/story/142940/i-tried-servo-the-undercover-web-browser-engine-made-with-rust/)
- v0.0.5 released late 2025, still experimental
- Embedding API being reworked: https://www.phoronix.com/news/Servo-Embed-API-2025-Progress

#### Blitz (Dioxus)
- Pure Rust HTML/CSS renderer — no JavaScript, no iframes
- Uses Stylo (Firefox CSS engine) + Taffy (layout) + Parley (text)
- Cannot render any interactive web content (YouTube, Instagram, etc.)
- Useful for rendering static HTML previews, not embeds
- GitHub: https://github.com/DioxusLabs/blitz

#### wry (Tauri webview)
- **Production-proven**, powers all Tauri apps
- Uses system webview: WebKitGTK (Linux), WebView2 (Windows), WKWebView (macOS/iOS), Android WebView
- **Cannot render to texture** — only creates standalone windows or child windows
- Slint [cannot embed native widgets](https://github.com/slint-ui/slint/issues/3930) — webview must be a popup
- Simplest integration, smallest binary impact
- GitHub: https://github.com/tauri-apps/wry

#### Ultralight
- Proprietary WebKit-based renderer with GPU-accelerated offscreen rendering
- Can render to CPU bitmap or GPU texture
- Licensing required after revenue threshold
- Smaller than Chromium (~30 MB vs ~1 GB)
- Community Rust bindings exist but not official
- Not open source — licensing concern for a privacy-focused app

#### Verso (Servo-based browser)
- Was a browser built on Servo by a Servo TSC member
- **No longer maintained** — couldn't keep pace with Servo API changes
- Key contributions merged upstream into Servo
- GitHub: https://github.com/versotile-org/verso

### Slint Integration Status

Per [Slint issue #3930](https://github.com/slint-ui/slint/issues/3930) (webview element request, open since 2023):

- **No native webview element in Slint** — still an open feature request
- **Servo example exists**: Renders web content to WGPU texture, composited into Slint. Works on macOS, Linux (hardware), Android (software). See: https://github.com/slint-ui/slint/tree/master/examples/servo
- **Steam-style approach**: Browser engine renders to shared texture, Slint composites it. Input events forwarded from Slint to engine.
- **Recommended workarounds**: External texture import, wry/CEF as separate window, Servo render-to-texture

---

## 3. Architecture Options

### Option A: wry popup (simple, production-ready)

```
Chat bubble:  [Preview card: thumbnail + title]  ← Pure Slint
                        ↓ click
Popup:        [wry standalone window]             ← System webview
              [youtube.com/embed/...]             ← Full playback
```

- **Pros**: Works today, all platforms, tiny binary, everything plays
- **Cons**: Not inline — opens a separate OS window
- **Effort**: Low

### Option B: CEF render-to-texture (Steam approach)

```
Chat bubble:  [Preview card: thumbnail + title]  ← Pure Slint
                        ↓ click
Inline:       [CEF offscreen → GL texture]       ← Composited in Slint
              [youtube.com/embed/...]             ← Full playback inline
```

- **Pros**: Inline playback, everything works (Chromium)
- **Cons**: +1 GB binary, complex integration, CEF can't use Cargo
- **Best library**: wew or tauri-apps/cef-rs
- **Effort**: High

### Option C: Servo render-to-texture (future Rust-native)

```
Chat bubble:  [Preview card: thumbnail + title]  ← Pure Slint
                        ↓ click
Inline:       [Servo → WGPU texture]             ← Composited in Slint
              [youtube.com/embed/...]             ← Broken (no MSE)
```

- **Pros**: 100% Rust, Slint example exists, ~50 MB
- **Cons**: YouTube won't play, many sites broken, experimental
- **Effort**: Medium (example exists) but unreliable results

### Option D: Hybrid (recommended)

Preview cards via server-side unfurling (solves Instagram/Twitter/Reddit metadata).
Click opens wry popup for now. Architecture prepared for CEF/Servo inline upgrade later.

```
Phase 1: Server-side unfurl → preview cards (all platforms)
Phase 2: wry popup on click (production, all platforms)
Phase 3: (Future) Swap wry for CEF inline or Servo when ready
```

- **Pros**: Ships now, works everywhere, upgrade path clear
- **Cons**: Not inline initially
- **Effort**: Medium

---

## 4. Server-Side Unfurling

The industry standard for link previews. A server endpoint fetches metadata with appropriate user-agents.

### Endpoint Design

```
POST /api/v1/unfurl
Request:  { "url": "https://www.instagram.com/p/xxx/" }
Response: {
  "title": "Post by @user",
  "description": "...",
  "thumbnail_url": "https://...",
  "site_name": "Instagram",
  "embed_url": "https://www.instagram.com/p/xxx/embed/",
  "embed_type": "instagram"
}
```

### Server Implementation

- Fetch URL with `User-Agent: facebookexternalhit/1.1` (sites serve OG tags to Facebook's crawler)
- Parse `<head>` for `og:title`, `og:description`, `og:image`, `og:site_name`
- For YouTube/TikTok/Spotify: call their oEmbed API instead (richer data)
- Cache results (URLs rarely change)
- Rate limit per user
- Strip tracking parameters from URLs

### Why Server-Side

Instagram, Twitter/X, and Reddit serve JavaScript SPAs to regular HTTP clients. They only serve OpenGraph meta tags to recognized crawler user-agents (Googlebot, facebookexternalhit, Twitterbot). A server-side endpoint can use these user-agents. Client-side code cannot (these sites also check IP reputation).

---

## 5. References

- Slint webview element: https://github.com/slint-ui/slint/issues/3930
- Slint + Servo example: https://github.com/slint-ui/slint/tree/master/examples/servo
- Slint HTML rendering discussion: https://github.com/slint-ui/slint/discussions/6642
- wry: https://github.com/tauri-apps/wry
- wew (CEF offscreen): https://github.com/mycrl/wew
- tauri-apps/cef-rs: https://github.com/tauri-apps/cef-rs
- wef (abandoned CEF): https://github.com/longbridge/wef
- Servo: https://servo.org/ / https://github.com/servo/servo
- Servo roadmap: https://github.com/servo/servo/wiki/Roadmap
- Blitz (Dioxus HTML/CSS): https://github.com/DioxusLabs/blitz
- Ultralight: https://ultralig.ht/
- Verso (discontinued): https://github.com/versotile-org/verso
- YouTube oEmbed: https://www.youtube.com/oembed?url=...&format=json
- TikTok oEmbed: https://www.tiktok.com/oembed?url=...
- Spotify oEmbed: https://open.spotify.com/oembed?url=...
- Twitter oEmbed: https://publish.twitter.com/oembed?url=...
- Signal link preview code: `Signal-Desktop/ts/linkPreviews/linkPreviewFetch.preload.ts`
- Link preview guide: https://andrejgajdos.com/how-to-create-a-link-preview/
- Embed engine comparison: https://dev.to/lmtr0/which-embed-browser-engine-to-use-3d0h
