#!/usr/bin/env bash
# `make gcstress-all` — EVERY in-language test file under BROOD_GC_STRESS + BROOD_GC_VERIFY.
#
# `make gcstress` covers the twelve process/mailbox files where values cross heaps, and runs
# on every push (CI's breakage job). A handle held across a collection is not confined to
# those files, though: any builtin that allocates twice can strand the first allocation, and
# a threshold collection only lands in that window by luck (KI-57). This runs the whole
# suite, file by file, collecting at every safepoint — 291 files in ~80 s at -P 6 on a
# 12-core box (2026-10-17), ~12 CPU-minutes, which is why it is nightly and `green-all`
# rather than per push.
#
# Through `nest test FILE`, not `brood --test`: most files `(:use suite-helpers)` or
# `(:use corpus)` from tests/support/, which only the project runner puts on the load path.
# The DEBUG build, as `make gcstress` uses: a release build with debug-assertions reported
# KI-57 clean on the pre-fix code. A file passes on a zero exit AND a summary line with no
# failure — a runner that died before printing one is not a pass.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/run-cap.sh"
NEST="${NEST:-$ROOT/target/debug/nest}"
JOBS="${JOBS:-$(( $(nproc) / 2 > 0 ? $(nproc) / 2 : 1 ))}"
LOGS="${LOGS:-${TMPDIR:-/tmp}/gcstress-all}"
[ -x "$NEST" ] || { echo "gcstress-all: no nest at $NEST — build with 'cargo build -p cli -p nest'"; exit 2; }
rm -rf "$LOGS"; mkdir -p "$LOGS"
cd "$ROOT"

one() {
  local f="$1" log
  log="$LOGS/$(basename "$f" .blsp).log"
  ( ulimit -v "$RUN_CAP_KB"; BROOD_GC_STRESS=1 BROOD_GC_VERIFY=1 timeout 900 "$NEST" test "$f" ) >"$log" 2>&1
  local status=$?
  local summary
  summary="$(grep -E '^[0-9]+ tests, [0-9]+ passed, [0-9]+ failed' "$log" | tail -1)"
  if [ $status -eq 0 ] && printf '%s\n' "$summary" | grep -q ', 0 failed'; then
    echo "ok $f"
  else
    echo "FAILED $f (exit $status) ${summary:-no summary line}"
  fi
}
export -f one
export NEST LOGS RUN_CAP_KB

results="$(ls tests/*_test.blsp | xargs -P "$JOBS" -I{} bash -c 'one "$@"' _ {})"
total=$(printf '%s\n' "$results" | grep -c .)
failed="$(printf '%s\n' "$results" | grep '^FAILED' || true)"
if [ -n "$failed" ]; then
  printf '%s\n' "$failed"
  printf '%s\n' "$failed" | sed -n 's/^FAILED \([^ ]*\).*/\1/p' | while read -r f; do
    grep -E 'test failed|use-after-GC|stale|panicked' "$LOGS/$(basename "$f" .blsp).log" | head -5 | sed 's/^/      /'
  done
  echo "gcstress-all: $(printf '%s\n' "$failed" | grep -c .) of $total files FAILED (logs: $LOGS)"
  exit 1
fi
echo "gcstress-all: all $total test files clean under GC_STRESS + GC_VERIFY"
