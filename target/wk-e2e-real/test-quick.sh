#!/usr/bin/env bash
# Quick smoke test - just the essentials

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Setup environment if not already set
export PATH="${HOME}/bin:${PATH}"
export WK="${WK:-${HOME}/wk-target/debug/wk}"
export WK_HOME="${WK_HOME:-/tmp/wk-quick}"
export TEST_CWD="${TEST_CWD:-/tmp/wk-quick-cwd}"

# Clean and setup
rm -rf "$WK_HOME" "$TEST_CWD"
mkdir -p "$WK_HOME" "$TEST_CWD"
pkill -f "wk daemon" 2>/dev/null || true
sleep 1

source "$SCRIPT_DIR/common.sh"

PROVIDER="claude"

echo "==== Quick smoke test with Claude Code ===="
echo

# T1: daemon works
t
"$WK" daemon stop >/dev/null 2>&1 || true
sleep 1
"$WK" daemon status >/dev/null 2>&1; rc=$?
check T1 "daemon not running" 3 "$rc"
"$WK" list >/dev/null 2>&1
check T2 "daemon auto-starts" 0 $?

# T3: simple run
t
mkdir -p "$TEST_CWD"
out=$("$WK" run "$PROVIDER" --mode auto --cwd "$TEST_CWD" "Echo: test" 2>"$TEST_CWD/out.err"); rc=$?
check T3 "simple run completes" 0 "$rc"
CTX=$(ctx_of "$TEST_CWD/out.err")
[ ${#CTX} = 6 ] && ok T4 "got context id ($CTX)" || bad T4 "no context id"

# T5: cancel then send
t
"$WK" run "$PROVIDER" --mode auto --cwd "$TEST_CWD" "Think" >/dev/null 2>&1 &
pid=$!
sleep 5
CTX2=$(newest_ctx)
[ -n "$CTX2" ] && ok T5 "background task started ($CTX2)" || bad T5 "no background task"
if [ -n "$CTX2" ]; then
  "$WK" cancel "$CTX2" >/dev/null 2>&1
  wait $pid 2>/dev/null || true
  "$WK" send "$CTX2" "test" 2>/dev/null; rc=$?
  # Accept both 0 (success) and 1 (provider error after cancel)
  if [ "$rc" = 0 ] || [ "$rc" = 1 ]; then
    ok T6 "send after cancel accepted (rc=$rc)"
  else
    bad T6 "send after cancel unexpected rc=$rc"
  fi
fi

# T7: persistence
t
"$WK" daemon stop >/dev/null 2>&1
sleep 1
out=$("$WK" result "$CTX" 2>/dev/null)
check T7 "data persists after restart" 0 $?
[ -n "$out" ] && ok T8 "result text survived" || bad T8 "result empty"

echo
summary
