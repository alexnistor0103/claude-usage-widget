# End-to-end live test for cuw (I3). Drives the daemon over localhost and
# prints a PASS/FAIL/SKIP table. Never prints a secret: the bearer stays in
# function locals, responses are scanned in memory, and the script's own output
# is checked for leaks as the last step.
#
#   powershell -NoProfile -File scripts\e2e-live.ps1 -SkipLive   # no browser
#   powershell -NoProfile -File scripts\e2e-live.ps1             # one live connect
#
# Rules: stops the overlay and cuw-daemon, never a
# `claude` process; never touches %USERPROFILE%\.claude; never reads a keyring
# value.
#
# The whole run is isolated (STATUS, 2026-08-31): its own data dir, its own
# keyring namespace and its own port, so it can neither read nor rewrite the
# real registry.toml or a real credential. Two daemons sharing one data dir is
# what cost two accounts.

param(
    [switch]$SkipLive,
    [switch]$Reconnect,
    # Not 8787: nothing here may land on the port the real widget uses.
    [int]$Port = 8799
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http

$root = Split-Path -Parent $PSScriptRoot
$tempDir = Join-Path $env:TEMP 'cuw-e2e'
New-Item -ItemType Directory -Force $tempDir | Out-Null
$outFile = Join-Path $tempDir 'e2e-output.log'
Set-Content -Path $outFile -Value '' -Encoding utf8

# `directories::ProjectDirs` resolves %APPDATA% through SHGetKnownFolderPath, so
# these two overrides are the only way to keep a test daemon off the real
# registry and the real credentials (cuw-daemon startup.rs, cuw-creds lib.rs).
$realDataDir = Join-Path $env:APPDATA 'local\cuw\data'
$dataDir = Join-Path $tempDir 'data'
New-Item -ItemType Directory -Force $dataDir | Out-Null
$configFile = Join-Path $tempDir 'accounts.toml'
Set-Content -Path $configFile -Value "port = $Port" -Encoding utf8
$env:CUW_DATA_DIR = $dataDir
$env:CUW_KEYRING_SERVICE = 'com.local.cuw-e2e'
$env:CUW_CONFIG = $configFile

$script:Port = $Port
$script:Results = @()

function Say($msg) {
    Add-Content -Path $outFile -Value $msg -Encoding utf8
    Write-Host $msg
}

function Report($step, $status, $reason) {
    $script:Results += [pscustomobject]@{ Step = $step; Status = $status; Reason = $reason }
    Say ("{0,-5} {1,-12} {2}" -f $status, $step, $reason)
}

function Read-BearerInto([ref]$slot) {
    # The value never leaves the caller's local; never echo it.
    $path = Join-Path $dataDir 'bearer.token'
    if (-not (Test-Path $path)) { throw "no bearer file at $path" }
    $slot.Value = (Get-Content -Raw $path).Trim()
}

function Invoke-Cuw($Method, $Path, $Body, [switch]$NoAuth) {
    $bearerLocal = $null
    if (-not $NoAuth) { Read-BearerInto ([ref]$bearerLocal) }
    $client = New-Object System.Net.Http.HttpClient
    try {
        $client.Timeout = [TimeSpan]::FromSeconds(30)
        $uri = "http://127.0.0.1:$($script:Port)$Path"
        $req = New-Object System.Net.Http.HttpRequestMessage(
            (New-Object System.Net.Http.HttpMethod($Method)), $uri)
        if ($null -ne $bearerLocal) {
            $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $bearerLocal)
        }
        if ($null -ne $Body) {
            $req.Content = New-Object System.Net.Http.StringContent($Body, [Text.Encoding]::UTF8, 'application/json')
        }
        $resp = $client.SendAsync($req).Result
        $text = $resp.Content.ReadAsStringAsync().Result
        return [pscustomobject]@{ Status = [int]$resp.StatusCode; Body = $text }
    } finally {
        $client.Dispose()
        Remove-Variable bearerLocal -ErrorAction SilentlyContinue
    }
}

