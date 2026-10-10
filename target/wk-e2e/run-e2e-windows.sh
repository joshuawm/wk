#!/usr/bin/env bash
# wk end-to-end test suite for Windows (Git Bash) — validates the CLI contract
# in docs/wk-help.md against the real daemon (wkd) with a fake provider shim
# on PATH. The unix twin lives in run-e2e.sh; keep scenario coverage aligned.
#
# Usage: bash target/wk-e2e/run-e2e-windows.sh   (from the repo root, wk built)
# WK_E2E_T11=1 opts in to the SIGINT test: msys `kill -INT` against a native
# child is emulation-shaped and may terminate it without the cancel flow, or
# worse reach the whole console group, so it is skipped by default. The
# equivalent manual check: run `wk run claude --mode auto ... "SLEEP:30"` in
# Windows Terminal, press Ctrl+C, and expect exit 130 with state=canceled.
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WK="${WK_BIN:-$REPO/target/debug/wk.exe}"
[ -x "$WK" ] || { echo "ABORT: wk.exe not built at $WK (cargo build -p wk)"; exit 1; }

ROOT="$(cygpath -m "$TEMP")/wk-e2e-win"
WK_HOME_WIN="$ROOT/home"
export WK_HOME="$WK_HOME_WIN"
export WK_MAX_DEPTH=3
export WK_FAKE_LOG="$ROOT/fake-env.jsonl"
CWD="$ROOT/cwd"
BIN="$ROOT/bin"
BIN_POSIX="$(cygpath "$BIN")"

# Locate a python for the fake provider and the JSON assertions. PATH entries
# must be POSIX-form here: MSYS splits a mixed `C:/...` entry on its colon.
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
  for candidate in /c/Users/*/AppData/Roaming/uv/python/cpython-3* /c/Users/*/AppData/Local/uv/python/cpython-3*; do
    if [ -e "$candidate/python.exe" ]; then cygpath -m "$candidate"; return 0; fi
  done
  return 1
}
PYDIR="$(find_python_dir)" || { echo "ABORT: no python found for the fake provider"; exit 1; }
PYDIR_POSIX="$(cygpath "$PYDIR")"
PY="$PYDIR/python.exe"
export PATH="$BIN_POSIX:$PYDIR_POSIX:$PATH"

# Kill only `wk daemon run` processes (not the user's other wk usage).
kill_wkd() {
  powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='wk.exe'\" | Where-Object { \$_.CommandLine -match 'daemon run' } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" >/dev/null 2>&1
}

# No stale daemon may hold our WK_HOME (Windows locks the SQLite files), then
# a clean slate before installing the shim the preflight depends on.
"$WK" daemon stop >/dev/null 2>&1
kill_wkd
sleep 1
for _ in 1 2 3; do rm -rf "$ROOT" 2>/dev/null && break; sleep 1; done
mkdir -p "$WK_HOME" "$CWD" "$BIN"

cp "$REPO/target/wk-e2e/fake-claude.py" "$BIN/fake-claude.py"
sed -i 's/\r$//' "$BIN/fake-claude.py"
printf '@echo off\r\npython "%%~dp0fake-claude.py" %%*\r\n' > "$BIN/claude.cmd"

# Preflight: the fake provider must win resolution exactly the way the daemon
# resolves it — PATH order plus PATHEXT. `where.exe` mirrors that from this
# shell; the daemon additionally captures the PowerShell-profile PATH, which
# keeps the inherited (harness-first) order unless the profile rewrites it.
resolved=$(where.exe claude 2>/dev/null | head -1)
lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | sed 's|/|\\|g'; }
if [ "$(lower "$resolved")" != "$(lower "$BIN")\\claude.cmd" ]; then
  echo "ABORT: claude resolves to [$resolved], not the fake provider shim"
  exit 1
fi

