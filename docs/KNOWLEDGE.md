# What we learned

Everything that had to be figured out to play Spotify on an iPad mini 1 with iOS 9, with the root cause of
each problem. Behaviour observed in October 2026; Spotify changes endpoints without notice.

## 1. Toolchain: Rust for armv7 / iOS 9 without an Apple SDK

- **Target**: use the built-in `armv7s-apple-ios` (tier 3, has std) with `-Z build-std=std,panic_abort`.
  The A5 is armv7, not armv7s: compile with `-C target-cpu=cortex-a9`, then change the Mach-O `cpusubtype`
  from 0x0B to 0x09 (offset 8). A custom armv7 target JSON triggers an ICE on nightly.
- **Nightly**: `nightly-2024-07-15`. 1.101 hits an ICE compiling `vergen` (a librespot 0.6 build dependency).
- **NEON flags**: `-C target-feature=+vfp3,+neon` worked but brought no measurable gain; removed.
  (It was suspected of breaking the crypto; it was not, see section 3.)
- **Host SDK**: build scripts need `SDKROOT=.../MacOSX15.sdk`; the beta SDK 27 breaks the CLT `ld`.
- **No iOS SDK**: `.tbd` stubs generated from the iPad's own dyld cache with `tools/dsc.py`. `ipsw` cannot read
  the iOS 9 dyld v1 format. `libSystem.B.dylib` re-exports everything from `libsystem_*`: its stub merges those
  exports (~9200 symbols).
- **iOS 9 libc**: Rust's std references 5 symbols that only exist on iOS 10+:
  `clock_gettime`, `fclonefileat`, `renamex_np`, `sysdir_start_search_path_enumeration`,
  `sysdir_get_next_search_path_enumeration`. `native/shim.c` implements them (clock via `gettimeofday`,
  the rest with a fallback or `ENOSYS`).
- **C**: `clang -target armv7s-apple-ios9.0 -nostdinc` (plain `-arch armv7s` tags the platform as macOS and the link fails).
- **Link**: `ld -arch armv7s -ios_version_min 9.0 ... sdk/tbd/libSystem.B.dylib.tbd`, passing the `.tbd` as a file.
- **Signing**: `ldid -S` runs **on the iPad** (not available on the Mac). AppSync Unified allows self-signed binaries.

## 2. Crypto without `ring`

- `ring` 0.17 has no assembly for 32-bit Apple (undefined `_ring_core_*` symbols at link) and `aws-lc-sys`
  does not cross-compile to armv7-ios.
- Fix: rustls with the pure-Rust provider **`rustls-rustcrypto`**:
  - `vendor/rustls` and `vendor/tokio-rustls` without the `ring`/`aws_lc_rs` default features, wired in via `[patch.crates-io]`.
  - `hyper-rustls` without default features, with `webpki-tokio`, and `with_provider_and_webpki_roots(...)`:
    iOS gives rustls no native CA roots, so the Mozilla roots are embedded.
  - `hyper-proxy2` without rustls; `from_proxy_unsecured` (the inner connector is already HTTPS).
  - `librespot-oauth` dependency removed from core (it pulled reqwest 0.11 → rustls 0.21 → ring).
  - `zeroize = "=1.7.0"` and `zeroize_derive = "=1.4.2"` pinned (newer manifests do not parse with the 2024 cargo).
  - The daemon installs the provider first: `CryptoProvider::install_default(rustls_rustcrypto::provider())`.
  - `cargo tree -i ring` must be empty.

## 3. Logging in: the platform tag

- The access point answers `TryAnotherAP` in a loop to **any** iPhone `ClientHello.build_info.platform`
  (`PLATFORM_IPHONE_ARM=3`, `PLATFORM_IPHONE_ARM64=36`) sent by a non-official client.
- Proven on a Mac with the same binary and the same credentials: platform 1 (OSX) logs in, 3 and 36 do not,
  17 (`PLATFORM_LINUX_ARM`) logs in **even with the login declaring `OS_IPHONE` and `CPU_ARM`**.
  Only the hello tag is policed.
- Ruled out by testing: the iPad's egress IP (a tunnel through another machine's IP still failed),
  crypto/NEON, ports, region.
- Project choice: run with `SP_PLATFORM=17` (what librespot sends on a Raspberry Pi).
  The source keeps the honest value; the override comes from the environment variable in the launchd plist.
- Some networks block the access points' ports 443/80; 4070 usually passes. Do not force `ap_port`:
  that filters out the dealer/spclient endpoints (only on :443) and breaks `apresolve`.

## 4. After login: what Spotify changed

- **Client id**: on iOS librespot uses the official iOS app's client id. Reusable credentials made with librespot's
  own client id (keymaster, `65b708073fc0480ea92a077233ca87bd`) must match it for login5 (`INVALID_CREDENTIALS`).
  Fix: `"ios" => KEYMASTER_CLIENT_ID` in `core/src/config.rs`.
- **Client token**: with the keymaster client id, the iOS platform struct returns HTTP 400. It now sends the
  generic `desktop_linux` struct with real `uname` values (`Darwin`, kernel 15.6.0, `arm`).
