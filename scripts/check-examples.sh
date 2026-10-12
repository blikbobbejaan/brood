#!/usr/bin/env bash
# Does every program in `examples/` still RUN against this build?
#
# `examples/` is outside `make test`, `nest check` and the breakage suite, so a rename
# sweep that misses it lets it rot silently (KI-45). This is the cheap counter.
#
# What it checks, and why that and not "exit 0". Several examples cannot COMPLETE in a
# sandbox — `webserver`/`node_server` are servers that run until killed, `node_client` needs
# a peer, `font-zoom` needs `--features gui`. Their exit codes are environment noise. But
# **an `unbound symbol` diagnostic is never environment noise**: it means a name the example
# uses no longer exists, which is precisely what every rot above looked like. So each
# example is run briefly and its diagnostics are scanned; a program that reaches its own
# graceful "server not running" error has passed.
#
#   scripts/check-examples.sh              # every example
#   scripts/check-examples.sh life tour    # a subset
#   BROOD=path/to/brood scripts/check-examples.sh
#
# Exits non-zero if any example names something unbound, so it can gate.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# WHICH binary, and which modules it carries — see `scripts/lib/gate-binary.sh` for both
# questions and why each one had a wrong answer here (KI-76's class).
. "$ROOT/scripts/lib/gate-binary.sh"
BROOD="${BROOD:-$(gate_pick brood)}"
NEST="${NEST:-$(gate_pick nest)}"
RUN_SECS="${RUN_SECS:-8}"

# Example PROJECTS whose suite is knowingly red — named here so the skip prints on every
# run rather than hiding, exactly as `BREAKAGE_SKIP` does for the breakage suite. Empty
# now that `examples/editor` (the one known-red project, KI-45) was deleted — brood-edit is
# the real editor project, so the in-repo duplicate was removed rather than kept limping.
SKIP_PROJECTS="${SKIP_PROJECTS:-}"

gate_require_fresh "$BROOD"
gate_load_modules "$BROOD"

want=("$@")
matches() {
  [ ${#want[@]} -eq 0 ] && return 0
  local n="$1" w
  for w in "${want[@]}"; do [ "$w" = "$n" ] && return 0; done
  return 1
}

# A project suite passed only when `nest test` exited 0 AND printed its summary line with
# exactly zero failures. Matching a bare `0 failed` accepted "10 failed"; a missing summary
# (the runner died before printing) must fail too.
project_suite_passed() {
  local exit_status="$1" output="$2"
  [ "$exit_status" -eq 0 ] || return 1
  printf '%s\n' "$output" | grep -qE '^[0-9]+ tests, [0-9]+ passed, 0 failed([,( ]|$)'
}

fail=0
echo ">>> checking examples with $BROOD (${RUN_SECS}s each)"

# --- single-file examples --------------------------------------------------------------
for f in "$ROOT"/examples/*.blsp; do
  name="$(basename "$f" .blsp)"
  matches "$name" || continue
  out="$(cd "$ROOT" && timeout "$RUN_SECS" "$BROOD" "$f" 2>&1)"
  gate_classify "$out"
  case "$GATE_VERDICT" in
    ok)   echo "  ok      $name" ;;
    skip) echo "  skip    $name ($GATE_DETAIL)" ;;
    *)    fail=1; echo "  FAIL    $name"; printf '%s\n' "$GATE_DETAIL" | sed 's/^/            /' ;;
  esac
done

# --- examples that live in a SUBDIRECTORY without a project.blsp ------------------------
# `examples/hot-reload/main.blsp` is neither `examples/*.blsp` nor an example project, so the
# first version of this script skipped it — and that is exactly the file that was broken
# (a bare `reload-on-change` after ADR-229, and a bare `greet` that module namespacing had
# turned into `greeter/greet`). A gap in the checker is indistinguishable from a passing check.
for f in "$ROOT"/examples/*/*.blsp; do
  dir="$(dirname "$f")"
  [ -f "$dir/project.blsp" ] && continue          # handled as a project below
  name="$(basename "$dir")/$(basename "$f" .blsp)"
  matches "$(basename "$dir")" || matches "$(basename "$f" .blsp)" || [ ${#want[@]} -eq 0 ] || continue
  out="$(cd "$ROOT" && timeout "$RUN_SECS" "$BROOD" "$f" 2>&1)"
  gate_classify "$out"
  case "$GATE_VERDICT" in
    ok)   echo "  ok      $name" ;;
    skip) echo "  skip    $name ($GATE_DETAIL)" ;;
    *)    fail=1; echo "  FAIL    $name"; printf '%s\n' "$GATE_DETAIL" | sed 's/^/            /' ;;
  esac
done

# --- example PROJECTS (a project.blsp + tests/) -----------------------------------------
for p in "$ROOT"/examples/*/project.blsp; do
  [ -f "$p" ] || continue
  dir="$(dirname "$p")"; name="$(basename "$dir")"
  matches "$name" || continue
  case " $SKIP_PROJECTS " in *" $name "*)
    echo "  skip    $name/ (named in SKIP_PROJECTS)"; continue ;;
  esac
  if [ ! -x "$NEST" ]; then
    echo "  skip    $name/ (no nest binary at $NEST)"; continue
  fi
  out="$(cd "$dir" && timeout 120 "$NEST" test 2>&1)"
  status=$?
  if project_suite_passed "$status" "$out"; then
    echo "  ok      $name/ ($(printf '%s\n' "$out" | grep -oE '[0-9]+ tests, [0-9]+ passed' | head -1))"
  else
    fail=1
    echo "  FAIL    $name/"
    printf '%s\n' "$out" | grep -E 'test failed|unbound|error' | head -3 | sed 's/^/            /'
  fi
done

echo
if [ $fail -ne 0 ]; then echo ">>> examples: FAILURES above"; exit 1; fi
echo ">>> examples: all clean"
