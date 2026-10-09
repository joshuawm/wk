#!/usr/bin/env bash
# Check environment prerequisites for wk real E2E tests

set -euo pipefail

# Check if running in WSL or native Linux
if [ ! -f /proc/version ]; then
  echo "ERROR: Not running in Linux/WSL environment"
  exit 1
fi

# Check if wk binary exists
if [ -z "${WK:-}" ] || [ ! -x "$WK" ]; then
  echo "ERROR: WK environment variable not set or binary not executable"
  echo "Expected: export WK=/path/to/wk"
  exit 1
fi

echo "✓ wk binary: $WK"

# Check provider CLIs
check_provider_cli() {
  local provider=$1
  local cli_name=$2
  local path=$(type -p "$cli_name" 2>/dev/null || true)

  if [ -n "$path" ]; then
    echo "✓ $provider CLI found: $path"
    return 0
  else
    echo "⊘ $provider CLI not found: $cli_name"
    return 1
  fi
}

# Check available providers
CLAUDE_AVAILABLE=0
CODEX_AVAILABLE=0
OPENCODE_AVAILABLE=0

if check_provider_cli "Claude Code" "claude"; then
  CLAUDE_AVAILABLE=1
fi

if check_provider_cli "Codex" "codex"; then
  CODEX_AVAILABLE=1
fi

if check_provider_cli "OpenCode" "opencode"; then
  OPENCODE_AVAILABLE=1
fi

# At least one provider must be available
if [ $CLAUDE_AVAILABLE -eq 0 ] && [ $CODEX_AVAILABLE -eq 0 ] && [ $OPENCODE_AVAILABLE -eq 0 ]; then
  echo
  echo "ERROR: No supported provider CLIs found in PATH"
  echo "Please install at least one of: claude, codex, opencode"
  exit 1
fi

echo
echo "Environment check passed!"
echo "Available providers:"
[ $CLAUDE_AVAILABLE -eq 1 ] && echo "  - Claude Code"
[ $CODEX_AVAILABLE -eq 1 ] && echo "  - Codex"
[ $OPENCODE_AVAILABLE -eq 1 ] && echo "  - OpenCode"

# Export availability flags for test scripts
export CLAUDE_AVAILABLE
export CODEX_AVAILABLE
export OPENCODE_AVAILABLE

exit 0
