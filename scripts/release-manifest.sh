#!/bin/sh
# Turns a directory of downloaded release artefacts into what a GitHub release
# needs for in-app updates: stable asset names (GitHub rewrites spaces in
# names, so the URLs written into the manifest would not match otherwise) and
# the `latest.json` the updater plugin reads (update.rs). Without signatures
# there is no manifest: an unsigned release is still installable by hand, the
# updater just never offers it.
#
#   release-manifest.sh <dir> <tag>
set -eu

dir=${1:?dir}
tag=${2:?tag}
repo=alexnistor0103/claude-usage-widget
version=${tag#v}
base="https://github.com/$repo/releases/download/$tag"

rename() {
    src=$1; dst=$2
    [ -e "$src" ] || return 0
    [ "$src" = "$dst" ] || mv "$src" "$dst"
}

cd "$dir"
# One of each is expected; a glob that matches nothing is left alone.
for f in *-setup.exe; do
    [ -f "$f" ] && rename "$f" "ClaudeUsageWidget_${version}_windows-x64-setup.exe"
done
for f in *-setup.exe.sig; do
    [ -f "$f" ] && rename "$f" "ClaudeUsageWidget_${version}_windows-x64-setup.exe.sig"
done
for f in *.dmg; do
    [ -f "$f" ] && rename "$f" "ClaudeUsageWidget_${version}_macos-universal.dmg"
done
for f in *.app.tar.gz; do
    [ -f "$f" ] && rename "$f" "ClaudeUsageWidget_${version}_macos-universal.app.tar.gz"
done
for f in *.app.tar.gz.sig; do
    [ -f "$f" ] && rename "$f" "ClaudeUsageWidget_${version}_macos-universal.app.tar.gz.sig"
done

win="ClaudeUsageWidget_${version}_windows-x64-setup.exe"
mac="ClaudeUsageWidget_${version}_macos-universal.app.tar.gz"
platforms=
if [ -f "$win.sig" ]; then
    platforms="\"windows-x86_64\":{\"signature\":\"$(cat "$win.sig")\",\"url\":\"$base/$win\"}"
fi
if [ -f "$mac.sig" ]; then
    sig=$(cat "$mac.sig")
    # One universal tarball serves both; the plugin looks up target-arch only.
    for arch in aarch64 x86_64; do
        entry="\"darwin-$arch\":{\"signature\":\"$sig\",\"url\":\"$base/$mac\"}"
        platforms="${platforms:+$platforms,}$entry"
    done
fi
if [ -z "$platforms" ]; then
    echo "no updater signatures found; latest.json not written" >&2
    ls -l
    exit 0
fi
pub_date=$(date -u +%Y-%m-%dT%H:%M:%SZ)
printf '{"version":"%s","notes":"Release %s","pub_date":"%s","platforms":{%s}}\n' \
    "$version" "$tag" "$pub_date" "$platforms" > latest.json
ls -l
