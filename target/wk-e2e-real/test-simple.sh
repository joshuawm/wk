#!/usr/bin/env bash
set -x
set -euo pipefail

export PATH="$HOME/bin:$PATH"
export WK="$HOME/wk-target/debug/wk"
export WK_HOME="/tmp/test-wk-simple"
export TEST_CWD="/tmp/test-wk-simple-cwd"

# Clean up
rm -rf "$WK_HOME" "$TEST_CWD"
mkdir -p "$WK_HOME" "$TEST_CWD"
pkill -f 'wk daemon' 2>/dev/null || true
sleep 1

echo "=== Testing wk basic functionality ==="
echo "WK=$WK"
echo "WK_HOME=$WK_HOME"

# Test 1: daemon status before start
echo
echo "Test 1: daemon status before start should exit 3"
if "$WK" daemon status 2>&1; then
  echo "FAIL: daemon status should have exited 3"
  exit 1
else
  rc=$?
  if [ "$rc" = "3" ]; then
    echo "PASS: daemon status exited 3 as expected"
  else
    echo "FAIL: daemon status exited $rc, expected 3"
    exit 1
  fi
fi

# Test 2: first command auto-starts daemon
echo
echo "Test 2: first command should auto-start daemon"
if "$WK" list >/dev/null 2>&1; then
  echo "PASS: first command succeeded"
else
  echo "FAIL: first command failed with rc=$?"
  exit 1
fi

# Test 3: daemon status after start
echo
echo "Test 3: daemon status should now exit 0"
if "$WK" daemon status 2>&1; then
  echo "PASS: daemon is running"
else
  echo "FAIL: daemon status failed"
  exit 1
fi

# Test 4: simple run command
echo
echo "Test 4: simple run command with claude provider"
echo "test-file" > "$TEST_CWD/test.txt"

if timeout 30 "$WK" run claude --mode auto --cwd "$TEST_CWD" "Echo: test-marker" 2>"$TEST_CWD/out.err"; then
  echo "PASS: run command completed"
  echo "=== stderr output ==="
  cat "$TEST_CWD/out.err"
  echo "=== end stderr ==="
else
  rc=$?
  echo "FAIL: run command failed or timed out with rc=$rc"
  cat "$TEST_CWD/out.err" 2>/dev/null || true
  exit 1
fi

echo
echo "=== All basic tests passed! ==="
"$WK" daemon stop
