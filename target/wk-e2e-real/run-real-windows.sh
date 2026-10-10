#!/usr/bin/env bash
# wk real-provider end-to-end suite for Windows (Git Bash). Drives the actual
# claude / codex / grok CLIs through the daemon: functional assertions only —
# state transitions and marker words, never exact response strings.
#
# Uses real provider quota. Provider sections are skipped (not failed) when
# the CLI is missing. WK_E2E_PROVIDERS="claude codex" limits the run.
#
# Usage: bash target/wk-e2e-real/run-real-windows.sh   (repo root, wk built)
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WK="${WK_BIN:-$REPO/target/debug/wk.exe}"
[ -x "$WK" ] || { echo "ABORT: wk.exe not built at $WK (cargo build -p wk)"; exit 1; }

# The JSON helpers below call `python`; bare python is often not on PATH on
# Windows, so resolve a uv-managed interpreter and prepend its directory.
find_python_dir() {
  local candidate
  if command -v python >/dev/null 2>&1 && python -c 'pass' >/dev/null 2>&1; then
    # command -v resolves the executable itself; we need its directory.
    cygpath -m "$(dirname "$(command -v python)")"
    return 0
  fi
  if command -v uv >/dev/null 2>&1; then
    # uv python find prints the interpreter path, not the directory.
    candidate="$(uv python find 2>/dev/null)" && [ -n "$candidate" ] && {
      [ -f "$candidate" ] && candidate="$(dirname "$candidate")"
      cygpath -m "$candidate"; return 0; }
  fi
  return 1
}
PYDIR="$(find_python_dir)" || { echo "ABORT: no python found for the JSON helpers"; exit 1; }
export PATH="$(cygpath "$PYDIR"):$PATH"

ROOT="$(cygpath -m "$TEMP")/wk-e2e-real"
export WK_HOME="$ROOT/home"
CWD="$ROOT/work"
mkdir -p "$CWD"

pass=0; fail=0; skip=0
ok()  { pass=$((pass+1)); echo "PASS [$1] $2"; }
bad() { fail=$((fail+1)); echo "FAIL [$1] $2"; }
check() { if [ "$3" = "$4" ]; then ok "$1" "$2"; else bad "$1" "$2 — expected [$3] got [$4]"; fi; }
contains() { case "$4" in *"$3"*) ok "$1" "$2";; *)
  bad "$1" "$2 — missing [$3]; got: $(printf '%s' "$4" | tail -c 240 | tr '\n' '|')";; esac; }
lacks() { case "$4" in *"$3"*) bad "$1" "$2 — unexpectedly contains [$3]";; *) ok "$1" "$2";; esac; }
skip_note() { skip=$((skip+1)); echo "SKIP [$1] $2"; }
ctx_of() { sed -n 's/^\[wk\] ctx=\([a-z0-9]\{6\}\).*/\1/p' "$1" | head -1; }

