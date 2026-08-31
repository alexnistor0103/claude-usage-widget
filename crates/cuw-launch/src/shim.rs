//! The launch shim. A terminal is started on this script, not on `claude`, so
//! the CLI token never reaches a process argument: the script redeems the
//! single-use nonce over localhost itself, puts the token in its own
//! environment and runs `claude` (SWITCHER §4).
//!
//! It is generated into the daemon's data dir and **holds no secret** — the
//! localhost gate it sends is read at run time from `bearer.token` beside its
//! own directory, so nothing sensitive is written here and nothing sensitive is
//! passed on a command line.

use std::path::{Path, PathBuf};

use crate::LaunchError;

/// Subdirectory of the data dir the scripts live in. One level down, so
/// `bearer.token` is reachable as `<script dir>/../bearer.token`.
pub const DIR: &str = "shim";
pub const PS1_NAME: &str = "session-shim.ps1";
pub const SH_NAME: &str = "session-shim.sh";

/// Where the generated scripts ended up.
#[derive(Debug, Clone)]
pub struct Shims {
    pub dir: PathBuf,
    pub powershell: PathBuf,
    pub posix: PathBuf,
}

/// Write both shims under `<data_dir>/shim`, replacing anything that differs
/// from this build's copy. Cheap enough to call on every launch, which is also
/// what repairs a hand-edited or truncated script.
pub fn ensure(data_dir: &Path) -> Result<Shims, LaunchError> {
    let dir = data_dir.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|e| shim_err(&dir, e))?;

    let powershell = dir.join(PS1_NAME);
    write_if_changed(&powershell, PS1, false)?;
    let posix = dir.join(SH_NAME);
    write_if_changed(&posix, SH, true)?;

    Ok(Shims {
        dir,
        powershell,
        posix,
    })
}

fn write_if_changed(path: &Path, content: &str, executable: bool) -> Result<(), LaunchError> {
    if std::fs::read_to_string(path).ok().as_deref() != Some(content) {
        std::fs::write(path, content).map_err(|e| shim_err(path, e))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| shim_err(path, e))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

fn shim_err(path: &Path, e: std::io::Error) -> LaunchError {
    LaunchError::Shim(format!("{}: {e}", path.display()))
}

/// Windows. Started as
/// `powershell.exe -NoProfile -ExecutionPolicy Bypass -NoExit -File <this> …`.
pub const PS1: &str = r##"# Redeems a one-time session code for a CLI token, then starts `claude` with it.
# Written by cuw-daemon under its data dir. Holds no secret (SWITCHER §4).
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Nonce,
    [Parameter(Mandatory = $true)][int]$Port,
    [Parameter(Mandatory = $true)][string]$Cwd
)

$ErrorActionPreference = 'Stop'

function Fail($message) {
    Write-Host "cuw: $message" -ForegroundColor Red
}

# Anything else binding an identity outranks the injected token, and the session
# would quietly run as the wrong account.
$env:ANTHROPIC_API_KEY = $null
$env:ANTHROPIC_AUTH_TOKEN = $null
$env:CLAUDE_CODE_USE_BEDROCK = $null
$env:CLAUDE_CODE_USE_VERTEX = $null

if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Fail 'the Claude Code CLI is not on PATH.'
    return
}

if (-not (Test-Path -LiteralPath $Cwd)) {
    Fail "no such directory: $Cwd"
    return
}

# The localhost gate, not a Claude token. It sits one level up from this script.
$bearerPath = Join-Path (Split-Path -Parent $PSScriptRoot) 'bearer.token'
try {
    $bearer = (Get-Content -Raw -LiteralPath $bearerPath).Trim()
} catch {
    Fail "could not read $bearerPath - is the widget running?"
    return
}

