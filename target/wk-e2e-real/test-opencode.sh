#!/usr/bin/env bash
# Real E2E tests for wk with OpenCode CLI

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

PROVIDER="opencode"
TEST_TIMEOUT=120

echo "==== Testing wk with OpenCode CLI ===="
echo

# Check if OpenCode is available
if ! type -p opencode >/dev/null 2>&1; then
  echo "⊘ OpenCode CLI not found - skipping tests"
  echo
  echo "==== wk E2E Real (OpenCode): 0 passed, 0 failed, 0 skipped (0 test groups, 0 assertions) ===="
  exit 0
fi

echo "TODO: Implement OpenCode integration tests"
echo "OpenCode-specific features to test:"
echo "  - Session adoption"
echo "  - Service interaction"
echo "  - Directory management"
echo

# Placeholder for future implementation
pass=0
fail=0
skip=0
tno=0

echo
summary
