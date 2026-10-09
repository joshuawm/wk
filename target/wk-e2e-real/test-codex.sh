#!/usr/bin/env bash
# Real E2E tests for wk with Codex CLI

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

PROVIDER="codex"
TEST_TIMEOUT=120

echo "==== Testing wk with Codex CLI ===="
echo

# Check if Codex is available
if ! type -p codex >/dev/null 2>&1; then
  echo "⊘ Codex CLI not found - skipping tests"
  echo
  echo "==== wk E2E Real (Codex): 0 passed, 0 failed, 0 skipped (0 test groups, 0 assertions) ===="
  exit 0
fi

echo "TODO: Implement Codex integration tests"
echo "Codex-specific features to test:"
echo "  - Steer (inject prompt during working turn)"
echo "  - Background work management"
echo "  - Thread goals"
echo

# Placeholder for future implementation
pass=0
fail=0
skip=0
tno=0

echo
summary
