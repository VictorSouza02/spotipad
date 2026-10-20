# Spotipad

**Spotipad** is an unofficial port of **[Spotifast](https://spotifast.rocks)** to **old jailbroken iPads**
(tested on an iPad mini 1st gen, A5/armv7, iOS 9.3.6), where the official Spotify app no longer installs.

Everything runs on the iPad: a Rust daemon built on the same playback engine Spotifast uses
([librespot](https://github.com/librespot-org/librespot)) plays the music, and a web UI with an early-2000s
terminal look opens from a home-screen icon.

Requires **Spotify Premium**.

```
.-------------.
|      o      |
| .---------. |    ____              _   _                 _
| |         | |   / ___| _ __   ___ | |_(_)_ __   __ _  __| |
| |   |>    | |   \___ \| '_ \ / _ \| __| | '_ \ / _` |/ _` |
| | ||:|:|| | |    ___) | |_) | (_) | |_| | |_) | (_| | (_| |
| '---------' |   |____/| .__/ \___/ \__|_| .__/ \__,_|\__,_|
|     (_)     |         |_|               |_|
'-------------'
```

**Works:** library with folders, playlists, Liked Songs, track search, queue, play all, shuffle,
next, previous and seek. Playlists open in ~40 ms, skipping takes ~20 ms, ~13 MB of memory.

**Not yet:** Spotify Connect, album/playlist search, on-screen volume (use the hardware buttons).

## Credits

This project exists because of **Spotifast**, by **[Carmine Paolino](https://paolino.me)**:

- Website: <https://spotifast.rocks>
- Source: <https://github.com/crmne/spotifast> (MIT)
- Getting started: <https://spotifast.rocks/getting-started/>
- Flatpak: `rocks.spotifast.Spotifast`

Spotifast is the base project and the inspiration: a native, fast Spotify client in Rust on top of librespot.
Its desktop UI (egui/wgpu, x86_64/arm64) cannot run on a 32-bit iPad with iOS 9, so this port keeps the same
idea and the same engine and replaces the frontend with a daemon plus a web UI. All credit for the concept goes
to Spotifast. Please support the original project.

The playback engine is [librespot](https://github.com/librespot-org/librespot) 0.6.0 (MIT, see
`librespot/LICENSE`), modified for iOS 9 as described in [docs/KNOWLEDGE.md](docs/KNOWLEDGE.md).

This port is not affiliated with Spotifast, librespot or Spotify.

## Requirements

| Where | What |
|---|---|
| iPad | jailbreak, **OpenSSH**, **ldid** and **AppSync Unified** (Cydia), Wi-Fi |
| Computer | **macOS** with Command Line Tools (`xcode-select --install`), `expect`, `python3` |
| Rust | `rustup toolchain install nightly-2024-07-15 -c rust-src` |
| Account | Spotify Premium and a librespot `credentials.json` (step 1) |

The build needs Apple's linker, so it **does not work on Linux or Windows**. Xcode is not required.

## Install

**1. Create credentials** (once, on any computer with the official [librespot](https://github.com/librespot-org/librespot)):

```bash
librespot --enable-oauth --system-cache /tmp/spcache --name generator
```

Log in in the browser when asked, then stop it with Ctrl+C. The file is `/tmp/spcache/credentials.json`.
Keep it outside the repository.

**2. Install on the iPad:**

```bash
export IPAD_HOST=192.168.x.x      # iPad IP address
export IPAD_PASS='root-password'  # iPad root password (jailbreak default: alpine)
scripts/install.sh /tmp/spcache/credentials.json
```

The first build compiles the Rust standard library for armv7 (3–5 min). At the end the iPad restarts its
home screen and the **Spotipad** icon appears.

**3. Update after changing the code:**

```bash
scripts/deploy.sh
```

If the computer is not on the iPad's network, set `IPAD_JUMP=user@host-on-that-network` to go through
another machine over SSH.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| The icon opens and says "no connection" | Daemon stopped. `scripts/ipad-ssh.exp 'tail -30 /var/root/spotipad/log.txt'` |
| Stopped working after an iPad reboot | Semi-untethered jailbreak: re-apply the jailbreak; the daemon starts by itself |
| Log shows `Try another access point` in a loop | `SP_PLATFORM=17` is missing from the launchd plist (see docs/KNOWLEDGE.md) |
| Log shows `INVALID_CREDENTIALS` | `credentials.json` is invalid or was made by another client. Redo step 1 |
| Daemon dies when playing | Memory. Look for `JetsamEvent-*.ips` in `/var/mobile/Library/Logs/CrashReporter/` |
| Network blocks Spotify | Access points use ports 4070, 443 and 80. Port 4070 must be open if the others are blocked |

## Layout

| Path | Contents |
|---|---|
| `librespot/` | librespot 0.6.0 crates adapted for iOS 9 + `iosglue/` (daemon, API and UI) |
| `native/` | C: audio output (`aq.c`), libc functions missing on iOS 9 (`shim.c`), program entry |
| `vendor/` | rustls without assembly dependencies, so it builds for armv7 |
| `sdk/tbd/` | iOS 9 library stubs (replace the iOS SDK) |
| `tools/dsc.py` | generates new stubs from the iPad's dyld cache, if needed |
| `deploy/` | launchd service, home-screen shortcut and icon |
| `scripts/` | `install.sh`, `deploy.sh`, `build.sh` and SSH helpers |
| `docs/` | architecture, build, API and what was learned about Spotify and iOS 9 |

## Documentation

- [docs/BUILD-DEPLOY.md](docs/BUILD-DEPLOY.md) — build details, operation and debugging.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — how the daemon, cache, audio and UI work.
- [docs/API.md](docs/API.md) — HTTP endpoints (control it from another device on the network).
- [docs/KNOWLEDGE.md](docs/KNOWLEDGE.md) — Spotify protocol, iOS 9 and pitfalls, with root causes.

## Notice

Unofficial project for use with your own Premium account. To be able to log in, the daemon identifies itself
to Spotify with the same platform tag librespot sends on a Raspberry Pi (`SP_PLATFORM=17`); the login still
declares iOS. The API on `:8888` is open to the local network.
