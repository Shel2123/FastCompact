#!/usr/bin/env bash
# Checks that the platform-independent crates stay independent of macOS.
#
# 1. They compile, tests included, for a non-macOS target.
# 2. Their dependency tree has no macOS-only crates.
# 3. Their sources do not name macOS APIs or `target_os`.
#
# It compiles only. Behavior on another system is checked when a port exists.

set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${ORBIT_PORTABLE_TARGET:-x86_64-pc-windows-msvc}"
CRATES=(orbit-domain orbit-app orbit-runtime)
PACKAGES=()
DIRS=()
for crate in "${CRATES[@]}"; do
    PACKAGES+=(-p "$crate")
    DIRS+=("crates/$crate")
done

if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "target $TARGET is not installed: rustup target add $TARGET" >&2
    exit 1
fi

echo "== cargo check ($TARGET)"
cargo check --target "$TARGET" "${PACKAGES[@]}" --all-targets --all-features
echo "== cargo clippy ($TARGET)"
cargo clippy --target "$TARGET" "${PACKAGES[@]}" --all-targets --all-features -- -D warnings

echo "== dependency trees"
status=0
for crate in "${CRATES[@]}"; do
    tree="$(cargo tree --target "$TARGET" -p "$crate" -e normal,build,dev --prefix none)"
    if hits="$(grep -E '^(objc2|dispatch2|core-foundation|global-hotkey|block2)' <<<"$tree")"; then
        echo "$crate depends on macOS crates:" >&2
        echo "$hits" >&2
        status=1
    fi
done

echo "== sources"
if rg -n 'objc2|dispatch2|target_os|cfg\(.*macos|core_foundation' "${DIRS[@]}"; then
    echo "macOS names found in platform-independent crates" >&2
    status=1
fi

if [ "$status" -ne 0 ]; then
    exit "$status"
fi
echo "portable: ok"
