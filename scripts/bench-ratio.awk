# bench-ratio.awk — parse `cargo bench --bench eval` (engine-grid) output and print each
# engine's load-robust ratio against the reference engine, per workload.
#
# The eval benches pin each row to an engine via `set_forced_engine`, so every size N
# appears as ADJACENT rows — `(Vm, N)`, `(Tw, N)`, … — in the same process. Under load they
# slow down together, so the ratio holds where absolute times wander (docs/benchmarking.md
# §1). Ratio < 1 means that engine beats the reference. Engine labels come from
# `Engine::short()`, so a new engine shows up as an extra column without touching this.
# `Tw` (the tree-walker) is the reference: the stable in-process baseline, not the subject.
#
# Usage: awk -f scripts/bench-ratio.awk <bench-output-file>   (ANSI colour already stripped)

function strip_tree(line) {
  while (match(line, /^(│|├|╰|└|─|┬|[ \t])/)) line = substr(line, RLENGTH + 1)
  return line
}

function unit_scale(unit) {
  if (unit == "ns") return 1
  if (unit == "µs" || unit == "us") return 1e3
  if (unit == "ms") return 1e6
  return 1e9
}

# Fill time_value[] / time_unit[] with every `<number> <unit>` timing in `line` (a unit must
# end at a word boundary, so `5 samples` is not `5 s`); return how many.
function find_times(line,    found, rest, token, after) {
  found = 0
  rest = line
  while (match(rest, /[0-9.]+[ \t]*(ns|µs|us|ms|s)/)) {
    token = substr(rest, RSTART, RLENGTH)
    after = substr(rest, RSTART + RLENGTH, 1)
    rest = substr(rest, RSTART + RLENGTH)
    if (after ~ /[A-Za-z0-9_]/) continue
    found++
    match(token, /[0-9.]+/)
    time_value[found] = substr(token, RSTART, RLENGTH)
    time_unit[found] = substr(token, RSTART + RLENGTH)
    gsub(/[ \t]/, "", time_unit[found])
  }
  return found
}

BEGIN { reference = "Tw"; current = "?" }

{
  line = strip_tree($0)
  if (line == "") next
  if (match(line, /\([A-Za-z][A-Za-z0-9_]*,[ \t]*[0-9]+\)/)) {
    leaf = substr(line, RSTART + 1, RLENGTH - 2)
    split(leaf, parts, ",")
    engine = parts[1]
    size = parts[2] + 0
    if (find_times(line) >= 3) {   # fastest, slowest, median, mean[, ...]: take the median
      if (!(current in bench_seen)) { bench_seen[current] = 1; bench_order[++bench_count] = current }
      key = current SUBSEP size
      if (!(key in size_seen)) { size_seen[key] = 1; sizes[current] = sizes[current] " " size }
      shown[current, size, engine] = time_value[3] " " time_unit[3]
      nanos[current, size, engine] = time_value[3] * unit_scale(time_unit[3])
      if (engine != reference && !(engine in engine_seen)) {
        engine_seen[engine] = 1; engine_order[++engine_count] = engine
      }
    }
    next
  }
  # a parent bench-fn line: a bare identifier with no timing columns
  label = line
  sub(/  .*/, "", label)
  gsub(/^[ \t]+|[ \t]+$/, "", label)
  if (label ~ /^[A-Za-z_][A-Za-z0-9_]*$/ && label != "eval" && find_times(line) == 0) current = label
}

END {
  if (engine_count == 0) {
    print "bench-ratio: only (" reference ", N) rows in input — nothing to compare against." > "/dev/stderr"
    exit 1
  }
  header = sprintf("%-24s%11s%14s", "bench", "size", "tree-walker")
  for (e = 1; e <= engine_count; e++) header = header sprintf("%14s%9s", engine_order[e], engine_order[e] "/" reference)
  rows = 0
  for (b = 1; b <= bench_count; b++) {
    bench = bench_order[b]
    count = split(sizes[bench], size_list, " ")
    # numeric insertion sort of this bench's sizes
    for (i = 2; i <= count; i++) {
      held = size_list[i]
      for (j = i - 1; j >= 1 && size_list[j] + 0 > held + 0; j--) size_list[j + 1] = size_list[j]
      size_list[j + 1] = held
    }
    for (i = 1; i <= count; i++) {
      size = size_list[i]
      # emit the row whenever the REFERENCE was measured; a missing subject engine prints
      # "-" rather than the size silently vanishing
      if (!((bench, size, reference) in nanos)) continue
      if (rows++ == 0) { print header; dashes = sprintf("%*s", length(header), ""); gsub(/ /, "-", dashes); print dashes }
      output = sprintf("%-24s%11s%14s", bench, size, shown[bench, size, reference])
      notes = ""
      for (e = 1; e <= engine_count; e++) {
        engine = engine_order[e]
        if (!((bench, size, engine) in nanos)) { output = output sprintf("%14s%9s", "-", "-"); continue }
        if (nanos[bench, size, reference] == 0) {
          # too fast to resolve, not "the same speed" — never report an unmeasured parity
          output = output sprintf("%14s%9s", shown[bench, size, engine], "n/a")
          note = engine " unresolved (reference measured 0)"
        } else {
          ratio = nanos[bench, size, engine] / nanos[bench, size, reference]
          output = output sprintf("%14s%9.2f", shown[bench, size, engine], ratio)
          if (ratio < 0.98) note = sprintf("%s %.0f%% faster", engine, (1 - ratio) * 100)
          else if (ratio > 1.02) note = sprintf("%s %.0f%% slower", engine, (ratio - 1) * 100)
          else note = engine " ~parity"
        }
        notes = notes (notes == "" ? "" : ", ") note
      }
      print output (notes == "" ? "" : "  " notes)
    }
  }
  if (rows == 0) {
    print "bench-ratio: no (" reference ", N) reference rows found in input." > "/dev/stderr"
    print "  (run on `cargo bench --bench eval` engine-grid output)" > "/dev/stderr"
    exit 1
  }
}