pass=0; fail=0; skip=0; tno=0
t() { tno=$((tno+1)); }
ok()  { pass=$((pass+1)); echo "PASS [$1] $2"; }
bad() { fail=$((fail+1)); echo "FAIL [$1] $2"; }
check() { # id desc expected actual
  if [ "$3" = "$4" ]; then ok "$1" "$2"; else bad "$1" "$2 — expected [$3] got [$4]"; fi
}
contains() { # id desc needle haystack
  case "$4" in *"$3"*) ok "$1" "$2";; *)
    bad "$1" "$2 — missing [$3]; got: $(printf '%s' "$4" | tail -c 240 | tr '\n' '|')";; esac
}
lacks() { # id desc needle haystack
  case "$4" in *"$3"*) bad "$1" "$2 — unexpectedly contains [$3]";; *) ok "$1" "$2";; esac
}
ctx_of() { sed -n 's/^\[wk\] ctx=\([a-z0-9]\{6\}\).*/\1/p' "$1" | head -1; }
state_of() { sed -n 's/.*state=\([a-z-]*\).*/\1/p' "$1" | head -1; }
# A background run prints its ctx only when it ends; discover it from the list
# JSON by the newest createdAt (the text list order is not stable).
newest_ctx() {
  "$WK" list --json 2>/dev/null | "$PY" -I -c '
import json, sys
d = json.load(sys.stdin)
cs = d.get("contexts", [])
print(max(cs, key=lambda c: c.get("createdAt", 0))["id"] if cs else "")
'
}
# Poll until the newest context shows the given state: a background run needs a
# moment to create its context, and grabbing the newest too early can pick the
# previous test's context. Prints the ctx id.
wait_state() { # expected-state max-seconds
  local want=$1 end=$((SECONDS + $2))
  while [ "$SECONDS" -lt "$end" ]; do
    local read id state
    read -r id state <<<"$("$WK" list --json 2>/dev/null | "$PY" -I -c '
import json, sys
d = json.load(sys.stdin)
cs = d.get("contexts", [])
c = max(cs, key=lambda x: x.get("createdAt", 0)) if cs else None
print((c["id"] + " " + c.get("state", "")) if c else " ")
')"
    if [ -n "$id" ] && [ "$state" = "$want" ]; then
      echo "$id"
      return 0
    fi
    sleep 0.3
  done
  newest_ctx
}

# ---------------------------------------------------------------- T2 daemon lifecycle (runs first: later T1 `run` commands auto-start the daemon)
t
out=$("$WK" daemon status 2>&1); rc=$?
check T2a "daemon status before start exits 3" 3 "$rc"
contains T2b "status says not running" "not running" "$out"
"$WK" list >/dev/null 2>&1; rc=$?
check T2c "first command auto-starts the daemon" 0 "$rc"
"$WK" daemon status >/dev/null 2>&1; check T2d "daemon status now exits 0" 0 $?
out=$("$WK" daemon start 2>&1)
contains T2e "daemon start is idempotent" "already running" "$out"
"$WK" daemon stop >/dev/null 2>&1; check T2f "daemon stop exits 0" 0 $?
rc=3
for _ in 1 2 3 4 5 6 7 8 9 10; do
  "$WK" daemon status >/dev/null 2>&1; rc=$?
  [ "$rc" = 3 ] && break
  sleep 0.5
done
check T2g "daemon stopped for real" 3 "$rc"

# ---------------------------------------------------------------- T1 usage errors
t
"$WK" >/dev/null 2>&1; check T1a "wk with no command exits 2" 2 $?
"$WK" run claude "x" >/dev/null 2>&1; check T1b "run without --mode exits 2" 2 $?
"$WK" run claude --mode auto "x" --prompt-file NUL >/dev/null 2>&1; check T1c "PROMPT + --prompt-file exits 2" 2 $?
out=$("$WK" run nosuchprovider --mode auto "x" --cwd "$CWD" 2>&1); rc=$?
check T1d "unknown provider exits 2" 2 "$rc"
contains T1e "unknown provider names the problem" "unknown provider" "$out"
"$WK" run claude --mode bogus "x" >/dev/null 2>&1; check T1f "unknown mode exits 2" 2 $?
"$WK" run amp --mode ask "x" >/dev/null 2>&1; rc=$?
check T1g "amp + --mode ask rejected up front" 2 "$rc"
out=$("$WK" run opencode --mode auto "x" 2>&1); rc=$?
check T1h "opencode unsupported in v1 exits 2" 2 "$rc"
"$WK" help >/dev/null 2>&1; check T1i "wk help exits 0" 0 $?
"$WK" get deadbe >/dev/null 2>&1; check T1j "unknown context id exits 2" 2 $?

