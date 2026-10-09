# wk Real E2E Tests

Real end-to-end integration tests for `wk` with actual coding agent CLIs (Claude Code, Codex, OpenCode).

## Overview

This test suite validates `wk` integration with real provider CLIs, unlike the previous `wk-e2e` which used a fake provider. These tests:

- Test actual end-to-end integration with real CLIs
- Verify core workflows (run, send, cancel, timeout, persistence)
- Check daemon lifecycle management
- Validate cross-turn continuity and context management

## Requirements

### Environment
- Linux or WSL (wk is unix-only currently)
- At least one coding agent CLI installed:
  - `claude` (Claude Code)
  - `codex` (Codex CLI)
  - `opencode` (OpenCode)
- Valid authentication/subscription for the provider(s)

### Build
```bash
# Build wk in debug mode (from WSL/Linux)
cd /path/to/waku/waku
CARGO_TARGET_DIR=$HOME/wk-target cargo build -p wk
```

## Running Tests

### All providers
```bash
cd target/wk-e2e-real
./run-all.sh
```

### Specific provider
```bash
# Claude Code only
cd target/wk-e2e-real
export WK=$HOME/wk-target/debug/wk
./test-claude.sh
```

### Configuration
Environment variables:
- `WK`: Path to wk binary (default: `$HOME/wk-target/debug/wk`)
- `WK_HOME`: Test home directory (default: `/tmp/wk-e2e-real-home`)
- `TEST_CWD`: Test working directory (default: `/tmp/wk-e2e-real-cwd`)

## Test Coverage

### Claude Code Tests (test-claude.sh)
- **T1**: Daemon lifecycle (start, stop, status, auto-start)
- **T2**: Simple query task (basic run and completion)
- **T3**: Continuation (send for new turns)
- **T4**: Read-only commands (get, list, result)
- **T5**: Cancel running task
- **T6**: Timeout handling
- **T7**: Close context
- **T8**: Persistence across daemon restart
- **T9**: Working task interrupted by daemon stop

### Codex Tests (test-codex.sh)
TODO: Not yet implemented

### OpenCode Tests (test-opencode.sh)
TODO: Not yet implemented

## Differences from Fake Tests

| Aspect | Fake (wk-e2e) | Real (wk-e2e-real) |
|--------|---------------|---------------------|
| Provider | `fake-claude.py` | Real CLIs |
| Responses | Predictable/scripted | Actual AI responses |
| Validation | Exact string matching | Functional validation |
| Execution | Offline | Requires network/auth |
| Speed | Fast (~30s) | Slower (~2-5 min) |
| CI | Always runnable | Needs credentials |
| Coverage | Protocol edge cases | Real usage scenarios |

## Known Limitations

1. **Authentication**: Tests assume providers are already authenticated
2. **Network**: Requires internet connection for provider API calls
3. **Timing**: Real providers have variable latency; timeouts are generous
4. **Cost**: May incur API usage costs depending on provider billing
5. **Flakiness**: Provider availability or rate limits may cause failures

## Troubleshooting

### "No supported provider CLIs found"
Ensure at least one provider CLI is in PATH:
```bash
which claude codex opencode
```

### "daemon status before start exits 3" fails
Previous daemon may still be running:
```bash
pkill -f 'wk daemon run'
rm -rf /tmp/wk-e2e-real-home
```

### Tests timeout
Increase `TEST_TIMEOUT` in the test script or check provider authentication:
```bash
claude --version
# Should not prompt for auth
```

### Windows Support
Currently `wk` is unix-only. Windows support is planned but not yet implemented.

## Next Steps

1. Implement Codex-specific tests (steer, background work)
2. Implement OpenCode tests once wk supports it
3. Add permission flow tests (mode=ask)
4. Add question flow tests (AskUserQuestion)
5. Add Windows support and tests
