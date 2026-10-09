#!/usr/bin/env bash
# Real E2E tests for wk with Claude Code CLI

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

# Test configuration
PROVIDER="claude"
TEST_TIMEOUT=120  # Real provider might take longer

echo "==== Testing wk with Claude Code CLI ===="
echo

# ================================================================ T1: daemon lifecycle
t
echo "→ T1: daemon lifecycle"

"$WK" daemon stop >/dev/null 2>&1 || true
sleep 1

out=$("$WK" daemon status 2>&1); rc=$?
check T1a "daemon status before start exits 3" 3 "$rc"
contains T1b "status says not running" "not running" "$out"

"$WK" list >/dev/null 2>&1; rc=$?
check T1c "first command auto-starts daemon" 0 "$rc"

"$WK" daemon status >/dev/null 2>&1
check T1d "daemon status now exits 0" 0 $?

"$WK" daemon start >/dev/null 2>&1
out=$("$WK" daemon start 2>&1)
contains T1e "daemon start is idempotent" "already running" "$out"

# ================================================================ T2: simple query task
t
echo
echo "→ T2: simple query (echo test)"

# Create a test file
mkdir -p "$TEST_CWD"
echo "test content" > "$TEST_CWD/test.txt"

# Use a simple prompt that should complete quickly
out=$("$WK" run "$PROVIDER" --mode auto --cwd "$TEST_CWD" \
  "Echo back: waku-test-marker" 2>"$TEST_CWD/t2.err"); rc=$?

if [ "$rc" = 0 ]; then
  ok T2a "simple task completes with exit 0"
else
  bad T2a "simple task failed with rc=$rc; stderr: $(cat "$TEST_CWD/t2.err")"
fi

