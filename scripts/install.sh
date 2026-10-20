#!/bin/bash
# First-time install on the iPad: data dir, credentials, launchd daemon, home-screen web clip.
# Usage: IPAD_PASS=... scripts/install.sh /path/to/credentials.json
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CREDS="${1:?usage: install.sh credentials.json}"
S="$ROOT/scripts"
W=/var/mobile/Library/WebClips/5B0F1A7E5F0D4C1D9A11C0DE5F07FA57.webclip
"$ROOT/scripts/build.sh"
"$S/ipad-ssh.exp" "mkdir -p /var/root/spotipad $W"
"$S/ipad-push.exp" "$CREDS" /var/root/spotipad/credentials.json
"$S/ipad-push.exp" "$ROOT/native/spotipad" /var/root/spotipad/spotipad
"$S/ipad-push.exp" "$ROOT/deploy/dev.spotipad.daemon.plist" /Library/LaunchDaemons/dev.spotipad.daemon.plist
"$S/ipad-push.exp" "$ROOT/deploy/webclip-Info.plist" $W/Info.plist
"$S/ipad-push.exp" "$ROOT/deploy/icon.png" $W/icon.png
"$S/ipad-ssh.exp" "chmod 600 /var/root/spotipad/credentials.json; ldid -S /var/root/spotipad/spotipad; chown root:wheel /Library/LaunchDaemons/dev.spotipad.daemon.plist; chmod 644 /Library/LaunchDaemons/dev.spotipad.daemon.plist; chown -R mobile:mobile $W; launchctl load /Library/LaunchDaemons/dev.spotipad.daemon.plist; sleep 2; killall -9 SpringBoard"
