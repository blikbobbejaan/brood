#!/usr/bin/env bash
# Reader / evaluator ROBUSTNESS fuzzer — adversarial, mostly-invalid input.
#
# The differential fuzzer (stress/fuzz_programs.blsp) only generates VALID, terminating
# programs, so it never reaches the reader's and evaluator's error paths. This feeds
# `brood` deliberately hostile inputs — random bytes, unbalanced delimiters, truncated
# strings and escapes, giant tokens, weird numerics, deep nesting — and asserts every run
# FAILS GRACEFULLY: no crash signal, no Rust panic (`panicked at` on stderr or a
# `.brood_crash_dump`), no hang. A clean nonzero exit with a diagnostic is the correct
# outcome for bad input. Anything else is kept in stress/fuzz_out/reader_<kind>_<seed>.bin.
#
# Usage: stress/fuzz_reader.sh [--seeds N] [--start S]   (bash's RANDOM, seeded per input)
set -u
cd "$(dirname "$0")/.."
BROOD=${BROOD:-target/release/brood}
seeds=2000; start=1
while [ $# -gt 0 ]; do
  case "$1" in
    --seeds) seeds=$2; shift 2 ;;
    --start) start=$2; shift 2 ;;
    *) echo "usage: $0 [--seeds N] [--start S]" >&2; exit 2 ;;
  esac
done
binary=$(realpath "$BROOD")
[ -x "$binary" ] || { echo "reader-fuzz: no brood binary at $BROOD — build it first" >&2; exit 2; }
outdir=stress/fuzz_out
# the panic hook writes `.brood_crash_dump` to the CWD — a dedicated workdir keeps a
# concurrent run from cross-polluting it
mkdir -p "$outdir/reader_work"
workdir=$(realpath "$outdir/reader_work")
probe=$workdir/reader_probe.blsp

soup=( "(" ")" "[" "]" "{" "}" '"' '\' ";" "'" '`' "," "~" "#" ":" "@" " " $'\n' $'\t'
       a b c d e f g h i j k l m n o p q r s t u v w x y z 0 1 2 3 4 5 6 7 8 9
       "+" "-" "*" "/" "<" ">" "=" "!" "?" "." "_" "|" "&" "^" "%" '$'
       '\n' '\t' '\"' '\x' '\u' "1e" "0x" "." ".." "..." "1/0" "1M" "1.5e999" "-" "+inf"
       "nan" ":" "::" "#(" "#{" "'" '`' ",@" "~@" )
hostile=( "1e999999" "0x" "1/0" "-" "1.2.3" "##" "#:" ".5.5" "1M2" "0b" "1e" "+" ":" "'" '`' ",@" )

# Every draw happens in THIS shell: a `$( … )` subshell reseeds RANDOM unpredictably, which
# would make a kept seed impossible to regenerate. So `inclusive` sets `drawn` instead of
# printing it.
inclusive() { drawn=$(( $1 + (RANDOM * 32768 + RANDOM) % ($2 - $1 + 1) )); }
percent() { (( RANDOM % 1000 < $1 )); }
repeat() { local text=$1 times=$2 out=""; printf -v out '%*s' "$times" ""; printf '%s' "${out// /$text}"; }
soup_draws() { local index; for ((index = 0; index < $1; index++)); do printf '%s' "${soup[RANDOM % ${#soup[@]}]}"; done; }

