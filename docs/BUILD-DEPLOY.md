# Build and deploy

## Requirements

**Build host: macOS** with Command Line Tools (Apple's `ld` and `clang` with the iOS target).
Linux does not work: there is no compatible Mach-O linker. Xcode and the iOS SDK are not needed.

- `rustup toolchain install nightly-2024-07-15 -c rust-src`
  - Recent nightlies (1.101) hit an ICE compiling `vergen`, used by librespot 0.6, and a custom armv7 target
    JSON also ICEs. This 2024 nightly compiles everything.
- `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk` (the `build.sh` default).
  A beta SDK (MacOSX27) has an `arm64e.x1` slice the CLT `ld` cannot parse, which breaks build scripts.
- `expect` (password SSH to the iPad) and `python3`.

**iPad**: jailbreak with OpenSSH, `ldid` installed on the iPad itself, and AppSync Unified.
iOS 9 SSH only speaks old algorithms (`ssh-rsa`, `diffie-hellman-group14-sha1`); the scripts pass those options.

## Script variables

| Variable | Default | Use |
|---|---|---|
| `IPAD_PASS` | — (required) | iPad root password. Never commit it |
| `IPAD_HOST` | — (required) | iPad IP address on the network |
| `IPAD_JUMP` | empty | ProxyJump, e.g. `user@host-on-that-network`, when the computer is not on the iPad's network |
| `CARGO_TARGET_DIR` | `./target` | build directory |

## Build

```bash
scripts/build.sh      # produces native/spotipad (armv7 Mach-O, ~33 MB)
```

What happens:
1. `cargo +nightly-2024-07-15 build --release -p iosglue -Z build-std=std,panic_abort --target armv7s-apple-ios`
   with `RUSTFLAGS="-C target-cpu=cortex-a9"`. The built-in target is armv7s (A6); the A5 is armv7, so codegen
   is lowered to Cortex-A9.
2. Compiles `aq.c`, `shim.c` and the entry file with `clang -target armv7s-apple-ios9.0 -nostdinc`.
3. Links with `ld -arch armv7s -ios_version_min 9.0` against the stubs in `sdk/tbd/` (passed as files, not `-l`).
4. Changes the Mach-O `cpusubtype` from `ARM_V7S` (0x0B) to `ARM_V7` (0x09), header offset 8.

The first build compiles the whole std (~3–5 min). Later builds are incremental.

**Do not enable LTO** (`lto = "fat"`/`codegen-units = 1`): the binary then crashes with `EXC_ARM_DA_ALIGN`
whenever a thread exits. See KNOWLEDGE.md.

## Credentials

The daemon uses librespot **reusable credentials** (`credentials.json`: `username`, `auth_type`, `auth_data`),
not a one-hour token. To create them:

1. On a desktop, run the official librespot with OAuth and a system cache, for example
   `librespot --enable-oauth --system-cache /tmp/spcache --name generator`.
2. Log in in the browser when asked. It writes `/tmp/spcache/credentials.json`.
3. Pass that file to `install.sh`. Keep a copy outside the repository (`.gitignore` already ignores `credentials*.json`).

The daemon rewrites the file when Spotify returns new credentials.

## First install

```bash
export IPAD_HOST=192.168.x.x IPAD_PASS='...'
scripts/install.sh /path/to/credentials.json
```

Creates the data directory, uploads credentials, binary, launchd plist and the web clip, signs with `ldid -S`
on the iPad, loads the daemon and restarts SpringBoard so the icon appears.

## Update

```bash
scripts/deploy.sh
```

Build → upload the new binary → `ldid -S` → swap → `launchctl unload/load` → print the end of the log.

## Operating the iPad

```bash
scripts/ipad-ssh.exp 'tail -50 /var/root/spotipad/log.txt'
scripts/ipad-ssh.exp 'launchctl list | grep spotipad'
scripts/ipad-ssh.exp 'launchctl unload /Library/LaunchDaemons/dev.spotipad.daemon.plist'   # stop
scripts/ipad-ssh.exp 'launchctl load /Library/LaunchDaemons/dev.spotipad.daemon.plist'     # start
scripts/ipad-ssh.exp 'rm /var/root/spotipad/cache.json'   # force a full reload (with the daemon stopped)
```

Crashes go to `/var/mobile/Library/Logs/CrashReporter/spotipad-*.ips`. To symbolicate on the Mac:

```bash
atos -arch armv7 -o native/spotipad -l <load address from the report> <addresses...>
```

Memory kills show up as `JetsamEvent-*.ips` with `reason: per-process-limit` or `vm-pageshortage`.

## After rebooting the iPad

The jailbreak is semi-untethered: after a reboot, re-apply the jailbreak. launchd then loads the daemon by itself.

## Generating new `.tbd` stubs

If you need another system library (for example UIKit):

1. Copy the iPad's dyld cache: `/System/Library/Caches/com.apple.dyld/dyld_shared_cache_armv7` (~540 MB).
2. Use `tools/dsc.py` (function `emit_tbd`), which reads the iOS 9 dyld v1 format that `ipsw` cannot.
3. `libSystem.B.dylib` is an umbrella that exports only 7 symbols: its stub must merge the exports of every
   `/usr/lib/system/libsystem_*.dylib` + libc++ + libobjc.

## Testing the UI off the iPad

The UI is a plain web page. With a tunnel to the iPad (`ssh -L 18888:<ipad-ip>:8888 user@host-on-that-network`)
any browser opens `http://127.0.0.1:18888/`. For an iPad viewport use 768×1024.
