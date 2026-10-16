# Handoff — the current state, and where to pick up

**This page is the current picture only, rewritten in place each session — not a log.** The
narrative and the measurements live in [`devlog.md`](devlog.md) (dated entries, newest at the
bottom); decisions in [`decisions.md`](decisions.md); bugs in
[`known-issues.md`](known-issues.md); the perf option book in
[`runtime-frontier.md`](runtime-frontier.md) and [`compute-frontier.md`](compute-frontier.md);
how to measure, the traps, and what needs a quiet benchmark box in
[`benchmarking.md`](benchmarking.md) (§7; the answered cross-session tasks are in
[`archive/perf-handoff.md`](archive/perf-handoff.md)). The dated sections this file
used to accumulate (2026-09-04 … 2026-10-05) are in git history; their substance is in the
devlog entries of the same dates.

## State (2026-10-16)

- **Latest work (2026-10-15/16, devlog "the call into native code, 32% cheaper" and "the
  interpreted call, 10% cheaper"):** the interpreter → native call 636 → 434 instructions
  (settled arms skip the tier-up; no shared-arm atomic write per entry), the interpreted call
  771 → 691. Rows: default ceiling `ring` −3.3%, `nbody`/`regex`/`spawn` ~−3%, `json` −1.6%;
  `BROOD_TIER=1` every row ≤ 0 but `sort` (+0.04%), `fib` −3.4%. Code generation itself is unchanged — the one IR
  change tried (`push_room` inlined) moved `json` +2.5% and was reverted. Six call-path
  experiments are recorded as measured-and-dropped; the remaining call costs are structural.
- **Don't build the debug profile while a suite runs:** `cargo build` relinks
  `target/debug/brood` under the running tests (it reddened `cli::stdimage_reporting`
  2026-10-16, reproduced on demand). Release builds (`make release-brood`) are safe.
- **2026-10-15/16, the maintenance review** (devlog entries of 2026-10-15, the last one
  "two bugs (KI-262, KI-263), one formatter walk, the 2024 edition"). Dependencies are on
  latest with MSRV 1.96, and the workspace is on the **2024 edition** (rustfmt still
  `style_edition = "2021"`; the reformat is its own commit when wanted). Two bugs were found
  and fixed:
  - **KI-262:** after `unlink` plus a zero-timeout flush, an `[:EXIT]` could still arrive
    later. Guard: `crates/lisp/tests/unlink_race.rs`.
  - **KI-263:** `nest format` deleted the blank line below a trailing comment.

  Large files were split by job, and the ADR log now lives in `docs/decisions/` by number
  range. `process.rs` documents the lock order in one table. All of this was verified with
  targeted tests on tiers 0–2, GC stress and fuzz. **Run on the other machine 2026-10-16:**
  `make test-both` found four reds, all stale tests from those moves (devlog "the merged
  tree's first full `test-both`"), fixed — now 1897/1897 on both engines. CI's red on
  `fec73f3f` was a test race, KI-267, also fixed.
- **Released:** v0.38.0 (`CHANGELOG.md`). On `main` since, merged 2026-10-14 from a branch
  developed apart from the release: preemption reaches every shape of running code (ADR-400..405,
  KI-230..238), one native entry and one quantum module, folds walked in spans, the gate sweep,
  and handoff items B (the fold span result stops allocating) and A (ADR-406, the computed-call
  memo). That branch's KI and ADR numbers were moved at the merge; commit messages from before
  it carry the old ones — the map is in the devlog entry "merged origin/main".
- `known-issues.md` lists **no open or watching entry**. KI-247 (a bystander raising after a
  holder died) and KI-248 (a large allocation passing the hard limit when the live snapshot
  had not seen an earlier large block) were found and fixed 2026-10-14 (night); KI-264 (a native
  tail call by name never hit its head's cache slot) and KI-265 (a memory-pressure test that
  required the selector's stderr line when the culprit had raised in itself) and KI-266 (a stale
  leaf derivation refused for good) 2026-10-15. Those three were KI-250..252 on the other
  machine and were renumbered at the 2026-10-16 merge (this side's KI-250..255 came first).
- **Gates on the merged tree** (2026-10-14): `make test-both` 1859/1859 on both engines,
  `make gcstress`, fuzz (`tier_transition`, `metamorphic`, `pipeline`: 1 350 programs, clean),
  clippy `--all-features`, the `--no-default-features` checks, the JIT/frame/preemption/
  `process_floor`/`doc_refs` Rust tests, and the pre-push steps (`nest check` and `--strict` at
  zero, the 45 test files the merge touches). `nest format --check` no longer reads
  git-ignored files (`make prepush` used to stop on `stress/fuzz_out/fuzz_6.blsp`). On
  `0ba6c797` (2026-10-16): `test-both` 1882/1882 both engines, fuzz (four generators × 300,
  `stale=0`), `gcstress`, `check-stress`, `check-examples`, `check-imaged`, `breakagetests`
  (37/37) and `latency-gate` (worst p99 79 µs) all clean.
- **Memory ceiling:** decided (ADR-407). Inside a memory-capped container `brood`/`nest run`
  default their limits from the cap (hard = 3/4 of it), re-read every few seconds so a
  resized container moves them; elsewhere they stay unlimited. A table holding the memory is
  named in the `E0043`; collecting a table when its creator dies is still a language question.
- **Probes worth keeping:** every value walker survives data nested a million deep; a
  program in `unshare -U` with `ulimit -u N` gets an exact thread budget
  (`crates/cli/tests/thread_exhaustion.rs`); `systemd-run --user --scope -p MemoryMax=…`
  gives a real container cap locally.
- **The cleanup is finished** (devlog entries 2026-10-09 and both of 2026-10-12): the dead
  `BROOD_*` flags and aliases are gone — `BROOD_TIER=0|1|2` is the only tier knob — `nest
  mcp` is Brood, every Python script is gone, closed known-issue bodies live in
  [`archive/known-issues-closed.md`](archive/known-issues-closed.md). Its last four items
  landed too: `tcp/close` silences the socket at once, the two message-copy walkers are one,
  ability devirtualization is always on (`BROOD_MONO` deleted), and the doc catalogue
  derives its categories. Nothing from it is deferred.
- **Before pushing:** `make prepush`, and `git merge --ff-only origin/main` first if it moved.
- **Is the tree green?** Answer with `make green`, never by reading the run list. On this
  machine run targeted tests only, capped (`( ulimit -v 16000000; … -j1 )`).
- **Not brood, noted:** bedit at CI's pin passes 2463/2464 with a grammar-enabled `nest` — its
  Elixir agent keeps the ANSI-coloured `dbg` header Elixir 1.21-dev prints. `make wasm-test`
  wants wasm-bindgen-cli 0.2.128 (0.2.100 is installed here). The local `../bedit` checkout is a
  detached HEAD 88 commits behind its origin.

## Open threads, in value order

### Next: retire `exec_value`, the VM's second executor (picked 2026-10-16)

`eval/compile/exec_value.rs` holds two things:
- **The prim helpers** (`prim2_int_fast`, `prim_apply`, `prim2_dispatch_rooted`, …). The
  bytecode loop uses them, so they stay; move them to their own file.
- **`exec_value`** (from about line 306), a tree-walking executor over the compiled `Node`
  tree. It reimplements `If`, `Do`, `LetBind`, `Call`, `Prim2`, `TryCatch` and more, so every
  node-level change has to be made twice, and its call-site cache handling has drifted from
  the bytecode loop's once already (the shared helpers `call_site_arm` and
  `vm_call_ic_install` now keep that half in step).

It has exactly three entry points:

1. **Top-level forms**, in `compile.rs` around line 547. `run` builds a throwaway
   `CompiledArm` with no chunk ("Top-level forms run via `exec_value` below, not the bytecode
   loop") and calls `exec_value(heap, &arm.body, base, genv)`.
2. **Optional-parameter defaults**, in `dispatch.rs` around line 594 (`fill_args`):
   `exec_value(heap, node, base, genv)` per missing optional.
3. **`exec_call`**, in `dispatch.rs` at line 12. It is reached only from `exec_value` itself
   (`exec_value.rs:477`), so it goes when the executor goes.

**Plan:**
- Emit a chunk for the top-level arm, as for any arm, and run it through the bytecode
  driver (`vm_run_bc`) instead of `exec_value`.
- Lower optional defaults into the arm's bytecode prologue, or emit each default as a tiny
  chunk.
- Then delete `exec_value` and `exec_call`.

Watch the tail-call contract (`tail_calls_do_not_overflow`), the top-level IC-block install
and restore around a nested `load` (`set_ic_bases`), `has_runtime_handles` and the
`live_arm_push`/`live_arm_truncate` pairing, and `try` at top level (ADR-400 runs `try`
bodies in the bytecode driver).

**Gates:**
- `make test-both`, `make gcstress`, and the fuzz differential.
- `make ab` **and** `make ab-vm`: top-level forms run once, so the cost to watch is the
  per-form setup, which shows on REPL, `eval-string` and test-runner workloads more than on
  benchmark rows.

### Next: performance items (A and B the user's pick, 2026-10-13; C found doing A)

Measure both by **instruction count** first — `perf stat -e instructions:u` with
`BROOD_NO_CHECK=1` (the run pre-flight walk is noise), each binary's stdlib image live (`nest
stdimage`; `make ab` now writes both arms' images itself), the timing binary from
`make release-brood`. Times on this box are noisy; confirm with `make ab --floor` and, for
anything on the scheduler path, `make latency-gate`. `perf record` is capped at 1 kHz here:
aggregate 20-40 runs, and build `make perf-symbols` for names.

**A. DONE (2026-10-14, ADR-406, devlog "native code memoises calls to a closure value").**
`jit_dispatch_call` memoises a computed-head call's arm, env and IC block per closure instance:
~240 instructions off every such call (anonymous closure 943 → 706; named heads 415). What
remains of the gap to a named call is `fill_captures`, `finish_nested`, `native_feedback` and the
unconditional stack probe — an in-IR fast link for computed heads (a site id per computed call,
the handoff's original design) is the next step there; check `staged` (KI-19) and
`mono_devirtualize` when allocating the site.

**C. DONE (2026-10-14, ADR-408).** Native code runs a thin wrapper's compiled arm instead of
the dispatcher's by-name redirect, so the wrapper tiers and links natively: `(fn (acc x) (+ acc
x))` as a callback 1 892 → 714 instructions a call. The interpreters keep the redirect.

**B. DONE (2026-10-13, devlog "(later) — the fold span result stops allocating").** A finished
span returns the accumulator; a stopped one returns `(:%fold-more acc . next)`. `nqueens` −1.3%,
one-span folds ~145 instructions cheaper each, `reduce` flat. Two leftovers, neither urgent:
`Reducer::resolve` per span (`reduce_prim_op` re-derives `+`'s primitive every span), and the
stopped-short path's `first`/`rest` are runtime callbacks (~150 instructions a span, once per
4 096 elements). **Read the devlog's measurement lesson before trusting a few-percent
instruction delta between two builds:** a comment-only prelude edit moved rows by up to 2.3%.

**D. DONE (2026-10-14, night).** `reduce` takes the counted range loop (`nqueens` 113 → 90
ms); a named thin wrapper links from native code (~2 100 → ~420 instructions a call); the
CLIF verifier is off in release (`base64`/`json` −11% pinned); a fold's native callback
finishes inline (746 → 617 instructions an element). Leads left, measured: the fold callback
is still ~617 instructions an element against ~412 for a native→native call by name — the
remaining Rust entry ceremony (`enter_native`, frame nil-fill, captures), whose amortisation
would reshape ADR-403's per-element preemption; and a computed-head call from native code is
652 against ~410 by name (771 before ADR-406's 2026-10-14 amendment). **The in-IR fast link
for computed heads is measured low-value on the corpus:** computed-head calls from native code
per run are `supervisor` 57k, `persistent-map` 50k, `base64` 48k, `json` 25k, `wordcount` 1k,
every other row ~0 — at ~240 instructions saved each, ≤2% of any row. Worth it for user code
built on higher-order loops, not for the benchmark column. `nqueens`' `solve` stays
interpreted (it builds its loop closure; §7.1).

**E. DONE (2026-10-15) — `json`, picked apart by counters.** KI-264 (a native tail call by
name caches its head), KI-266 (a stale leaf derivation is re-validated, not refused for good),
the unlinkable-call slow path reads the call IC instead of walking the environment, `nth`'s
default is a fixed parameter, and a queued callback compiles synchronously after 2 048 declines.
`json` −5.1% instructions on top of KI-264's rows. What is left on `json` (N=20 000, counter
build): `vm_apply` ~470 000 — the functions the native compiler refuses
(`call-mediated-boxed`: `json-value`, `json-slice`, `enc-key`, `enc-string`, `->string`,
`reverse`; `chunk-outside-jit-subset`: `enc-escapes`, `count`), each called from native code
through the slow path. Followed 2026-10-15 (evening): the gate is right (§7.1), so the
boundary was cut instead — a native → interpreted → native round trip −23% (devlog). Left:
the nested `vm_run_bc` trampoline per crossing and `jit_tier_in_frame`'s entry ceremony
(older thread 2), and `fast_link_from_entry` re-checking a callee that is not native on
every call. A spike (devlog 2026-10-15 night) measured that resuming native code through the
driver instead of nesting would be break-even for compute: the lever is the per-call
constants — 768 instructions an interpreted call, 640 into native code, ~410 native to native.
The middle one is now 434 (devlog 2026-10-15 late: settled arms skip the tier-up, the shared
atomic writes are gone); what is left of it is `exec_chunk`'s call-site share (IC probe + its
`Arc` clone, the frame fill), `enter_native`'s save/restore and the stack stamp. Next: the
768 of an interpreted call, then native to native.
The interpreted call is now 691 (devlog 2026-10-16; the quantum TLS merge, a fast-link base
mirror and an in-IR `push_room` were measured and dropped there — the last moved `json` +2.5%).
Native to native is ~255 once the xcall re-lowering lands; a direct i64-convention call that
skips the callee's boxed wrapper was the structural idea, but the wrapper is 0.0% of every
row (an i64 worker is entered once and recurses in registers) — not worth building for the
corpus.

### Older threads

1. **Receive loops run interpreted.** About a quarter of `pingpong`'s instructions are
   `exec_chunk` running process bodies that a `receive` keeps off the JIT. The general lever
   for every process loop; needs native frames that can suspend (or a receive that returns to
   the driver from native code) — an ADR, not a patch.
2. **The rest of the VM call** (~790 instructions after `CallInPlace`): `exec_chunk`'s `Call` +
   callee entry, and `vm_run_bc`'s frame push/pop. The cheap forms were built, measured and
   dropped (devlog 2026-09-25 "(later)"); what remains is structural.
3. **Per-message residue:** `receive_match`, `run_one`, allocation, `Ctx` install — each small;
   count the calls before touching any of them. Profiled 2026-10-16 (`ring`, precise
   sampling): `receive_match` is 9.7% of the row with no source line above 0.6% — no
   hotspot to cut; `run_one` 3.8%, `send` 2.3%, the `Ctx` thread-local + its drop ~2.8%.
   The interpreter running the process bodies (`exec_chunk` 30%) is thread 1's lever.
4. **Refresh the benchmark column** once 1 or 2 lands (protocol in brood-benchmarks'
   `CLAUDE.md`). Not on this machine.
5. **Known limit:** under the tree-walker a caught contract error's position is the raise
   inside `std/contract.blsp`, not the caller — only the VM filters contract machinery out of
   positions (`exec_chunk.rs` `is_contract_arm`). Tier 0 is the legacy engine.

## Flake hunting

A one-in-N race passes N-1 runs; repetition is the evidence. The tools:

- `make gcstress` — the concurrency/mailbox files under `BROOD_GC_STRESS=1 BROOD_GC_VERIFY=1`
  on the debug build (nightly runs it).
- `make test-both` — the suite at the default ceiling and on the tree-walker (`BROOD_TIER=0`).
- `scripts/fuzz/run.sh <generator> [count] [seed]` — the engine differential (tree-walker, VM,
  JIT, GC-stress) over generated programs; `make stress` for the program fuzzer, the reader
  fuzzer and the property/race corpus (nightly runs it).
- `make runtime-soak [SOAK_SECONDS=600]` — boots `stress/runtime_soak.blsp` fresh round after
  round (first module loads raced across a 240-process fan, warm spawn bursts, link churn,
  large copies), cycling tiers, one worker and the GC verifier; stops at the first lost
  process, hang or watchdog line. The hunt for KI-88 and KI-127, whose causes were never
  found; the nightly runs it for 30 minutes and keeps every round's record.
- The nightly workflow (`.github/workflows/nightly.yml`) runs the suite three times, the stress
  suite, ASAN, TSAN + loom, and a `perf-stats` build.
- A flake hunt keeps its logs on success too: a loop that deletes them when green cannot be
  believed afterwards.
- CI and the nightly run the suite through `scripts/nextest-no-flakes.sh`, so a test that
  passes only on nextest's retry fails the job and is named. Before it, `FLAKY 2/2` was green
  and unread (`dns_test` failed its first try in four of six runs before anyone looked).
- Several runtimes in one process is a real configuration (embedders, `cargo test` threads,
  the nightly sanitizer jobs): state that belongs to a runtime lives on `RuntimeCode`, never
  in a `static` (KI-224, KI-226). `crates/lisp/tests/process_wide_statics.rs` makes every
  lock-holding `static` in `crates/lisp/src` state why it is process-wide.
- A test that times out names what it was stuck on: the runner reads the hung worker and the
  processes it spawned before the kill (running / waiting with N unmatched messages / waiting
  with nothing queued).
