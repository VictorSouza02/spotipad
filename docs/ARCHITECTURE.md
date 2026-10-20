# Architecture

Spotipad follows the idea of [Spotifast](https://github.com/crmne/spotifast): a native, fast Spotify client
on top of librespot. On the iPad the frontend is a web UI served by the daemon itself.

## Overview

```
 iPad mini 1 (iOS 9.3.6, jailbroken)
 ┌──────────────────────────────────────────────────────────────┐
 │ Safari (web clip "Spotipad", full screen)                    │
 │   http://127.0.0.1:8888  ── ASCII UI (index.html, ES5)       │
 │            │ XHR JSON                                         │
 │ launchd ── spotipad (root daemon, KeepAlive)                │
 │   ├─ HTTP server (tokio)            /  /api/*  /img/*        │
 │   ├─ App: cache (cache.json), queue, warmup, SWR             │
 │   ├─ librespot Session  ── AP (ap-*.spotify.com:4070)        │
 │   │                      ── spclient (HTTPS, login5 token)   │
 │   ├─ librespot Player   ── audio CDN ── symphonia (Ogg)      │
 │   └─ AqSink ── aq.c ── AudioQueue ── speaker                 │
 └──────────────────────────────────────────────────────────────┘
```

Nothing runs outside the iPad. The computer only builds and deploys over SSH.

## Components

### `native/spotipad_main.c`
Process entry. First it calls `memorystatus_control` to lift the per-process memory limit iOS imposes on
launchd daemons (without it Jetsam kills the daemon when playback starts) and moves it to the audio priority
band (12). Then it calls `spotipad_main(dir, port)` in Rust.

### `librespot/iosglue/src/lib.rs` (daemon)

**Startup**
1. Loads `cache.json` (~20 ms for ~1400 tracks).
2. Starts the HTTP server **before** logging in, so the UI opens with cached data even while booting.
3. Connects to Spotify with `credentials.json` (reusable credentials), with backoff up to 60 s.
4. Starts the warmup and the player event loop.

**Cache (`Store`, persisted to `cache.json`)**
- `meta`: track URI → `{name, artist, img, dur}`.
- `lists`: `playlist:<id>` and `album:<id>` → `{rev, name, uris}`.
- `root`: library tree (folders + playlists) and each playlist's revision.
- `liked`: Liked Songs URIs, newest first.
- Saved every 15 s when dirty (write to `.tmp` + `rename`).

**Freshness**
- Playlists: the rootlist carries every playlist's revision. If the cached revision matches, the playlist is
  never requested again. If not, the cache is returned at once and refreshed in the background.
- Rootlist and Liked Songs: stale-while-revalidate with a 30 s TTL.
- Metadata: only unknown URIs are fetched, in batches of 200 with 4 parallel requests.
- Background jobs are de-duplicated by key (`inflight`).

**Warmup** (at boot and every 15 min): rootlist → playlists whose revision changed (3 in parallel) →
Liked Songs → missing metadata → drop playlists that left the library → download 64 px covers.

**Queue and playback**
- `POST /api/play {ctx, uri, shuffle}`: the server resolves the context (`liked`, `playlist:<id>`,
  `album:<id>`, `search:<q>`) from the cache, so the UI never sends large lists.
- When a track starts (a `Playing` event after a load), the next one is preloaded: a manual skip takes ~20 ms.
- Natural end of track: the next track loads without discarding buffered audio (no gap).
- Manual skip/seek/change: `aq_flush` drops queued audio immediately.
- Reconnect: every 15 s the session is checked; if it dropped, a new one is created and `player.set_session` is called.

**tokio runtime**: 2 workers, up to 8 blocking threads, `thread_keep_alive` of one year.
Threads must not exit on this target (see KNOWLEDGE.md, TLS pitfall).

### `native/aq.c` (audio)
AudioQueue PCM at 44.1 kHz stereo s16, 6 buffers of 16 KB (~93 ms each).
Every buffer has a state `FREE → FILLING → QUEUED`. Enqueue happens under the lock, and `aq_flush` calls
`AudioQueueReset` and reclaims the `QUEUED` buffers itself, because iOS 9 does not call the output callback
for buffers discarded by a reset. `aq_pause`/`aq_resume` use `AudioQueuePause`/`Start`.

### `index.html` (UI)
- Plain ES5, `-webkit-` prefixes, no libraries: the target is iOS 9 Safari on an A5.
- Virtualized list: 52 px rows placed with `translate3d`, a pool of reused nodes.
  A 407-track playlist keeps 22–37 nodes in the DOM.
- Client cache: memory → `localStorage` → network; redraws only if the JSON changed.
- Touch handling tells taps from scrolls (8 px threshold) and gives inverse-video feedback.
- Player bar: scrolling marquee, `[#####>-----]` progress interpolated locally,
  status polled every 1.5 s (3 s while playing).
- Stack navigation with a DOS-style path (`C:\LIBRARY\ROCK>_`).

### Spotify endpoints used
All with the **login5** token (not the public Web API). See KNOWLEDGE.md for why.

| Use | Endpoint |
|---|---|
| Login | AP via `apresolve`, port 4070 |
| Library and folders | `GET /playlist/v2/user/<user>/rootlist?decorate=revision,attributes,length,owner` |
| Playlist | `GET /playlist/v2/playlist/<id>` |
| Liked Songs | `POST /collection/v2/paging` (protobuf `collection2v2.PageRequest`) |
| Metadata | `POST /extended-metadata/v0/extended-metadata` (`TRACK_V4`, `ALBUM_V4`) |
| Search | `GET /context-resolve/v1/spotify:search:<words+joined>` without the metrics parameters |
| Covers | `https://i.scdn.co/image/<hex>` (proxied and disk-cached by the daemon) |

## Files on the iPad

| Path | What |
|---|---|
| `/var/root/spotipad/spotipad` | binary (signed with `ldid -S` on the iPad) |
| `/var/root/spotipad/credentials.json` | reusable credentials (secret) |
| `/var/root/spotipad/cache.json` | library cache |
| `/var/root/spotipad/img/` | 64 px covers |
| `/var/root/spotipad/log.txt` | log (stdout+stderr) |
| `/Library/LaunchDaemons/dev.spotipad.daemon.plist` | daemon (RunAtLoad, KeepAlive, `SP_PLATFORM=17`) |
| `/var/mobile/Library/WebClips/5B0F...FA57.webclip/` | home-screen icon → `http://127.0.0.1:8888/` |
