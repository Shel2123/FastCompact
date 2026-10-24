#!/usr/bin/env bash
# Builds, signs and launches the stage 0 spike as "Orbit Spike.app".
#
# Usage: scripts/bundle-spike.sh [--build-only]
#
# Environment:
#   ORBIT_SIGN_IDENTITY        codesign identity (default "-", ad-hoc)
#   ORBIT_SPIKE_HIT_DELAY_MS   delay before the AX hit-test
#   ORBIT_SPIKE_ATTR_DELAY_MS  delay between hit-test and attribute walk
#   ORBIT_SPIKE_BLOCK_MAIN_MS  block the main thread this long every 6 s
#   ORBIT_SPIKE_POINT          "tap" to use the event-tap location
# All ORBIT_SPIKE_* variables are passed to the app through `open --env`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE_ID="com.example.orbit.spike"
APP="$ROOT/target/spike/Orbit Spike.app"
EXE_PATTERN="Orbit Spike.app/Contents/MacOS/spike"
IDENTITY="${ORBIT_SIGN_IDENTITY:--}"
LOG_DIR="$HOME/Library/Logs/Orbit"
BUILD_ONLY=0
[[ "${1:-}" == "--build-only" ]] && BUILD_ONLY=1

# 1. Stop the running instance and wait until it is gone, so the next launch
#    is the new build and not the old process. SIGTERM avoids the Automation
#    prompt that an AppleScript "quit" from the terminal would trigger.
if pids="$(pgrep -f "$EXE_PATTERN")"; then
    echo "stopping running spike: $(echo "$pids" | tr '\n' ' ')"
    pkill -TERM -f "$EXE_PATTERN"
    for _ in $(seq 1 100); do
        pgrep -f "$EXE_PATTERN" >/dev/null || break
        sleep 0.1
    done
    if pgrep -f "$EXE_PATTERN" >/dev/null; then
        echo "spike still running after 10 s; quit it from the menu bar and retry" >&2
        exit 1
    fi
fi

# 2. Build with a fresh marker so the log proves which build is running.
BUILD="$(date -u +%Y%m%dT%H%M%SZ)-$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo nogit)"
echo "building spike $BUILD"
ORBIT_SPIKE_BUILD="$BUILD" cargo build --manifest-path "$ROOT/Cargo.toml" -p orbit-macos --example spike

# 3. Assemble and sign the bundle.
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$ROOT/resources/Spike-Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/debug/examples/spike" "$APP/Contents/MacOS/spike"
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
codesign --verify --strict --verbose=2 "$APP"
echo "designated requirement:"
codesign -d -r- "$APP" 2>&1 | sed -n 's/^#* *designated => /  /p'
if [[ "$IDENTITY" == "-" ]]; then
    echo "note: ad-hoc signature; Accessibility access may reset on every rebuild."
    echo "      Apple Development identities in the keychain:"
    security find-identity -v -p codesigning | grep "Apple Development" || echo "      (none)"
fi

[[ "$BUILD_ONLY" == 1 ]] && exit 0

# 4. Launch exactly one instance.
if pgrep -f "$EXE_PATTERN" >/dev/null; then
    echo "a spike instance appeared during the build; not launching" >&2
    exit 1
fi
mkdir -p "$LOG_DIR"
open_args=(--stdout "$LOG_DIR/spike.stdout.log" --stderr "$LOG_DIR/spike.stderr.log")
while IFS= read -r var; do
    [[ "$var" == ORBIT_SPIKE_BUILD=* ]] && continue
    open_args+=(--env "$var")
done < <(env | grep '^ORBIT_SPIKE_' || true)
open "${open_args[@]}" "$APP"
echo "launched; log: $LOG_DIR/spike.log"
