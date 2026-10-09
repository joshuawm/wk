#!/usr/bin/env bash
set -x
export PATH="$HOME/bin:$PATH"
export WK="$HOME/wk-target/debug/wk"
export WK_HOME="/tmp/test-t5e"
export TEST_CWD="/tmp/test-t5e-cwd"

rm -rf "$WK_HOME" "$TEST_CWD"
mkdir -p "$WK_HOME" "$TEST_CWD"
pkill -f "wk daemon" 2>/dev/null || true
sleep 2

echo "=== Testing T5e: send after cancel ==="

# Start a background task
"$WK" run claude --mode auto --cwd "$TEST_CWD" "Think carefully for a moment" >/dev/null 2>&1 &
pid=$!

# Wait for it to start
sleep 5

# Get the context
CTX=$("$WK" list --json 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)
cs = d.get("contexts", [])
if cs:
    print(cs[0]["id"])
')

echo "Context: $CTX"

if [ -z "$CTX" ]; then
    echo "FAIL: No context found"
    exit 1
fi

# Cancel it
echo "Canceling..."
"$WK" cancel "$CTX" 2>&1
cancel_rc=$?
echo "Cancel exit: $cancel_rc"

wait $pid 2>/dev/null || true

# Check state
echo "Checking state..."
"$WK" get "$CTX" 2>&1

# Try to send to canceled context
echo "Sending to canceled context..."
if "$WK" send "$CTX" "Echo: post-cancel" 2>"/tmp/t5e-err.txt"; then
    echo "SUCCESS: Send to canceled context worked (exit 0)"
    exit 0
else
    rc=$?
    echo "FAIL: Send to canceled context failed with exit $rc"
    echo "Error output:"
    cat "/tmp/t5e-err.txt"
    exit 1
fi