- **Metadata without audio files**: the legacy `/metadata/4/track/<id>` endpoint started returning `file: []`
  (librespot said "not available in any supported format", even on a Mac). Port of upstream PR
  [#1622](https://github.com/librespot-org/librespot/pull/1622): `POST /extended-metadata/v0/extended-metadata`
  with `ExtensionKind::TRACK_V4`, unwrapping `extension_data.value`.
- **Protobuf body**: librespot 0.6 sent protobuf in **text format** from `request_with_protobuf`;
  extended-metadata answers 400. It now sends `write_to_bytes()` (new `SpClient::request_bytes`).
- **The public Web API (`api.spotify.com`) is unusable** with librespot's client id: the keymaster token endpoint
  answers 403 and the login5 token gets a permanent 429 (the client id quota is shared by every librespot user).
  Everything uses internal spclient endpoints instead: rootlist, playlist v2, collection v2 (Liked Songs,
  `collection2v2.proto` added to `protocol/build.rs`, content type `application/vnd.collection-v2.spotify.proto`)
  and extended-metadata.
- **Search**: `searchview` is gone (400/404). `GET /context-resolve/v1/spotify:search:<words+joined>` works and
  returns track URIs only. It must be called **without** the `product/country/salt` parameters librespot's
  spclient appends: they leak into the search query.
- **Folders**: in the rootlist, a folder is a pair of items `spotify:start-group:<id>:<url-encoded name>` …
  `spotify:end-group:<id>`. `meta_items` is index-aligned with `items` and carries each playlist's revision.
- **Revisions**: a playlist's revision in the rootlist equals the playlist's `SelectedListContent.revision`.
  That tells whether the cache is fresh without downloading the playlist.

## 5. iOS 9 as a platform

- **Jetsam**: launchd daemons get a per-process memory limit of a few MB and are killed when playback starts
  (`JetsamEvent ... per-process-limit`). The `JetsamProperties` key in the plist is ignored. Fix inside the
  process, as root: `memorystatus_control(SET_JETSAM_HIGH_WATER_MARK, pid, -1)` and `SET_PRIORITY_PROPERTIES`
  with priority 12 (audio band). See the entry file in `native/`.
- **512 MB of RAM**: forgotten processes (test runs started over SSH survive the session) cause `vm-pageshortage`
  and Jetsam kills the daemon. Check with `ps` and kill leftovers.
- **AudioQueue**: works from a root daemon without AVAudioSession. On iOS 9 `AudioQueueReset` does **not** hand
  discarded buffers back through the callback, and the writer blocks forever. `aq.c` tracks every buffer's state
  and reclaims them itself.
- **iOS 9 Safari**: ES5 only, no `fetch` (use `XMLHttpRequest`), `-webkit-` prefixes for flex, transforms and
  animations. `width=device-width` removes the 300 ms click delay. Some CDNs' modern TLS fails, so covers are
  proxied by the daemon.
- **Web clip**: a `*.webclip` folder in `/var/mobile/Library/WebClips/` with an `Info.plist`
  (`URL`, `Title`, `FullScreen`, `IconIsPrecomposed`...) and `icon.png`, owned by `mobile`. SpringBoard must be restarted.
- **Missing tools on the iPad**: no `curl`, `python3`, `awk`, `netstat`, `strings`. There is `openssl`, `ldid`,
  `launchctl`. HTTP tests were run from another machine on the same network.
- **Semi-untethered jailbreak**: after a reboot it (and the daemon) only comes back once the jailbreak is re-applied.

## 6. Rust pitfalls on this target

- **Threads must not exit.** When a thread ends, std's thread-local destructor
  (`thread_local::native::lazy::destroy`, used by parking_lot) makes a misaligned access and the process dies with
  `EXC_BAD_ACCESS (SIGBUS) EXC_ARM_DA_ALIGN`. The real trigger: tokio's blocking pool (used by `tokio::fs` and the
  DNS resolver) retires threads after 10 s idle. Fix: `thread_keep_alive` of one year on the runtime and no `tokio::fs`.
- **Fat LTO + `codegen-units = 1`** made that crash happen as soon as playback started. Default release build, no LTO.
- **`Send` in handlers**: do not hold a `MutexGuard` across `.await` (the compiler rejects the `tokio::spawn`).

## 7. Performance

| Change | Effect |
|---|---|
| `cache.json` + playlist revisions | 407-track playlist: 1.4 s → 40 ms; restart with zero playlist requests |
| Only missing metadata, batches of 200 × 4 in parallel | 1385 tracks in ~1.5 s on the first warmup |
| Skip `decoder.seek(0)` on load (`playback/src/player.rs`) | track start 2.1 s → 0.8–1.1 s. Seeking to zero made symphonia's Ogg demuxer bisect the file with several CDN range reads |
| Preload the next track as soon as the current one starts | skip 1.0 s → ~20 ms |
| `aq_flush` on manual changes | the new track starts without ~0.5 s of the old one |
| HTTP up before login | UI opens during boot with the cache |
| Virtualized list in the UI | 22–37 DOM nodes for 407 tracks |
| 64 px covers cached on disk | ~3 KB each, served locally |

CPU while playing: 10–15 % (~65 % peak while loading a track). Memory: ~13 MB.

## 8. Debugging

- `core/src/http_client.rs` prints `[HTTP] <status> <url> headers=... body=...` for every non-200 response.
  That is what revealed the text-protobuf 400, the Web API 429 and its `retry-after`.
- The daemon logs each step's time (`rootlist: N in X ms`, `metadata: a/b tracks in X ms`, routes slower than 150 ms).
- Crash: pull the `.ips` and symbolicate with `atos -arch armv7 -o <binary> -l <load addr> <addrs>`.
