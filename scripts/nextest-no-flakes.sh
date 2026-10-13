#!/usr/bin/env bash
# Run `cargo nextest run "$@"` and fail if any test passed only on a retry.
#
# `.config/nextest.toml` retries a few binaries once (the in-language suite, the real-TCP
# ones), and a retry that passes reports the test as FLAKY and the run as green. Nothing read
# that line: `dns_test`'s well-formed-reply case failed its first try in four of six runs in
# October 2026 and every one of them was green. The retry stays — it still tells "failed
# once" from "always fails", and the second log is the evidence — but a flaky pass now fails
# the job and names the test.
#
# The log goes to $NEXTEST_LOG (default nextest.log) as well as stdout.
set -uo pipefail
log="${NEXTEST_LOG:-nextest.log}"
cargo nextest run "$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
flaky="$(sed 's/\x1b\[[0-9;]*m//g' "$log" | grep -E '^\s*FLAKY [0-9]+/[0-9]+' || true)"
if [ -n "$flaky" ]; then
  echo "::error::tests passed only on a retry — a flaky test is a failure here:"
  printf '%s\n' "$flaky"
  [ "$status" -eq 0 ] && status=1
fi
exit "$status"
