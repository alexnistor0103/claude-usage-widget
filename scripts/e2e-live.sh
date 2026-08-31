#!/bin/sh
# End-to-end live test for cuw (I3), the POSIX twin of e2e-live.ps1. Drives the
# daemon over localhost and prints a PASS/FAIL/SKIP table. Never prints a
# secret: the bearer reaches curl on stdin, responses are scanned in files the
# script owns, and the script's own output is checked for leaks as the last
# step.
#
#   scripts/e2e-live.sh --skip-live   # no browser
#   scripts/e2e-live.sh               # one live connect
#   scripts/e2e-live.sh --reconnect   # + one reconnect
#
# Rules: stops the overlay and cuw-daemon, never a
# `claude` process; never touches ~/.claude; never reads a keyring value.
#
# The whole run is isolated (STATUS, 2026-08-31): its own data dir, its own
# keyring namespace and its own port, so it can neither read nor rewrite the
# real registry.toml or a real credential. Two daemons sharing one data dir is
# what cost two accounts.
#
# No `set -e`: a failing check is a FAIL row, not an abort.
set -u

skip_live=no
reconnect=no
# Not 8787: nothing here may land on the port the real widget uses.
port=8799

while [ $# -gt 0 ]; do
    case $1 in
        --skip-live) skip_live=yes ;;
        --reconnect) reconnect=yes ;;
        --port)
            shift
            port=${1:-}
            [ -n "$port" ] || { echo "usage: e2e-live.sh [--skip-live] [--reconnect] [--port N]" >&2; exit 2; }
            ;;
        *) echo "usage: e2e-live.sh [--skip-live] [--reconnect] [--port N]" >&2; exit 2 ;;
    esac
    shift
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=${TMPDIR:-/tmp}/cuw-e2e
mkdir -p "$tmp"
out=$tmp/e2e-output.log
: > "$out"
body_file=$tmp/resp.body
sse_file=$tmp/sse.txt
: > "$body_file"

# CUW_DATA_DIR replaces the ProjectDirs data dir and CUW_KEYRING_SERVICE the
# keyring service name; they exist so a test daemon gets its own registry,
# bearer file, scratch root and credentials (cuw-daemon startup.rs, cuw-creds
# lib.rs).
data=$tmp/data
mkdir -p "$data"
printf 'port = %s\n' "$port" > "$tmp/accounts.toml"
CUW_DATA_DIR=$data
CUW_KEYRING_SERVICE=com.local.cuw-e2e
CUW_CONFIG=$tmp/accounts.toml
export CUW_DATA_DIR CUW_KEYRING_SERVICE CUW_CONFIG
bearer_file=$data/bearer.token

case $(uname -s) in
    Darwin) real_data=$HOME/Library/Application\ Support/com.local.cuw ;;
    *) real_data=${XDG_DATA_HOME:-$HOME/.local/share}/cuw ;;
esac

results=$tmp/results.txt
: > "$results"

say() {
    printf '%s\n' "$*" | tee -a "$out"
}

report() { # step status reason
    printf '%-5s %-12s %s\n' "$2" "$1" "$3" >> "$results"
    say "$(printf '%-5s %-12s %s' "$2" "$1" "$3")"
}

# The overlay first: it respawns the daemon within seconds of losing it, so
# killing the daemon alone leaves a daemon this script did not start (STATUS).
stop_widget() {
    if command -v pkill >/dev/null 2>&1; then
        pkill -x cuw-overlay >/dev/null 2>&1
        pkill -x cuw-daemon >/dev/null 2>&1
    fi
    return 0
}

# Builds a curl config on stdout. The bearer goes in here, never on the argv
# where `ps` would show it to any process in the session (same trick as the
# session shim).
req_config() { # method path body_file|'' auth|noauth
    printf 'url = "http://127.0.0.1:%s%s"\n' "$port" "$2"
    printf 'request = "%s"\n' "$1"
    if [ "$4" = auth ] && [ -f "$bearer_file" ]; then
        printf 'header = "Authorization: Bearer %s"\n' "$(cat "$bearer_file")"
    fi
    if [ -n "$3" ]; then
        printf 'header = "Content-Type: application/json"\n'
        printf 'data = "@%s"\n' "$3"
    fi
    printf 'silent\nshow-error\nmax-time = 30\n'
}

