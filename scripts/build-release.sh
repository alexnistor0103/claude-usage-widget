#!/bin/sh
# Release build (D3/M6.8), the macOS twin of build-release.ps1: the daemon
# FIRST, then the overlay bundle that carries it, then the sibling copy for the
# un-bundled binary. Needs the Tauri CLI: cargo install tauri-cli.
#
# --universal builds a fat (x86_64 + arm64) daemon and app, so one .dmg runs
# on Intel and Apple Silicon alike. Needs both rustup targets; the script adds
# them itself.
#
# The daemon is declared as a bundle resource HERE rather than in
# tauri.conf.json, because tauri-build validates resource paths inside the
# overlay's build script - a checked-in entry would make a plain `cargo check`
# of the overlay (and scripts/check-macos.ps1) fail on any machine that has not
# release-built the daemon.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
overlay=$root/apps/overlay/src-tauri

universal=
for arg in "$@"; do
    case $arg in
        --universal) universal=yes ;;
        *) echo "usage: build-release.sh [--universal]" >&2; exit 2 ;;
    esac
done

if [ -n "$universal" ]; then
    release=$overlay/target/universal-apple-darwin/release
else
    release=$overlay/target/release
fi
app=$release/bundle/macos/cuw-overlay.app

case $(uname -s) in
    Darwin) ;;
    *)
        echo "build-release.sh builds the macOS app; use scripts/build-release.ps1 on Windows." >&2
        exit 2
        ;;
esac

# The overlay first: it respawns the daemon within seconds of losing it, and a
# running daemon owns the port and the data dir the new build will want.
pkill -x cuw-overlay >/dev/null 2>&1 || true
pkill -x cuw-daemon >/dev/null 2>&1 || true

daemon=$root/target/release/cuw-daemon
if [ -n "$universal" ]; then
    # Per-arch builds lipo'd into the path the resource declaration below
    # already points at, so the tauri invocation stays identical.
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        echo "== cargo build --release -p cuw-daemon --target $t =="
        ( cd "$root" && cargo build --release -p cuw-daemon --target "$t" )
    done
    mkdir -p "$root/target/release"
    lipo -create -output "$daemon"         "$root/target/aarch64-apple-darwin/release/cuw-daemon"         "$root/target/x86_64-apple-darwin/release/cuw-daemon"
else
    echo '== cargo build --release -p cuw-daemon =='
    ( cd "$root" && cargo build --release -p cuw-daemon )
fi

if [ ! -f "$daemon" ]; then
    echo "missing $daemon" >&2
    exit 1
fi

# Notarization rejects any unsigned executable in the bundle, and the daemon
# rides along as a resource the bundler does not sign. Hardened runtime and a
# timestamp are both notarization requirements.
if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
    echo '== codesign cuw-daemon =='
    codesign --force --options runtime --timestamp \
        --sign "$APPLE_SIGNING_IDENTITY" "$daemon"
fi

# Inside a .app the executable is in Contents/MacOS while a bundle resource
# lands in Contents/Resources, which is exactly where the overlay's
# `bundled_daemon` looks - so the resource needs no copy afterwards and a .dmg
# built in the same run carries a complete app.
echo '== cargo tauri build =='
(
    cd "$overlay" && cargo tauri build \
        ${universal:+--target universal-apple-darwin} --config \
        '{"bundle":{"resources":{"../../../target/release/cuw-daemon":"cuw-daemon"}}}'
)

# The un-bundled binary has no Resources dir, so it needs the sibling copy.
cp "$daemon" "$release/cuw-daemon"

echo ''
echo 'Artefacts:'
echo "  overlay exe : $release/cuw-overlay"
echo "  daemon exe  : $release/cuw-daemon"
if [ -d "$app" ]; then
    echo "  app bundle  : $app"
else
    echo "  app bundle  : (none under $release/bundle/macos)"
fi
found_dmg=
for dmg in "$release"/bundle/dmg/*.dmg; do
    if [ -f "$dmg" ]; then
        echo "  installer   : $dmg"
        found_dmg=yes
    fi
done
if [ -z "$found_dmg" ]; then
    echo "  installer   : (no .dmg; the bundler needs hdiutil)"
fi