# ---------------------------------------------------------------- T3 basic run (completed)
t
out=$("$WK" run claude --mode auto --cwd "$CWD" "ECHO:hello-waku" 2>"$WK_HOME/t3.err"); rc=$?
check T3a "run completes with exit 0" 0 "$rc"
contains T3b "stdout carries the result text" "echo: hello-waku" "$out"
contains T3c "stderr block reports task=t1" "task=t1" "$(cat "$WK_HOME/t3.err")"
contains T3d "stderr block reports state=completed" "state=completed" "$(cat "$WK_HOME/t3.err")"
CTX=$(ctx_of "$WK_HOME/t3.err")
[ ${#CTX} = 6 ] && ok T3e "context id is 6 chars ($CTX)" || bad T3e "no 6-char ctx id; stderr: $(cat "$WK_HOME/t3.err")"
contains T3f "next: line suggests wk send" "next: wk send $CTX" "$(cat "$WK_HOME/t3.err")"
# --model / --effort pass-through
"$WK" run claude --mode auto --cwd "$CWD" --model fake-model --effort high "ECHO:opts" >/dev/null 2>"$WK_HOME/t3g.err"
grep -q '"--model", "fake-model"' "$WK_FAKE_LOG" && ok T3g "--model reaches the provider CLI" || bad T3g "--model not passed through: $(tail -1 "$WK_FAKE_LOG")"
grep -q '"--effort", "high"' "$WK_FAKE_LOG" && ok T3h "--effort reaches the provider CLI" || bad T3h "--effort not passed through"

# ---------------------------------------------------------------- T4 continuation / read-only commands
t
out=$("$WK" send "$CTX" "ECHO:again" 2>"$WK_HOME/t4.err"); rc=$?
check T4a "send starts a new turn, exit 0" 0 "$rc"
contains T4b "second turn is task=t2" "task=t2" "$(cat "$WK_HOME/t4.err")"
out=$("$WK" result "$CTX" 2>/dev/null)
contains T4c "result prints the latest turn text" "echo: again" "$out"
out=$("$WK" result "$CTX" --task t1 2>/dev/null)
contains T4d "result --task t1 prints turn 1" "echo: hello-waku" "$out"
out=$("$WK" get "$CTX" --json 2>/dev/null)
contains T4e "get --json reports completed state" '"state": "completed"' "$out"
contains T4f "get --json echoes the context id" "\"contextId\": \"$CTX\"" "$out"
out=$("$WK" list 2>/dev/null)
contains T4g "list shows the context" "$CTX claude" "$out"
contains T4h "list shows depth=1" "depth=1" "$out"
out=$("$WK" list --json 2>/dev/null)
printf '%s' "$out" | "$PY" -I -c 'import json,sys; d=json.load(sys.stdin); assert d["contexts"]' 2>/dev/null \
  && ok T4i "list --json is valid JSON" || bad T4i "list --json not valid JSON: $out"
# Windows-specific: the stored cwd is a real Windows path, no verbatim prefix.
printf '%s' "$out" | "$PY" -I -c '
import json, sys, os
d = json.load(sys.stdin)
cwd = d["contexts"][0]["cwd"]
assert cwd and not cwd.startswith("\\\\\\?\\\\"), cwd
assert cwd.lower().startswith("c:") or ":" in cwd[:3], cwd
' 2>/dev/null \
  && ok T4j "list --json cwd is a plain Windows path" || bad T4j "cwd not a Windows path: $out"

# ---------------------------------------------------------------- T5 prompt-file & stdin
t
printf 'ECHO:from-file' > "$ROOT/prompt.txt"
out=$("$WK" run claude --mode auto --cwd "$CWD" --prompt-file "$ROOT/prompt.txt" 2>/dev/null); rc=$?
check T5a "run --prompt-file F exits 0" 0 "$rc"
contains T5b "prompt file content reaches the provider" "echo: from-file" "$out"
out=$(printf 'ECHO:from-stdin' | "$WK" run claude --mode auto --cwd "$CWD" --prompt-file - 2>/dev/null); rc=$?
check T5c "run --prompt-file - reads stdin" 0 "$rc"
contains T5d "stdin prompt reaches the provider" "echo: from-stdin" "$out"

# ---------------------------------------------------------------- T6 question flow (exit 10)
t
out=$("$WK" run claude --mode auto --cwd "$CWD" "ASK" 2>"$WK_HOME/t6.err"); rc=$?
check T6a "question stops the run with exit 10" 10 "$rc"
CTX2=$(ctx_of "$WK_HOME/t6.err")
ERR=$(cat "$WK_HOME/t6.err")
contains T6b "block reports kind=question" "kind=question" "$ERR"
contains T6c "block lists q1 with options" "1) Yes, deploy 2) No, wait" "$ERR"
contains T6d "next: line answers via --reply q1" "next: wk send $CTX2 --reply q1" "$ERR"
lacks T6e "stdout is empty at a stop point" "answered" "$out"
out=$("$WK" pending "$CTX2" 2>/dev/null)
contains T6f "pending prints the full JSON payload" "Deploy to production now?" "$out"
"$WK" send "$CTX2" "hello instead" >/dev/null 2>&1; rc=$?
check T6g "prompt send while input-required is rejected (13)" 13 "$rc"
out=$("$WK" send "$CTX2" --reply q1 "直接迁移" 2>"$WK_HOME/t6h.err"); rc=$?
check T6h "free-text answer completes the turn (0)" 0 "$rc"
contains T6i "provider received the answer" "answered: Deploy to production now?=直接迁移" "$out"
"$WK" send "$CTX2" --reply q1 "x" >/dev/null 2>&1; rc=$?
check T6j "answering an answered request exits 2" 2 "$rc"

# ---------------------------------------------------------------- T7 multi-sub-question --answer
t
out=$("$WK" run claude --mode auto --cwd "$CWD" "ASK2" 2>"$WK_HOME/t7.err"); rc=$?
check T7a "two-sub-question request stops with exit 10" 10 "$rc"
CTX3=$(ctx_of "$WK_HOME/t7.err")
"$WK" send "$CTX3" --reply q1 --answer "Deploy to production now?=Yes, deploy" >/dev/null 2>&1; rc=$?
check T7b "partial --answer coverage is an error" 2 "$rc"
out=$("$WK" send "$CTX3" --reply q1 --answer "Deploy to production now?=Yes, deploy" --answer "Which region?=eu" 2>/dev/null); rc=$?
check T7c "full --answer coverage completes (0)" 0 "$rc"
contains T7d "both answers reached the provider" "Yes, deploy" "$out"
contains T7e "second answer reached the provider" "Which region?=eu" "$out"

# ---------------------------------------------------------------- T8 permission flow (exit 11)
t
out=$("$WK" run claude --mode ask --cwd "$CWD" "PERM" 2>"$WK_HOME/t8.err"); rc=$?
check T8a "permission stops the run with exit 11" 11 "$rc"
ERR=$(cat "$WK_HOME/t8.err")
contains T8b "block reports kind=permission" "kind=permission" "$ERR"
contains T8c "block lists the option ids" "options: allow, deny" "$ERR"
CTX4=$(ctx_of "$WK_HOME/t8.err")
"$WK" send "$CTX4" --reply p1 deny 2>/dev/null >"$ROOT/t8.out"; rc=$?
check T8d "deny answer completes the turn (0)" 0 "$rc"
contains T8e "provider acted on the denial" "perm: denied" "$(cat "$ROOT/t8.out")"
"$WK" run claude --mode auto --cwd "$CWD" "PERM" >"$ROOT/t8f.out" 2>"$WK_HOME/t8f.err"; rc=$?
check T8f "auto mode self-approves permissions (0)" 0 "$rc"
contains T8g "auto-approved permission reached the provider" "perm: allowed" "$(cat "$ROOT/t8f.out")"

# ---------------------------------------------------------------- T9 --timeout (exit 12)
t
"$WK" run claude --mode auto --cwd "$CWD" "SLEEP:6" --timeout 2 --json >"$ROOT/t9.out" 2>"$WK_HOME/t9.err"; rc=$?
check T9a "timeout exits 12" 12 "$rc"
grep -q "timeout" "$WK_HOME/t9.err" && ok T9b "block notes the timeout" || bad T9b "no timeout note: $(cat "$WK_HOME/t9.err")"
CTX5=$(ctx_of "$WK_HOME/t9.err")
out=$("$WK" get "$CTX5" 2>&1)
contains T9c "task keeps running after timeout" "state=working" "$out"
"$WK" wait "$CTX5" >/dev/null 2>&1; rc=$?
check T9d "wait blocks until completion (0)" 0 "$rc"

# ---------------------------------------------------------------- T10 cancel (exit 130)
t
"$WK" run claude --mode auto --cwd "$CWD" "SLEEP:30" >/dev/null 2>&1 &
pid=$!
CTX6=$(wait_state working 10)
[ -n "$CTX6" ] && ok T10a "background run produced a ctx ($CTX6)" || bad T10a "no ctx from list"
"$WK" cancel "$CTX6" >/dev/null 2>&1; rc=$?
check T10b "wk cancel exits 130" 130 "$rc"
wait $pid; rc=$?
check T10c "canceled run exits 130" 130 "$rc"
out=$("$WK" get "$CTX6" 2>&1)
contains T10d "get shows state=canceled" "state=canceled" "$out"
out=$("$WK" send "$CTX6" "ECHO:post-cancel" 2>/dev/null); rc=$?
check T10e "context survives cancel; send works" 0 "$rc"
contains T10f "post-cancel turn completed" "echo: post-cancel" "$out"

# ---------------------------------------------------------------- T11 SIGINT to the waiter (opt-in: WK_E2E_T11=1)
t
if [ "${WK_E2E_T11:-0}" = 1 ]; then
  "$WK" run claude --mode auto --cwd "$CWD" "SLEEP:30" >/dev/null 2>&1 &
  pid=$!
  CTX7=$(wait_state working 10)
  if [ -n "$CTX7" ]; then
    kill -INT $pid 2>/dev/null
    sleep 1
    wait $pid; rc=$?
    check T11a "SIGINT makes the waiter exit 130" 130 "$rc"
    out=$("$WK" get "$CTX7" 2>&1)
    contains T11b "interrupted task is canceled" "state=canceled" "$out"
    "$WK" cancel "$CTX7" >/dev/null 2>&1 # cleanup no-op on terminal task
  else
    bad T11pre "no ctx for the SIGINT test"
  fi
else
  skip=$((skip+1))
  echo "SKIP [T11] SIGINT emulation under Git Bash is opt-in (WK_E2E_T11=1); check Ctrl+C manually in Windows Terminal"
fi

# ---------------------------------------------------------------- T12 steer
t
"$WK" run claude --mode auto --cwd "$CWD" "SLEEP:4" >"$ROOT/t12.out" 2>&1 &
pid=$!
CTX8=$(wait_state working 10)
[ -n "$CTX8" ] || bad T12pre "no ctx for steer test"
out=$("$WK" send "$CTX8" "steer-left" 2>&1); rc=$?
check T12a "send during working steers and waits (0)" 0 "$rc"
wait $pid; rc=$?
check T12b "old waiter superseded with exit 12" 12 "$rc"
contains T12c "steer text reached the provider" "steered: steer-left" "$out"

# ---------------------------------------------------------------- T13 close
t
out=$("$WK" close "$CTX8" 2>&1); rc=$?
check T13a "close exits 0" 0 "$rc"
"$WK" send "$CTX8" "x" >/dev/null 2>&1; rc=$?
check T13b "send to a closed context exits 2" 2 "$rc"
"$WK" list >/dev/null 2>&1
"$WK" list 2>/dev/null | grep -q "$CTX8" && bad T13c "closed context hidden from list" || ok T13c "closed context hidden from list"
"$WK" list --all 2>/dev/null | grep -q "$CTX8.*closed" && ok T13d "list --all marks it closed" || bad T13d "list --all missing closed ctx: $("$WK" list --all 2>&1)"

# ---------------------------------------------------------------- T14 on-question policies
t
"$WK" run claude --mode auto --cwd "$CWD" --on-question fail "ASK" >/dev/null 2>"$WK_HOME/t14.err"; rc=$?
check T14a "--on-question fail fails the turn (1)" 1 "$rc"
grep -q "question-not-allowed" "$WK_HOME/t14.err" && ok T14b "error names the question" || bad T14b "no question-not-allowed: $(cat "$WK_HOME/t14.err")"
"$WK" run claude --mode auto --cwd "$CWD" --on-question auto "ASK" >"$ROOT/t14.out" 2>/dev/null; rc=$?
check T14c "--on-question auto completes (0)" 0 "$rc"
grep -q "The user is unavailable" "$ROOT/t14.out" && ok T14d "auto answer text delivered" || bad T14d "auto answer text: $(cat "$ROOT/t14.out")"

# ---------------------------------------------------------------- T15 provider failure
t
"$WK" run claude --mode auto --cwd "$CWD" "FAIL" >/dev/null 2>"$WK_HOME/t15.err"; rc=$?
check T15a "provider error turn exits 1" 1 "$rc"
grep -q "state=failed" "$WK_HOME/t15.err" && ok T15b "block reports state=failed" || bad T15b "$(cat "$WK_HOME/t15.err")"

# ---------------------------------------------------------------- T16 depth & env injection
t
WK_DEPTH=3 "$WK" run claude --mode auto --cwd "$CWD" "ECHO:x" >/dev/null 2>"$WK_HOME/t16.err"; rc=$?
check T16a "depth above max exits 2" 2 "$rc"
grep -qi "depth" "$WK_HOME/t16.err" && ok T16b "error mentions depth" || bad T16b "$(cat "$WK_HOME/t16.err")"
out=$(WK_DEPTH=1 "$WK" run claude --mode auto --cwd "$CWD" "ECHO:depth-check" 2>/dev/null); rc=$?
check T16c "nested run (WK_DEPTH=1) exits 0" 0 "$rc"
"$WK" list 2>/dev/null | grep -q "depth=2" && ok T16d "list shows the nested depth=2" || bad T16d "no depth=2 in list"
"$PY" -I - "$WK_FAKE_LOG" <<'EOF' && ok T16e "daemon injected WK_DEPTH=2 and WK_CONTEXT into the provider" || bad T16e "env injection missing"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert any(r["WK_DEPTH"] == "2" and r["WK_CONTEXT"] for r in rows), rows
EOF

# ---------------------------------------------------------------- T17 persistence across daemon restart
t
"$WK" daemon stop >/dev/null 2>&1
rc=3
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  "$WK" result "$CTX" >/dev/null 2>&1; rc=$?
  [ "$rc" = 0 ] && break
  sleep 0.5
done
check T17a "daemon stop then result auto-restarts and finds data (0)" 0 "$rc"
out=$("$WK" result "$CTX" 2>/dev/null)
contains T17b "result text survived the restart" "echo: again" "$out"
"$WK" list --all >/dev/null 2>&1; check T17c "contexts survived the restart" 0 $?

# ---------------------------------------------------------------- T18 daemon stop with a working task
t
"$WK" run claude --mode auto --cwd "$CWD" "SLEEP:30" >/dev/null 2>&1 &
pid=$!
CTX9=$(wait_state working 10)
[ -n "$CTX9" ] || bad T18pre "no ctx for daemon-stop test"
"$WK" daemon stop >/dev/null 2>&1
sleep 0.5
kill $pid 2>/dev/null
wait $pid 2>/dev/null
out=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
  out=$("$WK" get "$CTX9" --json 2>&1)
  case "$out" in *'"state"'*) break;; esac
  sleep 0.5
done
contains T18a "working task marked failed after daemon stop" '"state": "failed"' "$out"

# ---------------------------------------------------------------- summary
echo
echo "==== wk E2E (windows): $pass passed, $fail failed, $skip skipped ($tno test groups) ===="
[ $fail = 0 ]
