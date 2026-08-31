# Release build (D3/M6.8): the daemon FIRST, then the overlay bundle that
# carries it, then the sibling copy for the un-bundled exe. Needs the Tauri
# CLI: cargo install tauri-cli.
#
# The daemon is declared as a bundle resource HERE rather than in
# tauri.conf.json, because tauri-build validates resource paths inside the
# overlay's build script — a checked-in entry would make a plain `cargo check`
# of the overlay fail on any machine that has not release-built the daemon.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# A running daemon locks its own exe. The overlay respawns it within seconds,
# so it goes first.
foreach ($name in 'cuw-overlay', 'cuw-daemon') {
    try { Get-Process $name -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue } catch {}
}

Write-Host '== cargo build --release -p cuw-daemon =='
Push-Location $root
try {
    cargo build --release -p cuw-daemon
    if ($LASTEXITCODE -ne 0) { throw "daemon build failed ($LASTEXITCODE)" }
} finally { Pop-Location }

$daemonExe = Join-Path $root 'target\release\cuw-daemon.exe'
if (-not (Test-Path $daemonExe)) { throw "missing $daemonExe" }

Write-Host '== cargo tauri build =='
# Inline JSON does not survive PowerShell 5.1's native-arg quoting; --config
# also takes a path, so the resource declaration goes through a scratch file.
$resources = '{"bundle":{"resources":{"../../../target/release/cuw-daemon.exe":"cuw-daemon.exe"}}}'
$configPath = Join-Path ([IO.Path]::GetTempPath()) 'cuw-bundle-resources.json'
[IO.File]::WriteAllText($configPath, $resources)
Push-Location (Join-Path $root 'apps\overlay\src-tauri')
try {
    cargo tauri build --config $configPath
    if ($LASTEXITCODE -ne 0) { throw "overlay build failed ($LASTEXITCODE)" }
} finally { Pop-Location }

# The bundle carries the daemon; the plain exe needs the sibling copied by hand.
$overlayRelease = Join-Path $root 'apps\overlay\src-tauri\target\release'
Copy-Item $daemonExe (Join-Path $overlayRelease 'cuw-daemon.exe') -Force

Write-Host ''
Write-Host 'Artefacts:'
Write-Host ("  overlay exe : {0}" -f (Join-Path $overlayRelease 'cuw-overlay.exe'))
Write-Host ("  daemon exe  : {0}" -f (Join-Path $overlayRelease 'cuw-daemon.exe'))
$nsis = Join-Path $overlayRelease 'bundle\nsis'
if (Test-Path $nsis) {
    Get-ChildItem $nsis -Filter *.exe | ForEach-Object { Write-Host ("  installer   : {0}" -f $_.FullName) }
} else {
    Write-Host ("  installer   : (none under {0})" -f $nsis)
}
