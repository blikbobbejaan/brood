#!/usr/bin/env bash
# The runtime soak: boot stress/runtime_soak.blsp fresh, round after round, for a fixed time,
# and stop at the first round that fails. Each round races first module loads across a
# process fan, runs warm spawn bursts, link/monitor churn and large copies (see the .blsp).
#
# It exists for the two concurrency bugs whose cause was never found — KI-88 (a spawn never
# scheduled) and KI-127 (an autoload fan hung silently) — which are dormant behind watchdogs
# and would otherwise be seen only by accident. Rounds cycle through the tiers, a single
# worker, and the GC verifier, because each changes the interleavings.
#
# A round FAILS on: a nonzero exit, a hang past the per-round deadline, or any runtime
# report on stderr (the stranded-work watchdog, a lost process, the loader's BUG backstop,
# a panic). The failing round's stdout+stderr are kept; so is a one-line record of EVERY
# round — a hunt that keeps nothing on success cannot be believed afterwards.
#
# Usage: stress/runtime_soak.sh [SECONDS]      (default 600)
#        BROOD=path/to/brood  SOAK_LOG_DIR=dir  ROUND_TIMEOUT=seconds
set -u
cd "$(dirname "$0")/.."
BROOD=${BROOD:-target/release/brood}
duration=${1:-600}
round_timeout=${ROUND_TIMEOUT:-120}
log_dir=${SOAK_LOG_DIR:-target/runtime-soak}
mkdir -p "$log_dir"
summary="$log_dir/rounds.log"
: >"$summary"

# Lines the runtime prints only when something is wrong.
alarms='STRANDED WORK|\[sched\] LEDGER|\[require\] BUG|lost: the scheduler|panicked at|use-after-GC|stale handle'

deadline=$(($(date +%s) + duration))
round=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  case $((round % 6)) in
    0) environment="BROOD_TIER=2" ;;
    1) environment="BROOD_TIER=1" ;;
    2) environment="BROOD_TIER=0" ;;
    3) environment="BROOD_J=1" ;;
    4) environment="BROOD_GC_VERIFY=1" ;;
    5) environment="BROOD_J=2 BROOD_TIER=2" ;;
  esac
  output="$log_dir/round-$round.log"
  started=$(date +%s%N)
  # shellcheck disable=SC2086 # $environment is a list of VAR=value words.
  env $environment timeout --kill-after=10 "$round_timeout" "$BROOD" stress/runtime_soak.blsp >"$output" 2>&1
  status=$?
  elapsed=$((($(date +%s%N) - started) / 1000000))
  verdict=ok
  if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
    verdict="HUNG (no exit within ${round_timeout}s)"
  elif [ "$status" -ne 0 ]; then
    verdict="FAILED (exit $status)"
  elif grep -qE "$alarms" "$output"; then
    verdict="ALARM on stderr"
  elif ! grep -q '^soak round ok:' "$output"; then
    verdict="NO SUMMARY LINE"
  fi
  echo "round $round [$environment] ${elapsed} ms: $verdict" >>"$summary"
  if [ "$verdict" != ok ]; then
    echo "runtime soak: round $round [$environment] $verdict — log kept at $output"
    echo "--- last 40 lines ---"
    tail -40 "$output"
    echo "--- preserve this binary ($BROOD) with the log; $summary has every round ---"
    exit 1
  fi
  rm -f "$output"
  round=$((round + 1))
done
echo "runtime soak: $round rounds clean in ${duration}s ($summary)"
