#!/bin/bash
# Build the Spotipad daemon (armv7, iOS 9) into native/spotipad. Host: macOS with Command Line Tools.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export SDKROOT="${SDKROOT:-/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk}"
export IPHONEOS_DEPLOYMENT_TARGET=9.0 CC_armv7s_apple_ios=clang
export CFLAGS_armv7s_apple_ios="-target armv7s-apple-ios9.0 -isysroot $SDKROOT -miphoneos-version-min=9.0"
# A5 is armv7 (cortex-a9), the builtin target is armv7s: downgrade codegen, patch cpusubtype below.
export RUSTFLAGS="-C target-cpu=cortex-a9"
cd "$ROOT/librespot"
cargo +nightly-2024-07-15 build --release -p iosglue -Z build-std=std,panic_abort --target armv7s-apple-ios
cd "$ROOT/native"
for f in aq spotipad_main shim; do clang -target armv7s-apple-ios9.0 -nostdinc -O2 -c $f.c -o $f.o 2>/dev/null; done
T="$ROOT/sdk/tbd"
ld -arch armv7s -ios_version_min 9.0 -o spotipad spotipad_main.o aq.o shim.o \
  "$CARGO_TARGET_DIR/armv7s-apple-ios/release/libiosglue.a" \
  $T/libSystem.B.dylib.tbd $T/AudioToolbox.tbd $T/CoreFoundation.tbd $T/Security.tbd $T/CFNetwork.tbd $T/Foundation.tbd \
  2>&1 | grep -v "warning: object file" || true
# Mach-O cpusubtype ARM_V7S (0x0B) -> ARM_V7 (0x09) so the A5 accepts the binary.
python3 -c "import struct,sys;f=open(sys.argv[1],'r+b');f.seek(8);f.write(struct.pack('<I',9));f.close()" spotipad
ls -la spotipad