CTX1=$(ctx_of "$TEST_CWD/t2.err")
if [ ${#CTX1} = 6 ]; then
  ok T2b "context id is 6 chars ($CTX1)"
else
  bad T2b "no 6-char ctx id; stderr: $(cat "$TEST_CWD/t2.err")"
fi

err=$(cat "$TEST_CWD/t2.err")
contains T2c "stderr reports completed" "state=completed" "$err"
contains T2d "stderr reports task=t1" "task=t1" "$err"

# The output should contain our marker or some response
if [ -n "$out" ]; then
  ok T2e "got non-empty output"
else
  bad T2e "output was empty"
fi

# ================================================================ T3: continuation (send)
t
echo
echo "→ T3: continuation with send"

if [ -z "${CTX1:-}" ]; then
  skip_test T3a "send creates new turn"
  skip_test T3b "second turn is t2"
  skip_test T3c "result shows latest turn"
else
  out=$("$WK" send "$CTX1" "Respond with: continuation-marker" 2>"$TEST_CWD/t3.err"); rc=$?

  if [ "$rc" = 0 ]; then
    ok T3a "send completes with exit 0"
  else
    bad T3a "send failed with rc=$rc"
  fi

  err=$(cat "$TEST_CWD/t3.err")
  contains T3b "second turn is task=t2" "task=t2" "$err"

  out=$("$WK" result "$CTX1" 2>/dev/null)
  if [ -n "$out" ]; then
    ok T3c "result returns non-empty text"
  else
    bad T3c "result returned empty"
  fi
fi

# ================================================================ T4: read-only commands
t
echo
echo "→ T4: read-only commands (get, list, result)"

if [ -z "${CTX1:-}" ]; then
  skip_test T4a "get --json works"
  skip_test T4b "get reports correct state"
  skip_test T4c "list shows context"
  skip_test T4d "list --json is valid"
else
  out=$("$WK" get "$CTX1" --json 2>/dev/null); rc=$?
  check T4a "get --json exits 0" 0 "$rc"

  if echo "$out" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert "state" in d' 2>/dev/null; then
    ok T4b "get --json has valid state field"
  else
    bad T4b "get --json invalid or missing state: $out"
  fi

  out=$("$WK" list 2>/dev/null)
  contains T4c "list shows the context" "$CTX1" "$out"

  out=$("$WK" list --json 2>/dev/null)
  if echo "$out" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert "contexts" in d' 2>/dev/null; then
    ok T4d "list --json is valid JSON with contexts"
  else
    bad T4d "list --json invalid: $out"
  fi
fi

# ================================================================ T5: cancel task
t
echo
echo "→ T5: cancel running task"

# Start a task that would take time (asking it to wait/think)
"$WK" run "$PROVIDER" --mode auto --cwd "$TEST_CWD" \
  "Think carefully about the meaning of life, then respond." \
  >/dev/null 2>&1 &
pid=$!

CTX2=$(wait_state working 15)

if [ -n "$CTX2" ]; then
  ok T5a "background task started and is working ($CTX2)"

  "$WK" cancel "$CTX2" >/dev/null 2>&1; rc=$?
  check T5b "cancel exits 130" 130 "$rc"

  wait $pid 2>/dev/null; rc=$?
  check T5c "canceled run exits 130" 130 "$rc"

  out=$("$WK" get "$CTX2" 2>&1)
  contains T5d "get shows state=canceled" "state=canceled" "$out"

  # Context should still be usable for send (though the provider may fail the task)
  out=$("$WK" send "$CTX2" "Now echo: post-cancel" 2>&1); rc=$?
  # Send is accepted (creates new task), but provider may fail it
  # Both exit 0 (success) and exit 1 (provider error) are acceptable
  if [ "$rc" = 0 ] || [ "$rc" = 1 ]; then
    ok T5e "context survives cancel; send accepted (rc=$rc)"
  else
    bad T5e "send after cancel rejected with unexpected rc=$rc"
  fi
else
  bad T5a "failed to start background task or detect working state"
  skip_test T5b "cancel exits 130"
  skip_test T5c "canceled run exits 130"
  skip_test T5d "get shows canceled"
  skip_test T5e "context survives cancel"
fi

# ================================================================ T6: timeout handling
t
echo
echo "→ T6: timeout handling"

# Start a task with short timeout
"$WK" run "$PROVIDER" --mode auto --cwd "$TEST_CWD" \
  "Take your time to think deeply before responding." \
  --timeout 3 >/dev/null 2>"$TEST_CWD/t6.err" &
pid=$!

wait $pid 2>/dev/null; rc=$?

if [ "$rc" = 12 ]; then
  ok T6a "timeout exits 12"

  err=$(cat "$TEST_CWD/t6.err")
  if echo "$err" | grep -q "timeout"; then
    ok T6b "stderr mentions timeout"
  else
    bad T6b "stderr doesn't mention timeout: $err"
  fi

  CTX3=$(ctx_of "$TEST_CWD/t6.err")
  if [ -n "$CTX3" ]; then
    ok T6c "got context id from timeout run"

    # Task should still be running
    sleep 1
    out=$("$WK" get "$CTX3" 2>&1)
    if echo "$out" | grep -qE "state=(working|submitted)"; then
      ok T6d "task continues running after timeout"
    else
      skip_test T6d "task may have completed quickly"
    fi

    # Clean up
    "$WK" cancel "$CTX3" >/dev/null 2>&1 || true
  else
    bad T6c "no context id from timeout run"
    skip_test T6d "task continues running"
  fi
else
  bad T6a "timeout exit code was $rc, expected 12"
  skip_test T6b "timeout message"
  skip_test T6c "context from timeout"
  skip_test T6d "task continues"
fi

# ================================================================ T7: close context
t
echo
echo "→ T7: close context"

if [ -n "${CTX2:-}" ]; then
  "$WK" close "$CTX2" >/dev/null 2>&1; rc=$?
  check T7a "close exits 0" 0 "$rc"

  "$WK" send "$CTX2" "test" >/dev/null 2>&1; rc=$?
  check T7b "send to closed context exits 2" 2 "$rc"

  "$WK" list 2>/dev/null | grep -q "$CTX2" && bad T7c "closed context still in list" || ok T7c "closed context hidden from list"

  "$WK" list --all 2>/dev/null | grep -q "$CTX2.*closed" && ok T7d "list --all shows closed" || bad T7d "list --all missing closed ctx"
else
  skip_test T7a "close exits 0"
  skip_test T7b "send to closed fails"
  skip_test T7c "closed hidden from list"
  skip_test T7d "list --all shows closed"
fi

# ================================================================ T8: persistence across daemon restart
t
echo
echo "→ T8: persistence across daemon restart"

if [ -n "${CTX1:-}" ]; then
  "$WK" daemon stop >/dev/null 2>&1
  sleep 2

  # Daemon should auto-restart
  out=$("$WK" result "$CTX1" 2>/dev/null); rc=$?
  check T8a "result after restart exits 0" 0 "$rc"

  if [ -n "$out" ]; then
    ok T8b "result text survived restart"
  else
    bad T8b "result text empty after restart"
  fi

  "$WK" list --all >/dev/null 2>&1
  check T8c "list works after restart" 0 $?
else
  skip_test T8a "result after restart"
  skip_test T8b "data survived"
  skip_test T8c "list works"
fi

# ================================================================ T9: working task interrupted by daemon stop
t
echo
echo "→ T9: working task interrupted by daemon stop"

"$WK" run "$PROVIDER" --mode auto --cwd "$TEST_CWD" \
  "Think deeply for a while before responding." \
  >/dev/null 2>&1 &
pid=$!

CTX4=$(wait_state working 15)

if [ -n "$CTX4" ]; then
  ok T9a "background task started ($CTX4)"

  "$WK" daemon stop >/dev/null 2>&1
  sleep 2
  kill $pid 2>/dev/null || true
  wait $pid 2>/dev/null || true

  # Restart daemon and check task state
  "$WK" daemon start >/dev/null 2>&1
  sleep 1

  out=$("$WK" get "$CTX4" --json 2>&1)
  if echo "$out" | grep -q '"state": "failed"'; then
    ok T9b "interrupted task marked as failed"
  elif echo "$out" | grep -q '"state": "canceled"'; then
    ok T9b "interrupted task marked as canceled (acceptable)"
  else
    bad T9b "interrupted task has unexpected state: $out"
  fi
else
  bad T9a "failed to start background task for interrupt test"
  skip_test T9b "interrupted task state"
fi

echo
summary