# Reads /events for up to $Seconds, returns the raw text received. When
# $PostPath is given, that POST is started first and both run concurrently
# (a connect POST only answers when the flow ends).
function Read-Sse($Seconds, $PostPath, $PostBody) {
    $bearerLocal = $null
    Read-BearerInto ([ref]$bearerLocal)
    $client = New-Object System.Net.Http.HttpClient
    $postClient = $null
    $postTask = $null
    try {
        $client.Timeout = [TimeSpan]::FromSeconds($Seconds + 30)
        $req = New-Object System.Net.Http.HttpRequestMessage(
            (New-Object System.Net.Http.HttpMethod('GET')), "http://127.0.0.1:$($script:Port)/events")
        $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $bearerLocal)
        $resp = $client.SendAsync($req, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).Result
        if ([int]$resp.StatusCode -ne 200) { return [pscustomobject]@{ Ok = $false; Text = ''; PostStatus = $null } }
        $stream = $resp.Content.ReadAsStreamAsync().Result

        if ($null -ne $PostPath) {
            $postClient = New-Object System.Net.Http.HttpClient
            $postClient.Timeout = [TimeSpan]::FromSeconds($Seconds + 30)
            $preq = New-Object System.Net.Http.HttpRequestMessage(
                (New-Object System.Net.Http.HttpMethod('POST')), "http://127.0.0.1:$($script:Port)$PostPath")
            $preq.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $bearerLocal)
            if ($null -ne $PostBody) {
                $preq.Content = New-Object System.Net.Http.StringContent($PostBody, [Text.Encoding]::UTF8, 'application/json')
            }
            $postTask = $postClient.SendAsync($preq)
        }

        $sb = New-Object System.Text.StringBuilder
        $buf = New-Object byte[] 8192
        $deadline = (Get-Date).AddSeconds($Seconds)
        while ((Get-Date) -lt $deadline) {
            $readTask = $stream.ReadAsync($buf, 0, $buf.Length)
            $waitMs = [int]([Math]::Max(250, ($deadline - (Get-Date)).TotalMilliseconds))
            if (-not $readTask.Wait($waitMs)) { break }
            $n = $readTask.Result
            if ($n -le 0) { break }
            [void]$sb.Append([Text.Encoding]::UTF8.GetString($buf, 0, $n))
            # A live connect ends when the validated/failed phase arrives.
            if ($null -ne $PostPath) {
                $t = $sb.ToString()
                if ($t.Contains('"validated"') -or $t.Contains('"failed"')) { break }
            }
        }
        $postStatus = $null
        if ($null -ne $postTask) {
            if ($postTask.Wait(120000)) { $postStatus = [int]$postTask.Result.StatusCode }
        }
        return [pscustomobject]@{ Ok = $true; Text = $sb.ToString(); PostStatus = $postStatus }
    } finally {
        $client.Dispose()
        if ($null -ne $postClient) { $postClient.Dispose() }
        Remove-Variable bearerLocal -ErrorAction SilentlyContinue
    }
}

function Test-Port($port) {
    $tcp = New-Object System.Net.Sockets.TcpClient
    try {
        $t = $tcp.ConnectAsync('127.0.0.1', $port)
        if ($t.Wait(500) -and $tcp.Connected) { return $true }
        return $false
    } catch { return $false } finally { $tcp.Dispose() }
}

