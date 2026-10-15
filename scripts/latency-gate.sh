#!/usr/bin/env bash
# Tail-latency gate — run on the benchmark box, not CI (a timing gate on a shared runner cannot
# tell a regression from a noisy neighbour). Runs `stress/latency_probe.blsp` RUNS times and
# fails when any run saw too many quanta over 1 ms (`:long-schedule`, ADR-404 — the structural
# signal: code that could not be preempted) or when the WORST p99 is over its budget. p99.9 and
# max are reported, not gated: on an open-loop row they move run to run by 3× on this box.
#
#   scripts/latency-gate.sh [RUNS]          env: BROOD=<binary> MAX_LONG=<n> MAX_P99_US=<µs>
#
# Measured 2026-10-13 (i5-11500H, 12 cores, 20k req/s, 5% fat, 20 000 requests): p99 85–115 µs
# on v0.37.0, 76–80 µs after ADR-400..405, with 2–11 long quanta a run. (v0.37.0 predates
# `:long-schedule`, so its count reads 0 — `BROOD_STALL_MS=1` saw nearly one per fat request.)
set -uo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/run-cap.sh
RUNS="${1:-5}"
BIN="${BROOD:-target/release-fast/brood}"
MAX_LONG="${MAX_LONG:-200}"
MAX_P99_US="${MAX_P99_US:-250}"
[ -x "$BIN" ] || { echo "latency-gate: no brood binary at $BIN — build it first"; exit 2; }

fail=0
worst_p99=0
for i in $(seq 1 "$RUNS"); do
    out=$( (ulimit -v "$RUN_CAP_KB"; BENCH_N="${BENCH_N:-20000}" timeout 300 "$BIN" stress/latency_probe.blsp) 2>&1)
    metric() { printf '%s\n' "$out" | sed -n "s/^#metric $1=\(-*[0-9][0-9]*\)$/\1/p" | tail -1; }
    p50=$(metric p50_us); p99=$(metric p99_us); p999=$(metric p999_us); max=$(metric max_us)
    long=$(metric long_schedules)
    if [ -z "$p99" ] || [ -z "$long" ]; then
        echo "run $i: the probe printed no metrics — it failed:"
        printf '%s\n' "$out" | tail -5
        exit 2
    fi
    echo "run $i: p50=${p50}µs p99=${p99}µs p99.9=${p999}µs max=${max}µs long_schedules=${long}"
    [ "$p99" -gt "$worst_p99" ] && worst_p99=$p99
    if [ "$long" -lt 0 ] || [ "$long" -gt "$MAX_LONG" ]; then
        echo "  FAIL: $long quanta held a worker over 1 ms (budget $MAX_LONG) — something stopped being preemptible"
        fail=1
    fi
done
if [ "$worst_p99" -gt "$MAX_P99_US" ]; then
    echo "FAIL: worst p99 ${worst_p99}µs over the ${MAX_P99_US}µs budget"
    fail=1
fi
[ "$fail" -eq 0 ] && echo "latency-gate: ok (worst p99 ${worst_p99}µs over $RUNS runs)"
exit "$fail"