generate() { # one adversarial input on stdout, biased toward structurally-interesting garbage
  local mode=$(( RANDOM % 1000 )) index format byte
  if (( mode < 250 )); then
    # raw random bytes (possibly invalid UTF-8 — the reader must not choke)
    inclusive 0 400; format=""
    for ((index = 0; index < drawn; index++)); do printf -v byte '\\%03o' $(( RANDOM % 256 )); format+=$byte; done
    printf "$format"
  elif (( mode < 450 )); then
    # unbalanced / blown-out delimiters
    local opens=("(" "[" "{") open=$(( RANDOM % 3 ))
    inclusive 1 2000; repeat "${opens[open]}" "$drawn"
  elif (( mode < 600 )); then
    # deeply nested but balanced — stack-depth stress on the reader
    local depth
    inclusive 1 5000; depth=$drawn
    repeat "(" "$depth"; inclusive 0 5; repeat "1 " "$drawn"; repeat ")" "$depth"
  elif (( mode < 720 )); then
    # truncated string / escape
    printf '"'; inclusive 0 60; soup_draws "$drawn"
    if percent 500; then printf '\\'; fi   # dangling escape at EOF
  elif (( mode < 820 )); then
    # hostile numerics / reader macros
    inclusive 1 40
    for ((index = 0; index < drawn; index++)); do
      (( index > 0 )) && printf ' '
      printf '%s' "${hostile[RANDOM % ${#hostile[@]}]}"
    done
  elif (( mode < 920 )); then
    # a giant single token — no delimiters
    local giants=("a" "1" '"' '\' ";" ":") giant=$(( RANDOM % 6 ))
    inclusive 1000 20000; repeat "${giants[giant]}" "$drawn"
  else
    # structured soup, sometimes wrapped in a form
    inclusive 1 200
    if percent 400; then printf '('; soup_draws "$drawn"; printf ')'
    else soup_draws "$drawn"; fi
  fi
}

classify() { # "KIND detail" for a finding; DIAGNOSED (or nothing) for a graceful run
  rm -f "$workdir/.brood_crash_dump"
  local stderr_file=$workdir/stderr rc
  (cd "$workdir" && BROOD_NO_CHECK=1 timeout 20 "$binary" "$probe" >/dev/null 2>"$stderr_file")
  rc=$?
  case $rc in
    124) echo "HANG no exit within 20s"; return ;;
    132) echo "SIGNAL SIGILL"; return ;;
    133) echo "SIGNAL SIGTRAP"; return ;;
    134) echo "SIGNAL SIGABRT"; return ;;
    135) echo "SIGNAL SIGBUS"; return ;;
    136) echo "SIGNAL SIGFPE"; return ;;
    139) echo "SIGNAL SIGSEGV"; return ;;
  esac
  if grep -q "panicked at" "$stderr_file" || [ -e "$workdir/.brood_crash_dump" ]; then
    local panic_line; panic_line=$(grep -m1 "panicked at" "$stderr_file" | cut -c1-200)
    echo "PANIC ${panic_line:-crash dump written}"
  elif [ -s "$stderr_file" ]; then
    echo "DIAGNOSED"
  fi
}

bad=0; diagnosed=0
for ((seed = start; seed < start + seeds; seed++)); do
  RANDOM=$seed
  generate > "$probe"
  finding=$(classify)
  if [ "$finding" = DIAGNOSED ]; then
    diagnosed=$((diagnosed + 1))
  elif [ -n "$finding" ]; then
    bad=$((bad + 1))
    kind=${finding%% *}
    keep=$outdir/reader_${kind}_${seed}.bin
    cp "$probe" "$keep"
    echo "$kind seed=$seed (${finding#* }) — kept $keep ($(wc -c < "$keep") bytes)"
  elif (( seed % 500 == 0 )); then
    echo "seed $seed ok"
  fi
done
rm -rf "$workdir"
if [ $bad -gt 0 ]; then
  echo "---- reader-fuzz: $bad/$seeds inputs crashed/panicked/hung"
  exit 1
fi
# LIVENESS: almost every input is malformed and earns a diagnostic. None at all means brood
# never ran (a bad path, a broken redirect), not that every input was fine.
if [ $diagnosed -eq 0 ]; then
  echo "---- reader-fuzz: $seeds inputs and not ONE diagnostic — the harness did not run brood"
  exit 1
fi
echo "---- reader-fuzz: $seeds adversarial inputs, all failed gracefully ($diagnosed diagnosed)"