# Poll `wk list --json` until the newest context reaches a state; prints the id.
wait_state() { # expected-state max-seconds
  local want=$1 end=$((SECONDS + $2))
  while [ "$SECONDS" -lt "$end" ]; do
    local line id state
    line=$("$WK" list --json 2>/dev/null | python -I -c '
import json, sys
d = json.load(sys.stdin)
cs = d.get("contexts", [])
c = max(cs, key=lambda x: x.get("createdAt", 0)) if cs else None
print((c["id"] + " " + c.get("state", "")) if c else " ")
' 2>/dev/null)
    id=${line%% *}; state=${line#* }
    if [ -n "$id" ] && [ "$state" = "$want" ]; then echo "$id"; return 0; fi
    sleep 1
  done
  echo ""
}

# First pending permission request, as "<request-id> <option-id>" (wk get --json).
first_permission_option() { # ctx
  "$WK" get "$1" --json 2>/dev/null | python -I -c '
import json, sys
d = json.load(sys.stdin)
for request in d.get("pending") or []:
    if request.get("kind") == "permission":
        options = (request.get("payload") or {}).get("options") or []
        if options:
            print(request.get("id", ""), options[0].get("id", ""))
            break
' 2>/dev/null
}

# Deny-ish option for the pending permission request, as "<request-id> <option-id>".
deny_option() { # ctx
  "$WK" get "$1" --json 2>/dev/null | python -I -c '
import json, sys
d = json.load(sys.stdin)
request = next((r for r in d.get("pending") or [] if r.get("kind") == "permission"), None)
if request is None:
    raise SystemExit
options = (request.get("payload") or {}).get("options") or []
for option in options:
    text = (str(option.get("id", "")) + " " + str(option.get("title", "")) + " " + str(option.get("label", ""))).lower()
    if "deny" in text or "reject" in text or text.strip() == "no":
        print(request.get("id", ""), option.get("id", ""))
        break
else:
    if len(options) > 1:
        print(request.get("id", ""), options[1].get("id", ""))
' 2>/dev/null
}

provider_available() { where.exe "$1" >/dev/null 2>&1; }

seed_workspace() {
  printf 'The secret word is ZEPHYR-7.\n' > "$CWD/notes.txt"
}

provider_basic() { # provider label — prints ONLY the ctx id on stdout; the
  # caller captures it, so the assertion output goes to stderr.
  local provider=$1 label=$2
  local out rc ctx
  out=$("$WK" run "$provider" --mode ask --cwd "$CWD" "Reply with exactly OK and nothing else." 2>"$ROOT/$label-basic.err"); rc=$?
  check "$label-basic-rc" "$label basic ask completes (0)" 0 "$rc" >&2
  contains "$label-basic-ok" "$label reply says OK" "OK" "$out" >&2
  contains "$label-basic-state" "$label reports state=completed" "state=completed" "$(cat "$ROOT/$label-basic.err")" >&2
  ctx=$(ctx_of "$ROOT/$label-basic.err")
  echo "$ctx"
}

provider_read_file() { # provider label
  local provider=$1 label=$2
  local out rc
  out=$("$WK" run "$provider" --mode ask --cwd "$CWD" "Read the file notes.txt in the current directory and reply with just the secret word it contains." 2>/dev/null); rc=$?
  check "$label-read-rc" "$label read-file task completes (0)" 0 "$rc"
  contains "$label-read-word" "$label found the seeded word" "ZEPHYR-7" "$out"
}

provider_continue() { # provider ctx label
  local ctx=$1 label=$2
  local out rc
  out=$("$WK" send "$ctx" "What was the secret word in notes.txt? Reply with just the word." 2>/dev/null); rc=$?
  check "$label-cont-rc" "$label continuation send completes (0)" 0 "$rc"
  contains "$label-cont-word" "$label continued session recalls the word" "ZEPHYR-7" "$out"
}

provider_daemon_restart() { # provider ctx label
  local ctx=$1 label=$2
  local out rc
  "$WK" daemon stop >/dev/null 2>&1
  out=$("$WK" result "$ctx" 2>&1); rc=$?
  if [ "$rc" = 0 ]; then ok "$label-restart-rc" "$label daemon auto-restarts with data (0)"
  else bad "$label-restart-rc" "$label daemon auto-restarts with data (0) — rc=$rc out: $(printf '%s' "$out" | tail -c 200)"; fi
  out=$("$WK" send "$ctx" "Reply with just the word ZEPHYR-7 from earlier." 2>&1); rc=$?
  if [ "$rc" = 0 ]; then ok "$label-restart-send-rc" "$label send after restart completes (0)"
  else bad "$label-restart-send-rc" "$label send after restart completes (0) — rc=$rc out: $(printf '%s' "$out" | tail -c 200)"; fi
}

provider_timeout() { # provider label
  local provider=$1 label=$2
  local rc ctx out
  "$WK" run "$provider" --mode ask --cwd "$CWD" "Write out the numbers 1 to 12 in words, one per line." --timeout 1 --json >/dev/null 2>"$ROOT/$label-tmo.err"; rc=$?
  check "$label-tmo-rc" "$label --timeout exits 12" 12 "$rc"
  ctx=$(ctx_of "$ROOT/$label-tmo.err")
  out=$("$WK" get "$ctx" 2>&1)
  contains "$label-tmo-working" "$label task keeps running after timeout" "state=working" "$out"
  "$WK" wait "$ctx" >/dev/null 2>&1; rc=$?
  check "$label-tmo-wait" "$label wait completes after the turn (0)" 0 "$rc"
}

provider_cancel() { # provider label  (needs a reliably long turn)
  local provider=$1 label=$2
  local rc ctx out pid
  "$WK" run "$provider" --mode auto --cwd "$CWD" "Create a file count.txt in the current directory containing the numbers 1 to 50, one per line, by running shell commands. Do it one number at a time." >/dev/null 2>&1 &
  pid=$!
  ctx=$(wait_state working 90)
  if [ -z "$ctx" ]; then
    bad "$label-cancel-pre" "$label background run never reached working"
    kill $pid 2>/dev/null; wait $pid 2>/dev/null
    return
  fi
  "$WK" cancel "$ctx" >/dev/null 2>&1; rc=$?
  check "$label-cancel-rc" "$label cancel exits 130" 130 "$rc"
  wait $pid 2>/dev/null
  out=$("$WK" get "$ctx" 2>&1)
  contains "$label-cancel-state" "$label task shows state=canceled" "state=canceled" "$out"
}

provider_depth() { # provider label
  local provider=$1 label=$2
  local out rc
  out=$(WK_DEPTH=1 "$WK" run "$provider" --mode ask --cwd "$CWD" "Reply with exactly OK and nothing else." 2>/dev/null); rc=$?
  check "$label-depth-rc" "$label nested run (WK_DEPTH=1) completes (0)" 0 "$rc"
  "$WK" list 2>/dev/null | grep -q "depth=2" && ok "$label-depth-list" "$label list shows depth=2" || bad "$label-depth-list" "$label no depth=2 in list"
}

provider_windows_paths() { # provider label
  local provider=$1 label=$2
  local out
  out=$("$WK" list --json 2>/dev/null)
  printf '%s' "$out" | python -I -c '
import json, sys
d = json.load(sys.stdin)
cwd = d["contexts"][0]["cwd"]
assert cwd and not cwd.startswith("\\\\\\?\\\\"), cwd
' 2>/dev/null \
    && ok "$label-winpath" "$label stored cwd is a plain Windows path" || bad "$label-winpath" "$label cwd not a Windows path: $out"
}

run_claude() {
  echo "=== claude ==="
  local ctx
  ctx=$(provider_basic claude claude) || true
  [ -z "$ctx" ] && { skip_note claude-rest "no context from the basic run"; return; }
  provider_read_file claude claude
  ctx2=$("$WK" list --json 2>/dev/null | python -I -c '
import json, sys
d = json.load(sys.stdin)
cs = d.get("contexts", [])
print(max(cs, key=lambda c: c.get("createdAt", 0))["id"] if cs else "")
')
  provider_daemon_restart "$ctx2" claude
  # Permission flow: ask mode + a Write-tool call must stop with a request.
  # (Read-only commands like `echo` are auto-approved by claude's policy and
  # sandbox, so they never prompt — a file write always does.)
  local out rc opt deny
  out=$("$WK" run claude --mode ask --cwd "$CWD" "Use the Write tool to create a file named perm42.txt in the current directory containing the single line OK-42. Do not use any other method to create it." 2>"$ROOT/claude-perm.err"); rc=$?
  check claude-perm-rc "claude permission stops the run (11)" 11 "$rc"
  contains claude-perm-kind "claude block reports kind=permission" "kind=permission" "$(cat "$ROOT/claude-perm.err")"
  local pctx preq opt
  pctx=$(ctx_of "$ROOT/claude-perm.err")
  read -r preq opt <<< "$(first_permission_option "$pctx")"
  if [ -n "$preq" ] && [ -n "$opt" ]; then
    out=$("$WK" send "$pctx" --reply "$preq" "$opt" 2>&1); rc=$?
    if [ "$rc" = 0 ]; then ok claude-perm-allow-rc "claude permission allow completes (0)"
    else bad claude-perm-allow-rc "claude permission allow completes (0) — rc=$rc out: $(printf '%s' "$out" | tail -c 200)"; fi
    if [ -f "$CWD/perm42.txt" ]; then ok claude-perm-marker "claude approved write landed"
    else bad claude-perm-marker "claude approved write did not land"; fi
  else
    bad claude-perm-option "claude no permission option id in wk get"
  fi
  # Deny path: a fresh permission stop, answered with the deny-ish option.
  out=$("$WK" run claude --mode ask --cwd "$CWD" "Use the Write tool to create a file named deny77.txt in the current directory containing the single line NO-77. If the write is not allowed, do not try any other way to create the file — just say SKIPPED." 2>"$ROOT/claude-deny.err"); rc=$?
  check claude-deny-rc "claude second permission stops (11)" 11 "$rc"
  local dctx dreq deny
  dctx=$(ctx_of "$ROOT/claude-deny.err")
  read -r dreq deny <<< "$(deny_option "$dctx")"
  if [ -n "$dreq" ] && [ -n "$deny" ]; then
    out=$("$WK" send "$dctx" --reply "$dreq" "$deny" 2>&1); rc=$?
    if [ "$rc" = 0 ]; then ok claude-deny-rc2 "claude deny completes the turn (0)"
    else bad claude-deny-rc2 "claude deny completes the turn (0) — rc=$rc out: $(printf '%s' "$out" | tail -c 200)"; fi
    if [ -f "$CWD/deny77.txt" ]; then bad claude-deny-marker "claude denied write landed anyway"
    else ok claude-deny-marker "claude denied write did not land"; fi
  else
    bad claude-deny-option "claude no deny option id found"
  fi
  rm -f "$CWD/perm42.txt" "$CWD/deny77.txt"
  # auto mode self-approves.
  out=$("$WK" run claude --mode auto --cwd "$CWD" "Run this exact shell command: echo permmarker-42  Then reply with the command output." 2>/dev/null); rc=$?
  check claude-auto-rc "claude auto mode self-approves (0)" 0 "$rc"
  contains claude-auto-marker "claude auto-approved command ran" "permmarker-42" "$out"
  provider_timeout claude claude
  provider_cancel claude claude
  provider_depth claude claude
  provider_windows_paths claude claude
}

run_codex() {
  echo "=== codex ==="
  local ctx
  ctx=$(provider_basic codex codex) || true
  [ -z "$ctx" ] && { skip_note codex-rest "no context from the basic run"; return; }
  provider_read_file codex codex
  provider_timeout codex codex
  # Steer: a long turn, a mid-turn send, the old waiter superseded (12).
  local pid out rc
  "$WK" run codex --mode auto --cwd "$CWD" "Create a file count.txt in the current directory containing the numbers 1 to 50, one per line, by running shell commands. Do it one number at a time." >"$ROOT/codex-steer.out" 2>&1 &
  pid=$!
  local sctx
  sctx=$(wait_state working 90)
  if [ -n "$sctx" ]; then
    out=$("$WK" send "$sctx" "After the current step, append the word BANANA to count.txt." 2>/dev/null); rc=$?
    check codex-steer-send "codex mid-turn send accepted (0)" 0 "$rc"
    wait $pid; rc=$?
    check codex-steer-old "codex old waiter superseded (12)" 12 "$rc"
    sleep 2
    out=$("$WK" result "$sctx" 2>/dev/null)
    contains codex-steer-word "codex steer reached the turn" "BANANA" "$out"
  else
    skip_note codex-steer "background run never reached working"
    kill $pid 2>/dev/null; wait $pid 2>/dev/null
  fi
  provider_cancel codex codex
  provider_daemon_restart "$sctx" codex
}

run_grok() {
  echo "=== grok ==="
  provider_basic grok grok >/dev/null
  provider_cancel grok grok
}

echo "==== wk real-provider E2E (windows) ===="
seed_workspace
for provider in ${WK_E2E_PROVIDERS:-claude codex grok}; do
  if provider_available "$provider"; then
    "run_$provider"
  else
    skip_note "$provider" "CLI not found on PATH"
  fi
done

echo
echo "==== wk real E2E: $pass passed, $fail failed, $skip skipped ===="
[ $fail = 0 ]
