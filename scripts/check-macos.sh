#!/bin/sh
# Type-checks the whole app for macOS. See check-macos.ps1 for why the
# objc2-exception-helper stub is patched in; on a real Mac it is unnecessary,
# so pass --native to use the real crate.
set -eu

target=${TARGET:-aarch64-apple-darwin}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
verb=check
patch="--config patch.crates-io.objc2-exception-helper.path=\"$root/scripts/macos-check/objc2-exception-helper\""

for arg in "$@"; do
    case $arg in
        --clippy) verb=clippy ;;
        --native) patch= ;;
        *) echo "usage: check-macos.sh [--clippy] [--native]" >&2; exit 2 ;;
    esac
done

# The stub is patched into both workspaces, so `--native` means the same thing
# in each. The root workspace does not enable objc2's `exception` feature today,
# so cargo answers with "patch ... was not used in the crate graph" there; that
# warning is expected and is not a failure.
for dir in "$root" "$root/apps/overlay/src-tauri"; do
    cd "$dir"
    # shellcheck disable=SC2086
    eval cargo "$verb" --target "$target" --all-targets --target-dir target/macos-check $patch
done

echo "macOS $verb clean ($target)"