function Run-Cargo($step, $arguments, $dir) {
    $slug = ($arguments -join '_') -replace '[^a-z0-9_-]', '_'
    $so = Join-Path $tempDir "$slug.out.log"
    $se = Join-Path $tempDir "$slug.err.log"
    $p = Start-Process -FilePath 'cargo' -ArgumentList $arguments -WorkingDirectory $dir `
        -NoNewWindow -Wait -PassThru -RedirectStandardOutput $so -RedirectStandardError $se
    if ($p.ExitCode -ne 0) {
        Report $step 'FAIL' ("cargo {0} exited {1} (see {2})" -f ($arguments -join ' '), $p.ExitCode, $se)
        return $false
    }
    return $true
}

# The overlay first: it respawns the daemon within seconds of losing it, so
# killing the daemon alone leaves a daemon this script did not start (STATUS).
function Stop-Widget {
    foreach ($name in 'cuw-overlay', 'cuw-daemon') {
        try { Get-Process $name -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue } catch {}
    }
}

# Inherited by every child, so never leave them behind in a shell that will
# later start the real daemon.
function Reset-Env {
    foreach ($n in 'CUW_DATA_DIR', 'CUW_KEYRING_SERVICE', 'CUW_CONFIG') {
        Remove-Item "Env:$n" -ErrorAction SilentlyContinue
    }
}

# --- 1. Preflight -----------------------------------------------------------

Say "== cuw e2e ($(Get-Date -Format s)) =="
Say "isolated: data dir $dataDir, keyring com.local.cuw-e2e, port $Port"
Say 'stopping the overlay and any running daemon'
Stop-Widget
$pre = $true
if (-not (Run-Cargo 'fmt' @('fmt', '--check') $root)) { $pre = $false }
if ($pre) { if (-not (Run-Cargo 'clippy' @('clippy', '--all-targets') $root)) { $pre = $false } }
if ($pre) { if (-not (Run-Cargo 'test' @('test') $root)) { $pre = $false } }
if ($pre) { if (-not (Run-Cargo 'overlay' @('check') (Join-Path $root 'apps\overlay\src-tauri'))) { $pre = $false } }
if ($pre) { Report 'preflight' 'PASS' 'fmt/clippy/test/overlay-check green' }
if (-not $pre) {
    Say 'Preflight failed; aborting.'
    Reset-Env
    exit 1
}

# --- 2. Start the daemon ----------------------------------------------------

$portFile = Join-Path $dataDir 'port'
$pidFile = Join-Path $dataDir 'pid'
if (Test-Path $portFile) { Remove-Item $portFile -Force }
$daemonOut = Join-Path $tempDir 'daemon.out.log'
$daemonErr = Join-Path $tempDir 'daemon.err.log'
$daemon = Start-Process -FilePath 'cargo' -ArgumentList 'run', '-p', 'cuw-daemon' -WorkingDirectory $root `
    -WindowStyle Hidden -PassThru -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonErr

$up = $false
$died = ''
$deadline = (Get-Date).AddSeconds(30)
while ((Get-Date) -lt $deadline) {
    if (Test-Path $portFile) {
        $portText = (Get-Content -Raw $portFile).Trim()
        $parsed = 0
        if ([int]::TryParse($portText, [ref]$parsed)) {
            $script:Port = $parsed
            if (Test-Port $script:Port) { $up = $true; break }
        }
    }
    # A daemon that loses the single-instance race exits 2 before writing the
    # port file (cuw-daemon startup.rs), so waiting out the deadline would only
    # delay the same verdict.
    if ($daemon.HasExited) {
        $died = if ($daemon.ExitCode -eq 2) {
            "the port is already owned by another cuw-daemon - stop the widget first"
        } else {
            "the daemon exited with code $($daemon.ExitCode)"
        }
        break
    }
    Start-Sleep -Milliseconds 500
}
$scratch = Join-Path $dataDir 'scratch'
$scratchClean = $true
if (Test-Path $scratch) {
    if ((Get-ChildItem $scratch -ErrorAction SilentlyContinue | Measure-Object).Count -gt 0) { $scratchClean = $false }
}
if ($up -and (Test-Path $pidFile) -and $scratchClean) {
    Report 'startup' 'PASS' "port $($script:Port), pid file present, scratch clean"
} else {
    $extra = if ($died) { "$died; " } else { '' }
    Report 'startup' 'FAIL' "${extra}up=$up pid=$(Test-Path $pidFile) scratchClean=$scratchClean (see $daemonErr)"
    Stop-Widget
    Reset-Env
    exit 1
}

# --- 3. Auth + wire shape ---------------------------------------------------

$noAuth = Invoke-Cuw 'GET' '/accounts' $null -NoAuth
$withAuth = Invoke-Cuw 'GET' '/accounts' $null
$wireOk = $true
$why = ''
if ($noAuth.Status -ne 401) { $wireOk = $false; $why = "no-bearer GET gave $($noAuth.Status), want 401" }
elseif ($withAuth.Status -ne 200) { $wireOk = $false; $why = "bearer GET gave $($withAuth.Status), want 200" }
elseif ($withAuth.Body -match 'sk-ant') { $wireOk = $false; $why = 'response body contains a token prefix' }
else {
    $accounts = $withAuth.Body | ConvertFrom-Json
    if ($null -eq $accounts) { $accounts = @() }
    $need = @('stale', 'fetched_at', 'scoped', 'access_expires_at', 'refreshed_at', 'refresh', 'persist_pending', 'can_switch')
    foreach ($a in @($accounts)) {
        $names = $a.PSObject.Properties.Name
        foreach ($k in $need) {
            if ($names -notcontains $k) { $wireOk = $false; $why = "row missing '$k'" }
        }
        if ($names -contains 'expires_at') { $wireOk = $false; $why = "row still carries 'expires_at'" }
    }
    if ($wireOk) {
        $count = (@($accounts)).Count
        $why = if ($count -eq 0) {
            '401 without bearer, 200 with; 0 rows - the isolated registry is empty, so the row shape is unchecked'
        } else {
            "401 without bearer, 200 with; $count row(s), wire shape ok"
        }
    }
}
if ($wireOk) { Report 'auth-wire' 'PASS' $why } else { Report 'auth-wire' 'FAIL' $why }

# --- 3b. Session routes (M7.2) ----------------------------------------------
# No live login needed: an unknown account, an unminted code and a missing
# bearer are all answerable without one. The one thing never asserted here is a
# real token - the script must not be able to print one.

$sessNoAuth = Invoke-Cuw 'GET' '/session/0123456789abcdef0123456789abcdef' $null -NoAuth
$sessUnknown = Invoke-Cuw 'GET' '/session/0123456789abcdef0123456789abcdef' $null
$switchUnknown = Invoke-Cuw 'POST' '/accounts/no-such-account/session' '{}'
$sessionOk = $true
$why = ''
if ($sessNoAuth.Status -ne 401) { $sessionOk = $false; $why = "unauthenticated redeem gave $($sessNoAuth.Status), want 401" }
elseif ($sessUnknown.Status -ne 404) { $sessionOk = $false; $why = "unknown code gave $($sessUnknown.Status), want 404" }
elseif ($switchUnknown.Status -ne 404) { $sessionOk = $false; $why = "switch on an unknown account gave $($switchUnknown.Status), want 404" }
elseif (($sessUnknown.Body -match 'sk-ant') -or ($switchUnknown.Body -match 'sk-ant')) {
    $sessionOk = $false; $why = 'a refusal body contains a token prefix'
} else {
    $why = 'redeem needs the bearer; unknown code and unknown account both 404'
}
if ($sessionOk) { Report 'session' 'PASS' $why } else { Report 'session' 'FAIL' $why }

# --- 4. SSE first frame -----------------------------------------------------

$sse = Read-Sse 3 $null $null
if ($sse.Ok -and $sse.Text.TrimStart().StartsWith('event: accounts') -and ($sse.Text -notmatch 'sk-ant')) {
    Report 'sse' 'PASS' 'first frame is event: accounts, no token prefix'
} else {
    Report 'sse' 'FAIL' "ok=$($sse.Ok) firstFrame=$($sse.Text.Split("`n") | Select-Object -First 1)"
}

# --- 5. Live connect (opens a browser) --------------------------------------

$liveId = $null
if ($SkipLive) {
    Report 'connect' 'SKIP' '-SkipLive: no browser run'
} else {
    $before = @((Invoke-Cuw 'GET' '/accounts' $null).Body | ConvertFrom-Json) | Where-Object { $null -ne $_ }
    $beforeIds = @($before | ForEach-Object { $_.id })
    $live = Read-Sse 90 '/accounts' '{"label":"e2e"}'
    $phases = @()
    foreach ($m in ([regex]::Matches($live.Text, '"phase"\s*:\s*"([a-z_]+)"'))) {
        if ($phases -notcontains $m.Groups[1].Value) { $phases += $m.Groups[1].Value }
    }
    Say ("connect phases seen: {0}" -f ($phases -join ', '))
    Say ("awaiting_code appeared: {0} (plan par.8 Q7)" -f ($phases -contains 'awaiting_code'))
    if ($phases -contains 'validated' -and $live.PostStatus -eq 200) {
        $ready = $false
        $deadline = (Get-Date).AddMinutes(3)
        while ((Get-Date) -lt $deadline) {
            $now = @((Invoke-Cuw 'GET' '/accounts' $null).Body | ConvertFrom-Json) | Where-Object { $null -ne $_ }
            $fresh = @($now | Where-Object { $beforeIds -notcontains $_.id })
            if ($fresh.Count -ge 1 -and $fresh[0].state -eq 'available') {
                $liveId = $fresh[0].id
                $ready = $true
                break
            }
            Start-Sleep -Seconds 20
        }
        $scratchEmpty = $true
        if (Test-Path $scratch) {
            if ((Get-ChildItem $scratch -ErrorAction SilentlyContinue | Measure-Object).Count -gt 0) { $scratchEmpty = $false }
        }
        if ($ready -and $scratchEmpty) {
            Report 'connect' 'PASS' "validated; new row available; scratch empty"
        } else {
            Report 'connect' 'FAIL' "available=$ready scratchEmpty=$scratchEmpty"
        }
        if ($null -ne $liveId) {
            $del = Invoke-Cuw 'DELETE' "/accounts/$liveId" $null
            $after = @((Invoke-Cuw 'GET' '/accounts' $null).Body | ConvertFrom-Json) | Where-Object { $null -ne $_ }
            $gone = (@($after | Where-Object { $_.id -eq $liveId }).Count -eq 0)
            if ($del.Status -eq 204 -and $gone) {
                Report 'cleanup' 'PASS' 'e2e row deleted'
            } else {
                Report 'cleanup' 'FAIL' "delete=$($del.Status) gone=$gone"
            }
        }
    } else {
        Report 'connect' 'FAIL' "post=$($live.PostStatus) phases=$($phases -join ',')"
    }
}
if ($Reconnect -and $null -eq $liveId) {
    Report 'reconnect' 'SKIP' 'no live row to reconnect (run without -SkipLive first)'
}

# --- 6. Refresh observation -------------------------------------------------

# PS 5.1: ConvertFrom-Json '[]' yields $null, so filter it out.
$obs = @((Invoke-Cuw 'GET' '/accounts' $null).Body | ConvertFrom-Json) | Where-Object { $null -ne $_ }
foreach ($a in $obs) {
    Say ("row {0}: refresh={1} refreshed_at={2} persist_pending={3}" -f $a.label, $a.refresh, $a.refreshed_at, $a.persist_pending)
}
Report 'refresh' 'SKIP' 'not forced; the isolated dir holds only rows this run made (plan par.8 Q8/Q12)'

# --- 7. Redaction -----------------------------------------------------------

# The real widget's log is scanned too: a leak there is a leak, even though
# this run wrote none of it.
$leak = $false
$logs = @((Join-Path $dataDir 'daemon.log'), (Join-Path $realDataDir 'daemon.log'), $daemonOut, $daemonErr)
foreach ($f in $logs) {
    if (Test-Path $f) {
        if ((Get-Content -Raw $f -ErrorAction SilentlyContinue) -match 'sk-ant') { $leak = $true; Say "token prefix found in $f" }
    }
}
if ($leak) { Report 'redaction' 'FAIL' 'a log contains a token prefix' } else { Report 'redaction' 'PASS' 'no token prefix in daemon logs' }

# --- 8. Shutdown ------------------------------------------------------------

$down = Invoke-Cuw 'POST' '/shutdown' $null
$gone = $false
$deadline = (Get-Date).AddSeconds(3)
while ((Get-Date) -lt $deadline) {
    if (-not (Test-Port $script:Port)) { $gone = $true; break }
    Start-Sleep -Milliseconds 200
}
# The pid file goes at process exit, a moment after the listener closes.
$pidGone = $false
$deadline = (Get-Date).AddSeconds(3)
while ((Get-Date) -lt $deadline) {
    if (-not (Test-Path $pidFile)) { $pidGone = $true; break }
    Start-Sleep -Milliseconds 200
}
if ($down.Status -eq 204 -and $gone -and $pidGone) {
    Report 'shutdown' 'PASS' '204, port closed <=3 s, pid file removed'
} else {
    Report 'shutdown' 'FAIL' "status=$($down.Status) gone=$gone pidRemoved=$pidGone"
    Stop-Widget
}

# --- 9. Manual matrix + leak self-check --------------------------------------

Say ''
Say 'Manual overlay matrix:'
Say '  undocked: drag, Esc, settings persist, tray show/hide/quit, click-through'
Say '  docked:   tray pick WT, move/resize/minimise, Win+D, virtual desktop,'
Say '            close+reopen WT, Alt-Tab absence, modal focus return'
Say '  multi-monitor: needs an external display'
Say ''

$bearerCheck = $null
$leakSelf = $false
try {
    Read-BearerInto ([ref]$bearerCheck)
    $own = Get-Content -Raw $outFile
    if ($own -match 'sk-ant') { $leakSelf = $true }
    if ($bearerCheck.Length -gt 0 -and $own.Contains($bearerCheck)) { $leakSelf = $true }
} catch {
    # No bearer file (daemon never started): nothing to leak.
} finally {
    Remove-Variable bearerCheck -ErrorAction SilentlyContinue
}
if ($leakSelf) { Report 'no-leak' 'FAIL' 'script output contains a secret' } else { Report 'no-leak' 'PASS' 'script output is clean' }

Say ''
Say "isolated state left behind: $dataDir (and keyring service com.local.cuw-e2e)"
Say '== summary =='
foreach ($r in $script:Results) { Say ("{0,-5} {1,-12} {2}" -f $r.Status, $r.Step, $r.Reason) }
$fails = @($script:Results | Where-Object { $_.Status -eq 'FAIL' }).Count
Reset-Env
if ($fails -gt 0) { exit 1 } else { exit 0 }
