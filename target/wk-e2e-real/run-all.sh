#!/usr/bin/env bash
# Main entry point for wk real E2E tests

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Configuration
export WK="${WK:-$HOME/wk-target/debug/wk}"
export WK_HOME="${WK_HOME:-/tmp/wk-e2e-real-home}"
export TEST_CWD="${TEST_CWD:-/tmp/wk-e2e-real-cwd}"
export WK_MAX_DEPTH=3

# Add Claude CLI to PATH if available
export PATH="$HOME/bin:$PATH"

echo "==== wk Real E2E Test Suite ===="
echo "wk binary: $WK"
echo "WK_HOME: $WK_HOME"
echo "Test CWD: $TEST_CWD"
echo

# Check environment
echo "→ Checking environment..."
if ! bash "$SCRIPT_DIR/check-env.sh"; then
  echo
  echo "ERROR: Environment check failed"
  exit 1
fi
echo

# Clean up any previous state
echo "→ Cleaning up previous state..."
rm -rf "$WK_HOME" "$TEST_CWD"
mkdir -p "$WK_HOME" "$TEST_CWD"
pkill -f 'wk daemon run' 2>/dev/null || true
sleep 1
echo

# Source environment check results (just get the variables, don't run checks again)
CLAUDE_AVAILABLE=0
if type -p claude >/dev/null 2>&1; then CLAUDE_AVAILABLE=1; fi
export CLAUDE_AVAILABLE

CODEX_AVAILABLE=0
if type -p codex >/dev/null 2>&1; then CODEX_AVAILABLE=1; fi
export CODEX_AVAILABLE

OPENCODE_AVAILABLE=0
if type -p opencode >/dev/null 2>&1; then OPENCODE_AVAILABLE=1; fi
export OPENCODE_AVAILABLE

# Run provider-specific tests
EXIT_CODE=0

if [ "${CLAUDE_AVAILABLE:-0}" = "1" ]; then
  echo "========================================"
  echo "Running Claude Code tests..."
  echo "========================================"
  if ! bash "$SCRIPT_DIR/test-claude.sh"; then
    EXIT_CODE=1
  fi
  echo
fi

if [ "${CODEX_AVAILABLE:-0}" = "1" ]; then
  echo "========================================"
  echo "Running Codex tests..."
  echo "========================================"
  if ! bash "$SCRIPT_DIR/test-codex.sh"; then
    EXIT_CODE=1
  fi
  echo
else
  # Run stub that will skip gracefully
  bash "$SCRIPT_DIR/test-codex.sh" || true
  echo
fi

if [ "${OPENCODE_AVAILABLE:-0}" = "1" ]; then
  echo "========================================"
  echo "Running OpenCode tests..."
  echo "========================================"
  if ! bash "$SCRIPT_DIR/test-opencode.sh"; then
    EXIT_CODE=1
  fi
  echo
else
  # Run stub that will skip gracefully
  bash "$SCRIPT_DIR/test-opencode.sh" || true
  echo
fi

# Final cleanup
echo "→ Final cleanup..."
"$WK" daemon stop >/dev/null 2>&1 || true
rm -rf "$WK_HOME" "$TEST_CWD"
echo

if [ $EXIT_CODE = 0 ]; then
  echo "==== All tests passed! ===="
else
  echo "==== Some tests failed ===="
fi

exit $EXIT_CODE