# Sets $status; the body lands in $body_file.
request() { # method path body-json|'' [noauth]
    _body=
    if [ -n "${3:-}" ]; then
        _body=$tmp/req.json
        printf '%s' "$3" > "$_body"
    fi
    : > "$body_file"
    status=$(
        {
            req_config "$1" "$2" "$_body" "${4:-auth}"
            printf 'output = "%s"\nwrite-out = "%%{http_code}"\n' "$body_file"
        } | curl -K - 2>/dev/null
    )
    [ -n "$status" ] || status=000
}

# The wire is compact JSON (serde, no spaces) and this script carries no jq
# dependency: awk splits the array into one row per line, sed pulls a field out
# of a row.
rows() { awk '{ gsub(/},[{]/, "}\n{"); print }' "$body_file"; }

jfield() { # row field  (quoted or bare value)
    printf '%s' "$1" | sed -n 's/.*"'"$2"'":[[:space:]]*"\{0,1\}\([^",}]*\)"\{0,1\}.*/\1/p'
}

ids_in_body() {
    tr ',' '\n' < "$body_file" | sed -n 's/.*"id":[[:space:]]*"\([^"]*\)".*/\1/p'
}

port_open() {
    curl -s --max-time 1 -o /dev/null "http://127.0.0.1:$port/" >/dev/null 2>&1
}

now() { date +%s; }

# Reads /events for $1 seconds into $sse_file. With $2 given that POST is
# started first and both run concurrently - a connect POST only answers when
# the flow ends. Sets $post_status ('' when no POST was made).
read_sse() { # seconds [post-path] [post-body]
    _secs=$1
    _path=${2:-}
    _post_body=${3:-}
    : > "$sse_file"
    : > "$tmp/post.status"
    {
        req_config GET /events '' auth
        printf 'no-buffer\noutput = "%s"\nmax-time = %s\n' "$sse_file" "$_secs"
    } | curl -K - >/dev/null 2>&1 &
    _sse_pid=$!

    _post_pid=
    if [ -n "$_path" ]; then
        printf '%s' "$_post_body" > "$tmp/post.json"
        {
            {
                req_config POST "$_path" "$tmp/post.json" auth
                printf 'output = "%s"\nwrite-out = "%%{http_code}"\nmax-time = %s\n' \
                    "$tmp/post.body" "$((_secs + 30))"
            } | curl -K - 2>/dev/null > "$tmp/post.status"
        } &
        _post_pid=$!
    fi

    _deadline=$(( $(now) + _secs ))
    while [ "$(now)" -lt "$_deadline" ]; do
        # A live connect ends when the validated/failed phase arrives.
        if [ -n "$_path" ] && grep -qE '"validated"|"failed"' "$sse_file" 2>/dev/null; then
            break
        fi
        kill -0 "$_sse_pid" 2>/dev/null || break
        sleep 1
    done
    kill "$_sse_pid" >/dev/null 2>&1
    wait "$_sse_pid" >/dev/null 2>&1

    post_status=
    if [ -n "$_post_pid" ]; then
        wait "$_post_pid" >/dev/null 2>&1
        post_status=$(cat "$tmp/post.status" 2>/dev/null)
    fi
}

run_cargo() { # step dir args...
    _step=$1
    _dir=$2
    shift 2
    _slug=$(printf '%s' "$*" | tr -c 'a-z0-9_-' '_')
    if ! ( cd "$_dir" && cargo "$@" ) >"$tmp/$_slug.out.log" 2>"$tmp/$_slug.err.log"; then
        report "$_step" FAIL "cargo $* failed (see $tmp/$_slug.err.log)"
        return 1
    fi
    return 0
}

