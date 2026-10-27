#!/usr/bin/env bash
# Builds, signs and launches Orbit as "Orbit.app".
#
# Usage: scripts/bundle.sh [--build-only] [--release]
#
# Environment:
#   ORBIT_SIGN_IDENTITY           codesign identity (default "-", ad-hoc; an
#                                 Apple Development identity keeps the
#                                 Accessibility grant across rebuilds)
# Debug builds only (docs/stage-4.md):
#   ORBIT_DEBUG_RESOLVE_DELAY_MS  the resolver waits this long before each read
#   ORBIT_DEBUG_MAIN_DELAY_MS     the main thread sleeps this long before it
#                                 handles a press
#   ORBIT_DEBUG_SECURE_PULSE      "1": secure keyboard entry on 200 ms after
#                                 each press, off 500 ms later
#   ORBIT_DEBUG_NO_POLL           "1": the event tap runs without its watchdog
# All ORBIT_DEBUG_* variables are passed to the app through `open --env`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE_ID="com.example.orbit"
APP="$ROOT/target/bundle/Orbit.app"
EXE_PATTERN="Orbit.app/Contents/MacOS/orbit"
IDENTITY="${ORBIT_SIGN_IDENTITY:--}"
LOG_DIR="$HOME/Library/Logs/Orbit"
BUILD_ONLY=0
PROFILE=debug
for arg in "$@"; do
    case "$arg" in
        --build-only) BUILD_ONLY=1 ;;
        --release) PROFILE=release ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# 1. Stop the running instance and wait until it is gone, so the next launch
#    is the new build. SIGTERM avoids the Automation prompt of an AppleScript
#    "quit" from the terminal.
if pids="$(pgrep -f "$EXE_PATTERN")"; then
    echo "stopping running Orbit: $(echo "$pids" | tr '\n' ' ')"
    pkill -TERM -f "$EXE_PATTERN"
    for _ in $(seq 1 100); do
        pgrep -f "$EXE_PATTERN" >/dev/null || break
        sleep 0.1
    done
    if pgrep -f "$EXE_PATTERN" >/dev/null; then
        echo "Orbit still running after 10 s; quit it from the menu bar and retry" >&2
        exit 1
    fi
fi

# 2. Build.
cargo_args=(build --manifest-path "$ROOT/Cargo.toml" -p orbit-desktop)
[[ "$PROFILE" == release ]] && cargo_args+=(--release)
cargo "${cargo_args[@]}"

# 3. Assemble and sign the bundle.
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$ROOT/resources/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/$PROFILE/orbit" "$APP/Contents/MacOS/orbit"
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

# 4. Launch. A second instance would exit on its own (instance lock).
mkdir -p "$LOG_DIR"
open_args=(--stdout "$LOG_DIR/orbit.stdout.log" --stderr "$LOG_DIR/orbit.stderr.log")
while IFS= read -r var; do
    open_args+=(--env "$var")
done < <(env | grep '^ORBIT_DEBUG_' || true)
open "${open_args[@]}" "$APP"
echo "launched; log: $LOG_DIR/orbit.log"
