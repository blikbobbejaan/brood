# Handoff — the current state, and where to pick up

**This page is the current picture only, rewritten in place each session — not a log.** The
narrative and the measurements live in [`devlog.md`](devlog.md) (dated entries, newest at the
bottom); decisions in [`decisions.md`](decisions.md); bugs in
[`known-issues.md`](known-issues.md); the perf option book in
[`runtime-frontier.md`](runtime-frontier.md) and [`compute-frontier.md`](compute-frontier.md);
perf work that needs a quiet benchmark box in [`perf-handoff.md`](perf-handoff.md); how to
measure (and the traps) in [`benchmarking.md`](benchmarking.md). The dated sections this file
used to accumulate (2026-09-04 … 2026-10-05) are in git history; their substance is in the
devlog entries of the same dates.

## State (2026-10-15)

- **Released:** v0.38.0 (`CHANGELOG.md`). On `main` since, merged 2026-10-14 from a branch
  developed apart from the release: preemption reaches every shape of running code (ADR-400..405,
  KI-230..238), one native entry and one quantum module, folds walked in spans, the gate sweep,
  and handoff items B (the fold span result stops allocating) and A (ADR-406, the computed-call
  memo). That branch's KI and ADR numbers were moved at the merge; commit messages from before
  it carry the old ones — the map is in the devlog entry "merged origin/main".
- `known-issues.md` lists **no open or watching entry**. KI-247 (a bystander raising after a
  holder died) and KI-248 (a large allocation passing the hard limit when the live snapshot
  had not seen an earlier large block) were found and fixed 2026-10-14 (night); KI-250 (a native
  tail call by name never hit its head's cache slot) and KI-251 (a memory-pressure test that
  required the selector's stderr line when the culprit had raised in itself) 2026-10-15.
- **Gates on the merged tree** (2026-10-14): `make test-both` 1859/1859 on both engines,
  `make gcstress`, fuzz (`tier_transition`, `metamorphic`, `pipeline`: 1 350 programs, clean),
  clippy `--all-features`, the `--no-default-features` checks, the JIT/frame/preemption/
  `process_floor`/`doc_refs` Rust tests, and the pre-push steps (`nest check` and `--strict` at
  zero, the 45 test files the merge touches). `nest format --check` no longer reads
  git-ignored files (`make prepush` used to stop on `stress/fuzz_out/fuzz_6.blsp`). Not run
  since 2026-10-13: `latency-gate`, `check-examples`, `check-stress`, `check-imaged`,
  `breakagetests`.
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

**E. 2026-10-15 — `json`, picked apart by counters (KI-250 fixed).** A native tail call by
name now caches its head (`primes` −6.4% instructions, `supervisor` −1.8%). Leads measured on
`json` at N=20 000 and not yet taken: `env_get` still runs ~1M times a run (was 1.7M) — find the
callers before guessing; `json/skip-ws`, `digits-run` and `json-object` never get their inlined
native body (`[jit-bail] … reason=leaf-derivation-stale`): the leaf derivation is stamped with
the epoch at compile time, a module loaded later moves the epoch, and nothing re-derives it;
`hof_decline_queued` 19 872 — HOF callbacks declined while their compile is in flight.

### Older threads

1. **Receive loops run interpreted.** About a quarter of `pingpong`'s instructions are
   `exec_chunk` running process bodies that a `receive` keeps off the JIT. The general lever
   for every process loop; needs native frames that can suspend (or a receive that returns to
   the driver from native code) — an ADR, not a patch.
2. **The rest of the VM call** (~790 instructions after `CallInPlace`): `exec_chunk`'s `Call` +
   callee entry, and `vm_run_bc`'s frame push/pop. The cheap forms were built, measured and
   dropped (devlog 2026-09-25 "(later)"); what remains is structural.
3. **Per-message residue:** `receive_match`, `run_one`, allocation, `Ctx` install — each small;
   count the calls before touching any of them.
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