# --- 1. Preflight -----------------------------------------------------------

say "== cuw e2e ($(date +%Y-%m-%dT%H:%M:%S)) =="
say "isolated: data dir $data, keyring com.local.cuw-e2e, port $port"
say 'stopping the overlay and any running daemon'
stop_widget

pre=yes
run_cargo fmt "$root" fmt --check || pre=no
[ "$pre" = yes ] && { run_cargo clippy "$root" clippy --all-targets || pre=no; }
[ "$pre" = yes ] && { run_cargo test "$root" test || pre=no; }
[ "$pre" = yes ] && { run_cargo overlay "$root/apps/overlay/src-tauri" check || pre=no; }
if [ "$pre" = yes ]; then
    report preflight PASS 'fmt/clippy/test/overlay-check green'
else
    say 'Preflight failed; aborting.'
    exit 1
fi

# --- 2. Start the daemon ----------------------------------------------------

rm -f "$data/port"
daemon_out=$tmp/daemon.out.log
daemon_err=$tmp/daemon.err.log
( cd "$root" && exec cargo run -p cuw-daemon ) >"$daemon_out" 2>"$daemon_err" &
daemon_pid=$!

up=no
died=
deadline=$(( $(now) + 30 ))
while [ "$(now)" -lt "$deadline" ]; do
    if [ -f "$data/port" ]; then
        p=$(tr -dc '0-9' < "$data/port")
        if [ -n "$p" ]; then
            port=$p
            if port_open; then up=yes; break; fi
        fi
    fi
    # A daemon that loses the single-instance race exits 2 before writing the
    # port file (cuw-daemon startup.rs), so waiting out the deadline would only
    # delay the same verdict.
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        wait "$daemon_pid" 2>/dev/null && code=0 || code=$?
        if [ "$code" -eq 2 ]; then
            died="the port is already owned by another cuw-daemon - stop the widget first; "
        else
            died="the daemon exited with code $code; "
        fi
        break
    fi
    sleep 1
done

scratch_clean=yes
if [ -d "$data/scratch" ] && [ -n "$(ls -A "$data/scratch" 2>/dev/null)" ]; then
    scratch_clean=no
fi
if [ "$up" = yes ] && [ -f "$data/pid" ] && [ "$scratch_clean" = yes ]; then
    report startup PASS "port $port, pid file present, scratch clean"
else
    report startup FAIL "${died}up=$up pid=$([ -f "$data/pid" ] && echo yes || echo no) scratchClean=$scratch_clean (see $daemon_err)"
    stop_widget
    exit 1
fi

# --- 3. Auth + wire shape ---------------------------------------------------

request GET /accounts '' noauth
no_auth_status=$status
request GET /accounts ''
with_auth_status=$status

wire_ok=yes
why=
if [ "$no_auth_status" != 401 ]; then
    wire_ok=no; why="no-bearer GET gave $no_auth_status, want 401"
elif [ "$with_auth_status" != 200 ]; then
    wire_ok=no; why="bearer GET gave $with_auth_status, want 200"
elif grep -q 'sk-ant' "$body_file"; then
    wire_ok=no; why='response body contains a token prefix'
else
    seen=0
    rows > "$tmp/rows.txt"
    while IFS= read -r row; do
        case $row in *'"id":'*) ;; *) continue ;; esac
        seen=$((seen + 1))
        for k in stale fetched_at scoped access_expires_at refreshed_at refresh persist_pending can_switch; do
            case $row in *"\"$k\":"*) ;; *) wire_ok=no; why="row missing '$k'" ;; esac
        done
        case $row in *'"expires_at":'*) wire_ok=no; why="row still carries 'expires_at'" ;; esac
    done < "$tmp/rows.txt"
    if [ "$wire_ok" = yes ] && [ "$seen" -eq 0 ]; then
        why='401 without bearer, 200 with; 0 rows - the isolated registry is empty, so the row shape is unchecked'
    elif [ "$wire_ok" = yes ]; then
        why="401 without bearer, 200 with; $seen row(s), wire shape ok"
    fi