$call = @{
    Method     = 'Get'
    Uri        = "http://127.0.0.1:$Port/session/$Nonce"
    Headers    = @{ Authorization = "Bearer $bearer" }
    TimeoutSec = 15
}
try {
    $resp = Invoke-RestMethod @call
} catch {
    # Never print the error record: a response body can carry a token.
    Fail 'the session code was refused. It is single-use and expires quickly - use the button again.'
    Remove-Variable call, bearer -ErrorAction SilentlyContinue
    $Error.Clear()
    return
}

$token = $resp.token
if ([string]::IsNullOrWhiteSpace($token)) {
    Fail 'the widget returned no session token.'
    Remove-Variable call, bearer, resp -ErrorAction SilentlyContinue
    return
}

$env:CLAUDE_CODE_OAUTH_TOKEN = $token

# The console stays open after `claude` exits, so leave nothing readable behind
# except the environment variable the session actually needs.
Remove-Variable token, resp, bearer, call -ErrorAction SilentlyContinue
$Error.Clear()

Set-Location -LiteralPath $Cwd
claude
"##;

/// macOS/Linux. Started as `<shell> <this> <nonce> <port> <cwd>` — on macOS by
/// the per-launch `.command` wrapper, because `open` forwards no arguments to a
/// document (`macos::write_wrapper`, SWITCHER §5).
pub const SH: &str = r##"#!/bin/sh
# Redeems a one-time session code for a CLI token, then starts `claude` with it.
# Written by cuw-daemon under its data dir. Holds no secret (SWITCHER §4).
set -eu

