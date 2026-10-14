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

## State (2026-10-14)

- **Released:** v0.38.0 (`CHANGELOG.md`). On `main` since, merged 2026-10-14 from a branch
  developed apart from the release: preemption reaches every shape of running code (ADR-400..405,
  KI-230..238), one native entry and one quantum module, folds walked in spans, the gate sweep,
  and handoff items B (the fold span result stops allocating) and A (ADR-406, the computed-call
  memo). That branch's KI and ADR numbers were moved at the merge; commit messages from before
  it carry the old ones — the map is in the devlog entry "merged origin/main".
- `known-issues.md` has one **watching** entry, KI-239 (a dist test node's connect refused right
  after its peer was seen listening; retry-absorbed, not reproducible on demand, diagnostic
  armed). KI-226..228 (per-runtime resolver memos, a listener out of descriptors, the lingering
  TCP close) were found and fixed in the 2026-10-13 stability pass, and KI-229 (the soft memory
  limit stopped a bystander; now it stops the largest holder, ADR-399) on 2026-10-14, then
  KI-240..245 (dotted lists unsendable, refused threads panicking, fast allocators reaching
  the hard abort, a stuck victim shielding the rest, lost stdout exiting 0, a `receive`
  under `reflect/eval` re-running its form).
- **Gates on the merged tree** (2026-10-14): `make test-both` 1859/1859 on both engines,
  `make gcstress`, fuzz (`tier_transition`, `metamorphic`, `pipeline`: 1 350 programs, clean),
  clippy `--all-features`, the `--no-default-features` checks, the JIT/frame/preemption/
  `process_floor`/`doc_refs` Rust tests, and the pre-push steps (`nest check` and `--strict` at
  zero, the 45 test files the merge touches). `make prepush` itself stops at `nest format
  --check` on `stress/fuzz_out/fuzz_6.blsp`, a git-ignored fuzz output CI never sees. Not run
  since 2026-10-13: `latency-gate`, `check-examples`, `check-stress`, `check-imaged`,
  `breakagetests`.
- **Memory ceiling:** decided (ADR-407). Inside a memory-capped container `brood`/`nest run`
  default their limits from the cap (hard = 3/4 of it); elsewhere they stay unlimited.
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

**C. A thin-wrapper callback costs ~1 900 instructions a call from native code** — named or
not. `(fn (acc x) (+ acc x))`, the canonical reducer, is a passthrough (ADR-69): the dispatcher
never runs its body, it redirects to `+` by NAME on every call — `passthrough_arm`, `env_get`,
`boundary_redirect`, a second hop to `+`'s own passthrough arm, then `call_native` (profile:
`dispatch` 35–38%, `env_get` ~6–11%). The compiled body would be one inlined `Prim2`. The redirect
exists to spare the tree-walker and VM a frame; from native code it is the slow path. Measure
with `MODE=passanon` in a loop like devlog 2026-10-14's before choosing between running the
compiled arm from native code and caching the redirect target (late binding: the inner head is
re-resolved per call today, so a cache must be epoch-stamped).

**B. DONE (2026-10-13, devlog "(later) — the fold span result stops allocating").** A finished
span returns the accumulator; a stopped one returns `(:%fold-more acc . next)`. `nqueens` −1.3%,
one-span folds ~145 instructions cheaper each, `reduce` flat. Two leftovers, neither urgent:
`Reducer::resolve` per span (`reduce_prim_op` re-derives `+`'s primitive every span), and the
stopped-short path's `first`/`rest` are runtime callbacks (~150 instructions a span, once per
4 096 elements). **Read the devlog's measurement lesson before trusting a few-percent
instruction delta between two builds:** a comment-only prelude edit moved rows by up to 2.3%.

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