fi
if [ "$wire_ok" = yes ]; then report auth-wire PASS "$why"; else report auth-wire FAIL "$why"; fi

# --- 3b. Session routes (M7.2) ----------------------------------------------
# No live login needed: an unknown account, an unminted code and a missing
# bearer are all answerable without one. The one thing never asserted here is a
# real token - the script must not be able to print one.

request GET /session/0123456789abcdef0123456789abcdef '' noauth
sess_noauth=$status
request GET /session/0123456789abcdef0123456789abcdef ''
sess_unknown=$status
cp "$body_file" "$tmp/session-unknown.body"
request POST /accounts/no-such-account/session '{}'
switch_unknown=$status

session_ok=yes
if [ "$sess_noauth" != 401 ]; then
    session_ok=no; why="unauthenticated redeem gave $sess_noauth, want 401"
elif [ "$sess_unknown" != 404 ]; then
    session_ok=no; why="unknown code gave $sess_unknown, want 404"
elif [ "$switch_unknown" != 404 ]; then
    session_ok=no; why="switch on an unknown account gave $switch_unknown, want 404"
elif grep -q 'sk-ant' "$tmp/session-unknown.body" "$body_file"; then
    session_ok=no; why='a refusal body contains a token prefix'
else
    why='redeem needs the bearer; unknown code and unknown account both 404'
fi
if [ "$session_ok" = yes ]; then report session PASS "$why"; else report session FAIL "$why"; fi

# --- 4. SSE first frame -----------------------------------------------------

read_sse 3
first=$(sed -n '1p' "$sse_file" 2>/dev/null)
case $first in
    'event: accounts'*)
        if grep -q 'sk-ant' "$sse_file"; then
            report sse FAIL 'first frame carries a token prefix'
        else
            report sse PASS 'first frame is event: accounts, no token prefix'
        fi
        ;;
    *) report sse FAIL "firstFrame=$first" ;;
esac

# --- 5. Live connect (opens a browser) --------------------------------------

live_id=
if [ "$skip_live" = yes ]; then
    report connect SKIP '--skip-live: no browser run'
else
    request GET /accounts ''
    ids_in_body > "$tmp/before-ids.txt"
    read_sse 90 /accounts '{"label":"e2e"}'
    phases=$(tr ',' '\n' < "$sse_file" | sed -n 's/.*"phase":"\([a-z_]*\)".*/\1/p' | awk '!seen[$0]++' | tr '\n' ' ')
    say "connect phases seen: $phases"
    case " $phases " in
        *' awaiting_code '*) say 'awaiting_code appeared: yes (plan par.8 Q7)' ;;
        *) say 'awaiting_code appeared: no (plan par.8 Q7)' ;;
    esac
    if grep -q '"validated"' "$sse_file" && [ "$post_status" = 200 ]; then
        ready=no
        deadline=$(( $(now) + 180 ))
        while [ "$(now)" -lt "$deadline" ]; do
            request GET /accounts ''
            rows > "$tmp/rows.txt"
            while IFS= read -r row; do
                case $row in *'"id":'*) ;; *) continue ;; esac
                rid=$(jfield "$row" id)
                [ -n "$rid" ] || continue
                if ! grep -qx "$rid" "$tmp/before-ids.txt" 2>/dev/null; then
                    if [ "$(jfield "$row" state)" = available ]; then
                        live_id=$rid
                        ready=yes
                    fi
                fi
            done < "$tmp/rows.txt"
            [ "$ready" = yes ] && break
            sleep 20
        done
        scratch_empty=yes
        if [ -d "$data/scratch" ] && [ -n "$(ls -A "$data/scratch" 2>/dev/null)" ]; then
            scratch_empty=no
        fi
        if [ "$ready" = yes ] && [ "$scratch_empty" = yes ]; then
            report connect PASS 'validated; new row available; scratch empty'
        else
            report connect FAIL "available=$ready scratchEmpty=$scratch_empty"
        fi
        if [ -n "$live_id" ]; then
            request DELETE "/accounts/$live_id" ''
            del=$status
            request GET /accounts ''
            gone=yes
            ids_in_body | grep -qx "$live_id" && gone=no
            if [ "$del" = 204 ] && [ "$gone" = yes ]; then
                report cleanup PASS 'e2e row deleted'
            else
                report cleanup FAIL "delete=$del gone=$gone"
            fi
        fi
    else
        report connect FAIL "post=$post_status phases=$phases"
    fi