if [ $# -lt 3 ]; then
    echo "cuw: usage: session-shim.sh <nonce> <port> <cwd>" >&2
    exit 2
fi
nonce=$1
port=$2
cwd=$3

# Anything else binding an identity outranks the injected token, and the session
# would quietly run as the wrong account.
unset CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN || true
unset CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX || true

# Terminal.app runs a .command in a login shell, but an override terminal need
# not, and the CLI's own installer puts it somewhere PATH may not reach: try the
# well-known locations before giving up (SWITCHER §8, Q4).
if command -v claude >/dev/null 2>&1; then
    claude_bin=claude
else
    claude_bin=
    for candidate in \
        "${HOME:-}/.claude/local/claude" \
        "${HOME:-}/.local/bin/claude" \
        /opt/homebrew/bin/claude \
        /usr/local/bin/claude
    do
        if [ -x "$candidate" ]; then
            claude_bin=$candidate
            break
        fi
    done
fi
if [ -z "$claude_bin" ]; then
    echo "cuw: cannot find the Claude Code CLI - not on PATH, nor where it is usually installed." >&2
    exit 1
fi
if [ ! -d "$cwd" ]; then
    echo "cuw: no such directory: $cwd" >&2
    exit 1
fi

# The localhost gate, not a Claude token. It sits one level up from this script.
here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if ! bearer=$(cat "$here/bearer.token" 2>/dev/null); then
    echo "cuw: could not read $here/bearer.token - is the widget running?" >&2
    exit 1
fi

# curl is configured on stdin so neither the localhost gate nor the session code
# reaches the process list.
if ! body=$(printf 'url = "http://127.0.0.1:%s/session/%s"\nheader = "Authorization: Bearer %s"\nmax-time = 15\n' "$port" "$nonce" "$bearer" | curl -fsS -K -); then
    echo "cuw: the session code was refused. It is single-use and expires quickly - use the button again." >&2
    exit 1
fi
unset bearer

token=$(printf '%s' "$body" | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
unset body
if [ -z "$token" ]; then
    echo "cuw: the widget returned no session token." >&2
    exit 1
fi

CLAUDE_CODE_OAUTH_TOKEN=$token
export CLAUDE_CODE_OAUTH_TOKEN
unset token

cd "$cwd"
exec "$claude_bin"
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SCRUBBED_ENV;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cuw-shim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    #[test]
    fn ensure_writes_both_shims_one_level_below_the_data_dir() {
        let root = temp_root("write");
        let shims = ensure(&root).expect("ensure");
        assert_eq!(shims.powershell, root.join(DIR).join(PS1_NAME));
        assert_eq!(shims.posix, root.join(DIR).join(SH_NAME));
        assert!(shims.powershell.is_file());
        assert!(shims.posix.is_file());
        // `<script dir>/../bearer.token` is the daemon's own gate file.
        assert_eq!(shims.dir.parent(), Some(root.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_repairs_a_tampered_shim_and_is_otherwise_a_no_op() {
        let root = temp_root("repair");
        let shims = ensure(&root).expect("ensure");
        std::fs::write(&shims.powershell, "# clobbered").expect("clobber");
        ensure(&root).expect("second ensure");
        assert_eq!(
            std::fs::read_to_string(&shims.powershell).expect("read"),
            PS1
        );
        // Repeating with matching content leaves the bytes alone.
        ensure(&root).expect("third ensure");
        assert_eq!(
            std::fs::read_to_string(&shims.powershell).expect("read"),
            PS1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_shims_hold_no_secret() {
        for src in [PS1, SH] {
            assert!(!src.contains("sk-ant"), "a token literal in a shim");
            // The gate is read at run time, never baked in.
            assert!(src.contains("bearer.token"));
        }
    }

    #[test]
    fn the_shims_scrub_every_conflicting_identity() {
        for key in SCRUBBED_ENV {
            // The PowerShell shim sets CLAUDE_CODE_OAUTH_TOKEN rather than
            // clearing it; the other four are cleared in both.
            assert!(PS1.contains(key), "{key} unhandled in the ps1 shim");
            assert!(SH.contains(key), "{key} unhandled in the sh shim");
        }
    }

    #[test]
    fn a_failed_redemption_never_prints_the_response() {
        // `$_` in the catch would surface the response body.
        assert!(!PS1.contains("$_"), "the ps1 shim echoes the error record");
        assert!(PS1.contains("$Error.Clear()"));
        // Nothing readable is left behind in a console that stays open.
        assert!(PS1.contains("Remove-Variable token"));
    }

    #[test]
    fn the_posix_shim_looks_past_path_for_the_cli() {
        for known in [
            "/.claude/local/claude",
            "/.local/bin/claude",
            "/opt/homebrew/bin/claude",
            "/usr/local/bin/claude",
        ] {
            assert!(SH.contains(known), "{known} is not tried");
        }
        // `set -eu` is on, so an unset HOME must not abort the loop.
        assert!(
            SH.contains("${HOME:-}"),
            "an unset HOME would abort the shim"
        );
        assert!(
            SH.contains(r#"exec "$claude_bin""#),
            "the fallback is unused"
        );
    }

    #[test]
    fn the_posix_shim_keeps_the_gate_off_the_process_list() {
        assert!(SH.contains("curl -fsS -K -"), "curl args carry the header");
        assert!(!SH.contains("-H \"Authorization"), "header on the argv");
    }

    /// Parses the script the way PowerShell would; it is never executed here.
    /// Silently skipped where `powershell.exe` is unavailable.
    #[cfg(windows)]
    #[test]
    fn the_powershell_shim_parses() {
        let root = temp_root("psparse");
        let shims = ensure(&root).expect("ensure");
        let script = format!(
            "$errs = $null; \
             [void][System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$errs); \
             if ($errs.Count -gt 0) {{ $errs | ForEach-Object {{ Write-Output $_.Message }}; exit 1 }}",
            shims.powershell.display()
        );
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output();
        let _ = std::fs::remove_dir_all(&root);
        let Ok(out) = out else {
            return;
        };
        assert!(
            out.status.success(),
            "the ps1 shim does not parse:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_posix_shim_parses() {
        let root = temp_root("shparse");
        let shims = ensure(&root).expect("ensure");
        let out = std::process::Command::new("sh")
            .arg("-n")
            .arg(&shims.posix)
            .output();
        let _ = std::fs::remove_dir_all(&root);
        let Ok(out) = out else {
            return;
        };
        assert!(
            out.status.success(),
            "the sh shim does not parse:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
