#!/bin/bash
# Run from the repository root. The installed app remains available for rollback.
set -euo pipefail

dev_dir="$(pwd)/target/macos-dev"
dev_app="$dev_dir/Lan Mouse Dev.app"
remote_host="${LAN_MOUSE_DEV_REMOTE:-szengel@handwerkerle-6}"

sign() {
    local identity="${LAN_MOUSE_DEV_SIGNING_IDENTITY:-}"
    local identity_file="$dev_dir/signing-identity"
    if [ -z "$identity" ] && [ -f "$identity_file" ]; then
        identity=$(cat "$identity_file")
    fi
    if [ -z "$identity" ]; then
        identity=$(security find-identity -v -p codesigning | awk '/"Apple Development:/ {print $2; exit}')
    fi
    if [ -z "$identity" ]; then
        echo "No Apple Development signing identity available. Set LAN_MOUSE_DEV_SIGNING_IDENTITY to a valid certificate hash." >&2
        return 1
    fi
    printf '%s\n' "$identity" > "$identity_file"
    # Remove the earlier ad-hoc requirement so codesign generates a default
    # requirement anchored to the developer certificate and bundle identifier.
    codesign --remove-signature "$dev_app/Contents/MacOS/lan-mouse"
    find "$dev_app/Contents/Frameworks" -type f \
        -exec codesign --force --sign "$identity" --timestamp=none {} +
    codesign --force --sign "$identity" --timestamp=none "$dev_app"
    codesign --verify --deep --strict "$dev_app"
}

build() {
    MACOSX_DEPLOYMENT_TARGET=13.0 cargo build -p lan-mouse --no-default-features
    stop_local
    mkdir -p "$dev_app/Contents/MacOS"
    cp target/debug/lan-mouse "$dev_app/Contents/MacOS/lan-mouse"
    python3 - "$dev_app" <<'PY'
import pathlib, plistlib, sys
app = pathlib.Path(sys.argv[1])
info = {
    "CFBundleExecutable": "lan-mouse",
    "CFBundleIdentifier": "de.feschber.LanMouse.Dev",
    "CFBundleName": "Lan Mouse Dev",
    "CFBundlePackageType": "APPL",
    "CFBundleVersion": "0.11.0",
    "CFBundleShortVersionString": "0.11.0",
    "LSUIElement": True,
    "NSAppSleepDisabled": True,
    "NSInputMonitoringUsageDescription": "Capture input for LAN Mouse development.",
    "NSLocalNetworkUsageDescription": "Share keyboard, mouse, and trackpad input with your other Mac.",
}
with (app / "Contents/Info.plist").open("wb") as out:
    plistlib.dump(info, out)
PY
    bash scripts/copy-macos-dylib.sh "$dev_app/Contents/MacOS/lan-mouse"
    sign
}

stop_local() {
    # Match executable paths, not unrelated processes containing the app name.
    local pids
    pids=$(ps -axo pid=,comm= | awk -v dev="$dev_app/Contents/MacOS/lan-mouse" \
        '$2 == "/Applications/Lan" && $3 == "Mouse.app/Contents/MacOS/lan-mouse" {print $1} index($0, dev) {print $1}')
    if [ -n "$pids" ]; then
        while read -r pid; do kill -INT "$pid" 2>/dev/null || true; done <<< "$pids"
        for attempt in {1..30}; do
            local alive=0
            while read -r pid; do kill -0 "$pid" 2>/dev/null && alive=1; done <<< "$pids"
            [ "$alive" = 0 ] && return
            sleep 0.1
        done
        echo "Previous LAN Mouse process did not exit; refusing to start a second instance." >&2
        return 1
    fi
}

run_local() {
    stop_local
    mkdir -p "$dev_dir"
    # LaunchServices keeps the daemon alive after the invoking terminal exits.
    open -n --env LAN_MOUSE_LOG_LEVEL=debug --env LAN_MOUSE_SPACES_SWIPE=1 --env LAN_MOUSE_DEV_APP=1 --env "LAN_MOUSE_DEV_PERMISSIONS=${LAN_MOUSE_DEV_PERMISSIONS:-0}" --stdout "$dev_dir/stdout.log" --stderr "$dev_dir/stderr.log" "$dev_app" --args daemon
    echo "Local log: $dev_dir/stderr.log"
}

deploy() {
    ssh -o BatchMode=yes "$remote_host" 'mkdir -p "$HOME/lan-mouse-dev/incoming"'
    tar -C "$dev_dir" -cf - 'Lan Mouse Dev.app' | \
        ssh -o BatchMode=yes "$remote_host" 'tar -C "$HOME/lan-mouse-dev/incoming" -xf -'
}

run_remote() {
    ssh -o BatchMode=yes "$remote_host" 'bash -s' <<'REMOTE'
set -euo pipefail
app="$HOME/lan-mouse-dev/Lan Mouse Dev.app"
pids=$(ps -axo pid=,comm= | awk -v dev="$app/Contents/MacOS/lan-mouse" \
    '$2 == "/Applications/Lan" && $3 == "Mouse.app/Contents/MacOS/lan-mouse" {print $1} index($0, dev) {print $1}')
if [ -n "$pids" ]; then
    while read -r pid; do kill -INT "$pid" 2>/dev/null || true; done <<< "$pids"
    for attempt in {1..30}; do
        alive=0
        while read -r pid; do kill -0 "$pid" 2>/dev/null && alive=1; done <<< "$pids"
        [ "$alive" = 0 ] && break
        sleep 0.1
    done
    [ "$alive" = 0 ] || { echo "Previous LAN Mouse process did not exit." >&2; exit 1; }
fi
ditto "$HOME/lan-mouse-dev/incoming/Lan Mouse Dev.app" "$app"
open -n --env LAN_MOUSE_LOG_LEVEL=debug --env LAN_MOUSE_SPACES_SWIPE=1 --env LAN_MOUSE_DEV_APP=1 --stdout "$HOME/lan-mouse-dev/stdout.log" --stderr "$HOME/lan-mouse-dev/stderr.log" "$app" --args daemon
echo "Remote log: ~/lan-mouse-dev/stderr.log"
REMOTE
}

case "${1:-}" in
    build) build ;;
    sign) stop_local; sign ;;
    local) build; run_local ;;
    start) run_local ;;
    permissions) LAN_MOUSE_DEV_PERMISSIONS=1 run_local ;;
    deploy) deploy ;;
    remote) deploy; run_remote ;;
    all) build; deploy; run_remote; run_local ;;
    stop) stop_local ;;
    logs) tail -n 80 "$dev_dir/stderr.log" ;;
    *) echo "Usage: bash scripts/macos-dev.sh {build|sign|local|start|permissions|deploy|remote|all|stop|logs}" >&2; exit 2 ;;
esac
