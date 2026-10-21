#!/usr/bin/env bash
# `make test-contracts` — the in-language suite with every `sig` armed as a runtime
# contract, the way `nest test` runs a project (ADR-381).
#
# `make test` runs the same suite through its cargo wrapper (crates/lisp/tests/suite.rs),
# which leaves contracts OFF: armed, the suite costs twice the CPU (251 s against 126 s,
# 2026-10-17) on a box `make test` already saturates, and that would push the standard run
# past its five-minute budget. But armed is what every `nest test` and `nest run` user gets,
# so a contract shim that breaks a caller, or a test that only passes unarmed, must still go
# red somewhere — here, in `make green-all`, and in CI's examples job.
#
# Passes only on a zero exit AND a summary line reporting no failure: a runner that died
# before printing one is not a pass.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/run-cap.sh"
NEST="${NEST:-$ROOT/target/release/nest}"
[ -x "$NEST" ] || { echo "test-contracts: no nest at $NEST — build with 'cargo build --release -p cli -p nest' (or set NEST=)"; exit 2; }
log="$(mktemp -t brood-test-contracts.XXXXXX)"
cd "$ROOT"
( ulimit -v "$RUN_CAP_KB"; env -u BROOD_CONTRACTS BROOD_CONTRACTS=1 "$NEST" test ) >"$log" 2>&1
status=$?
summary="$(grep -E '^[0-9]+ tests, [0-9]+ passed, [0-9]+ failed' "$log" | tail -1)"
if [ $status -eq 0 ] && [ -n "$summary" ] && printf '%s\n' "$summary" | grep -q ', 0 failed'; then
  echo "test-contracts: $summary (contracts armed)"
  rm -f "$log"
  exit 0
fi
echo "test-contracts: FAILED (exit $status) — ${summary:-no summary line: the runner died}"
# Each failure record whole — its `test failed: group › name` header and the indented
# assert/actual/expect lines under it, up to the blank line that ends it. A plain grep for
# `error:` also matched every `process N died: error: …` line the suite's crash tests print
# on purpose, and twenty of those filled the window: CI's log of 2026-10-21 said
# "1 failed" and never which test.
records="$(sed 's/\x1b\[[0-9;]*m//g' "$log" | awk '/test failed: /{on=1} on{print} on&&/^[[:space:]]*$/{on=0}' | head -60)"
if [ -n "$records" ]; then
  printf '%s\n' "$records" | sed 's/^/    /'
else
  # No record to show (the runner died before reporting): the errors, minus that noise.
  grep -E 'error:' "$log" | grep -v ' died: ' | head -20 | sed 's/^/    /'
fi
echo "    full log: $log"
exit 1