fi
if [ "$reconnect" = yes ] && [ -z "$live_id" ]; then
    report reconnect SKIP 'no live row to reconnect (run without --skip-live first)'
fi

# --- 6. Refresh observation -------------------------------------------------

request GET /accounts ''
rows > "$tmp/rows.txt"
while IFS= read -r row; do
    case $row in *'"id":'*) ;; *) continue ;; esac
    say "row $(jfield "$row" label): refresh=$(jfield "$row" refresh) refreshed_at=$(jfield "$row" refreshed_at) persist_pending=$(jfield "$row" persist_pending)"
done < "$tmp/rows.txt"
report refresh SKIP 'not forced; the isolated dir holds only rows this run made (plan par.8 Q8/Q12)'

# --- 7. Redaction -----------------------------------------------------------

# The real widget's log is scanned too: a leak there is a leak, even though
# this run wrote none of it.
leak=no
for f in "$data/daemon.log" "$real_data/daemon.log" "$daemon_out" "$daemon_err"; do
    if [ -f "$f" ] && grep -q 'sk-ant' "$f" 2>/dev/null; then
        leak=yes
        say "token prefix found in $f"
    fi
done
if [ "$leak" = yes ]; then
    report redaction FAIL 'a log contains a token prefix'
else
    report redaction PASS 'no token prefix in daemon logs'
fi

# --- 8. Shutdown ------------------------------------------------------------

request POST /shutdown ''
down=$status
gone=no
deadline=$(( $(now) + 3 ))
while [ "$(now)" -le "$deadline" ]; do
    port_open || { gone=yes; break; }
    sleep 1
done
# The pid file goes at process exit, a moment after the listener closes.
pid_gone=no
deadline=$(( $(now) + 3 ))
while [ "$(now)" -le "$deadline" ]; do
    [ -f "$data/pid" ] || { pid_gone=yes; break; }
    sleep 1
done
if [ "$down" = 204 ] && [ "$gone" = yes ] && [ "$pid_gone" = yes ]; then
    report shutdown PASS '204, port closed <=3 s, pid file removed'
else
    report shutdown FAIL "status=$down gone=$gone pidRemoved=$pid_gone"
    stop_widget
fi

# --- 9. Manual matrix + leak self-check --------------------------------------

say ''
say 'Manual overlay matrix:'
say '  undocked: drag, Esc, settings persist, tray show/hide/quit, click-through'
say '  docked:   pick Terminal.app or iTerm2, move/resize/minimise, space switch,'
say '            close+reopen the terminal, Cmd-Tab absence, modal focus return'
say '  macOS:    both tracking paths - CGWindowList without a grant, AX with one'
say '  multi-monitor: needs an external display'
say ''

leak_self=no
if grep -q 'sk-ant' "$out"; then leak_self=yes; fi
# Patterns are read straight from the file, so the bearer never becomes an
# argument either.
if [ -s "$bearer_file" ] && grep -qFf "$bearer_file" "$out"; then leak_self=yes; fi
if [ "$leak_self" = yes ]; then
    report no-leak FAIL 'script output contains a secret'
else
    report no-leak PASS 'script output is clean'
fi

say ''
say "isolated state left behind: $data (and keyring service com.local.cuw-e2e)"
say '== summary =='
while IFS= read -r line; do say "$line"; done < "$results"
if grep -q '^FAIL' "$results"; then exit 1; fi
exit 0
