<#
.SYNOPSIS
    Type-checks the app for macOS from a machine that is not a Mac.

.DESCRIPTION
    `cargo check` never links, so an Apple target only needs the Rust std for it
    (`rustup target add aarch64-apple-darwin`). The one thing that does not
    cross is `objc2-exception-helper`, whose build script compiles a `.m` file
    and so wants clang plus the macOS SDK; it is swapped for the never-linked
    stub in scripts/macos-check/ via a command-line `[patch]`, which leaves
    Cargo.toml and Cargo.lock untouched.

    This is a compiler, not a Mac: it proves the macOS code typechecks, never
    that it behaves. Runtime claims still need a run on a real Mac.

.EXAMPLE
    scripts/check-macos.ps1                      # workspace + overlay
    scripts/check-macos.ps1 -Package cuw-tracker # one crate, no overlay
    scripts/check-macos.ps1 -OverlayOnly -Clippy
#>
[CmdletBinding()]
param(
    [string]$Target = 'aarch64-apple-darwin',
    [switch]$Clippy,
    [switch]$SkipOverlay,
    [switch]$OverlayOnly,
    # One workspace crate instead of all of them; implies -SkipOverlay. Use this
    # while someone else is mid-edit in another crate.
    [string]$Package,
    [string]$TargetDir = 'target/macos-check'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$stub = Join-Path $root 'scripts\macos-check\objc2-exception-helper'
# A TOML literal string: a Windows path must not have its backslashes read as
# escapes.
$patch = "patch.crates-io.objc2-exception-helper.path='$stub'"
$verb = if ($Clippy) { 'clippy' } else { 'check' }
$failed = @()

function Invoke-Check([string]$Where, [string[]]$CargoArgs) {
    Write-Host "== $verb $Target :: $Where" -ForegroundColor Cyan
    Push-Location $Where
    try {
        & cargo $CargoArgs
        if ($LASTEXITCODE -ne 0) { $script:failed += $Where }
    } finally { Pop-Location }
}

if (-not (rustup target list --installed | Select-String -SimpleMatch $Target)) {
    Write-Host "install the target first: rustup target add $Target" -ForegroundColor Yellow
    exit 2
}

# The root workspace does not enable objc2's `exception` feature today, so the
# stub goes unused there and cargo says so; that warning is expected, not a
# failure. It is still patched in, so enabling the feature cannot break this.
if (-not $OverlayOnly) {
    $cargoArgs = @($verb, '--target', $Target, '--all-targets', '--target-dir', $TargetDir, '--config', $patch)
    if ($Package) { $cargoArgs += @('-p', $Package) }
    Invoke-Check $root $cargoArgs
}

# The overlay is its own workspace, so it needs its own invocation.
if (-not $SkipOverlay -and -not $Package) {
    Invoke-Check (Join-Path $root 'apps\overlay\src-tauri') @(
        $verb, '--target', $Target, '--all-targets',
        '--target-dir', $TargetDir, '--config', $patch)
}

if ($failed.Count -gt 0) {
    Write-Host "macOS $verb FAILED in: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "macOS $verb clean ($Target)" -ForegroundColor Green
