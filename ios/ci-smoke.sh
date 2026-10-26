#!/bin/bash
# End-to-end check of the iOS app in the simulator (CI, macOS): launch it,
# pair it with a desktop bsyncd on the same machine, share a folder from
# the desktop and check that files sync both ways.
# Usage: ios/ci-smoke.sh <path to BetterSync.app> <dir with bsyncd and bsync>
set -euo pipefail
APP="$1"
BIN="$2"
BUNDLE=com.bettersync.app

UDID=$(xcrun simctl list devices available -j | python3 -c '
import json, sys
devs = [d for rt, ds in json.load(sys.stdin)["devices"].items() if "iOS" in rt for d in ds]
print(next(d["udid"] for d in devs if d["name"].startswith("iPhone")))')
echo "simulator $UDID"
xcrun simctl boot "$UDID" || true
xcrun simctl bootstatus "$UDID" -b
xcrun simctl install "$UDID" "$APP"
xcrun simctl launch "$UDID" "$BUNDLE"

DATA=$(xcrun simctl get_app_container "$UDID" "$BUNDLE" data)
CFG="$DATA/Library/Application Support/BetterSync"
for _ in $(seq 60); do [ -f "$CFG/api.addr" ] && break; sleep 1; done
PHONE_API="http://$(cat "$CFG/api.addr")"
PHONE_TOKEN=$(cat "$CFG/api.token")
phone() { curl -sf -H "Authorization: Bearer $PHONE_TOKEN" -H 'Content-Type: application/json' "$@"; }
PHONE_ID=$(phone "$PHONE_API/api/status" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device_id"])')
echo "phone $PHONE_ID"

DESK=$(mktemp -d)
"$BIN/bsync" --config-dir "$DESK/cfg" init --name desk --listen 127.0.0.1:22100 --api 127.0.0.1:0
DESK_ID=$("$BIN/bsync" --config-dir "$DESK/cfg" id)
"$BIN/bsyncd" --config-dir "$DESK/cfg" > "$DESK/bsyncd.log" 2>&1 &
DAEMON=$!
trap 'kill $DAEMON 2>/dev/null || true; cat "$DESK/bsyncd.log" | tail -40' EXIT
for _ in $(seq 30); do [ -f "$DESK/cfg/api.addr" ] && break; sleep 1; done

"$BIN/bsync" --config-dir "$DESK/cfg" pair "$PHONE_ID" --name phone
phone -X POST "$PHONE_API/api/pair" \
    -d "{\"id\":\"$DESK_ID\",\"name\":\"desk\",\"addresses\":[\"127.0.0.1:22100\"]}"
echo
mkdir -p "$DESK/share"
echo "from the desktop" > "$DESK/share/hello.txt"
"$BIN/bsync" --config-dir "$DESK/cfg" folder add shared "$DESK/share" --share "$PHONE_ID"

wait_for() { # file content
    for _ in $(seq 90); do
        if [ -f "$1" ] && [ "$(cat "$1")" = "$2" ]; then return 0; fi
        sleep 1
    done
    echo "timed out waiting for $1"; return 1
}
wait_for "$DATA/Documents/shared/hello.txt" "from the desktop"
echo "desktop -> phone ok"
echo "from the phone" > "$DATA/Documents/shared/reply.txt"
wait_for "$DESK/share/reply.txt" "from the phone"
echo "phone -> desktop ok"
phone "$PHONE_API/api/status" | python3 -m json.tool | head -40
