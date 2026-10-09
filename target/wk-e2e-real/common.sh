#!/usr/bin/env bash
# Common utilities for wk real E2E tests

# Test counters
pass=0
fail=0
skip=0
tno=0

# Test functions
t() { tno=$((tno+1)); }
ok()  { pass=$((pass+1)); echo "✓ PASS [$1] $2"; }
bad() { fail=$((fail+1)); echo "✗ FAIL [$1] $2"; }
skip_test() { skip=$((skip+1)); echo "⊘ SKIP [$1] $2"; }

# Assertions
check() { # id desc expected actual
  if [ "$3" = "$4" ]; then
    ok "$1" "$2"
  else
    bad "$1" "$2 — expected [$3] got [$4]"
  fi
}

contains() { # id desc needle haystack
  case "$4" in
    *"$3"*)
      ok "$1" "$2"
      ;;
    *)
      bad "$1" "$2 — missing [$3]; got: $(printf '%s' "$4" | tail -c 240 | tr '\n' '|')"
      ;;
  esac
}

lacks() { # id desc needle haystack
  case "$4" in
    *"$3"*)
      bad "$1" "$2 — unexpectedly contains [$3]"
      ;;
    *)
      ok "$1" "$2"
      ;;
  esac
}

# Context/state helpers
ctx_of() {
  sed -n 's/^\[wk\] ctx=\([a-z0-9]\{6\}\).*/\1/p' "$1" | head -1
}

state_of() {
  sed -n 's/.*state=\([a-z-]*\).*/\1/p' "$1" | head -1
}

task_of() {
  sed -n 's/.*task=\([a-z0-9]*\).*/\1/p' "$1" | head -1
}

# Get newest context from JSON list
newest_ctx() {
  "$WK" list --json 2>/dev/null | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    cs = d.get("contexts", [])
    if cs:
        print(max(cs, key=lambda c: c.get("createdAt", 0))["id"])
except:
    pass
'
}

# Poll until newest context shows expected state
wait_state() { # expected-state max-seconds
  local want=$1 end=$((SECONDS + $2))
  while [ "$SECONDS" -lt "$end" ]; do
    local id state
    read -r id state <<<"$("$WK" list --json 2>/dev/null | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    cs = d.get("contexts", [])
    if cs:
        c = max(cs, key=lambda x: x.get("createdAt", 0))
        print(c["id"], c.get("state", ""))
except:
    pass
')"
    if [ -n "$id" ] && [ "$state" = "$want" ]; then
      echo "$id"
      return 0
    fi
    sleep 0.3
  done
  newest_ctx
}

# Summary
summary() {
  local total=$((pass + fail + skip))
  echo
  echo "==== wk E2E Real: $pass passed, $fail failed, $skip skipped ($tno test groups, $total assertions) ===="
  [ $fail = 0 ]
}

# Cleanup helper
cleanup_test_env() {
  rm -rf "$WK_HOME" "$TEST_CWD" 2>/dev/null
  pkill -f 'wk daemon run' 2>/dev/null || true
  "$WK" daemon stop >/dev/null 2>&1 || true
}

# Provider capability detection
provider_supports_steer() {
  local provider=$1
  # Only Codex is known to support steer
  [ "$provider" = "codex" ]
}

provider_supports_background_work() {
  local provider=$1
  # Codex supports background work
  [ "$provider" = "codex" ]
}
