#!/bin/bash
# Build, push, sign on device and restart the launchd daemon. Needs IPAD_PASS (see ipad-ssh.exp).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
"$ROOT/scripts/build.sh"
"$ROOT/scripts/ipad-push.exp" "$ROOT/native/spotipad" /var/root/spotipad/spotipad.new | grep -o "100%" | tail -1
"$ROOT/scripts/ipad-ssh.exp" 'cd /var/root/spotipad && ldid -S spotipad.new && mv spotipad.new spotipad && launchctl unload /Library/LaunchDaemons/dev.spotipad.daemon.plist; : > log.txt; launchctl load /Library/LaunchDaemons/dev.spotipad.daemon.plist; sleep 12; launchctl list | grep spotipad; tail -6 log.txt'
