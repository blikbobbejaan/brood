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

## State (2026-10-13)

- **Released:** v0.37.0. Unreleased on `main`, **not pushed** (four commits ahead of
  `origin/main` at the time of writing — check `git log origin/main..`): `67a540e1` preemption
  reaches every shape of running code (ADR-399..404, KI-223..231); `95c8b749` one native entry
  (`jit_runtime/entry.rs`), one quantum module (`process/scheduler/quantum.rs`), folds walked in
  spans; `4f0fa07f` / `3ec72080` the gate sweep (ab-bench's image guard, check-stress, feature
  builds). Devlog entries of 2026-10-12 and 2026-10-13 carry the narrative and the numbers.
- `known-issues.md` lists **no open entry**. Gates last run green on this tree: `make test-both`,
  `make gcstress`, the fuzz pass (13 generators x 100), clippy `--all-features`,
  `make latency-gate`, `check-examples`, `check-stress`, `tier-audit`, `check-imaged`,
  `breakagetests`, the dist tests x5.
- **Before pushing:** `make prepush`, and `git merge --ff-only origin/main` first if it moved.
- **Is the tree green?** Answer with `make green`, never by reading the run list. On this
  machine run targeted tests only, capped (`( ulimit -v 16000000; … -j1 )`).
- **Not brood, noted:** bedit at CI's pin passes 2463/2464 with a grammar-enabled `nest` — its
  Elixir agent keeps the ANSI-coloured `dbg` header Elixir 1.21-dev prints. `make wasm-test`
  wants wasm-bindgen-cli 0.2.128 (0.2.100 is installed here). The local `../bedit` checkout is a
  detached HEAD 88 commits behind its origin.

## Open threads, in value order

### Next: two performance items (the user's pick, 2026-10-13)

Measure both by **instruction count** first — `perf stat -e instructions:u` with
`BROOD_NO_CHECK=1` (the run pre-flight walk is noise), each binary's stdlib image live (`nest
stdimage`; `make ab` now writes both arms' images itself), the timing binary from
`make release-brood`. Times on this box are noisy; confirm with `make ab --floor` and, for
anything on the scheduler path, `make latency-gate`. `perf record` is capped at 1 kHz here:
aggregate 20-40 runs, and build `make perf-symbols` for names.

**A. Calling a closure value from native code has no inline cache.** A computed-head call
(`(f acc x)` with `f` a local) is lowered with `site = NO_SITE` (`eval/compile/lower.rs`, the
`let site = match callee` block: only a `Node::Global` head gets a call-site IC). In native code
it becomes `brood_rt_call_slow` → `jit_dispatch_call` (`jit_runtime/dispatch.rs`), whose computed
branch runs `compiled_arm_for(heap, id, argc)` and `vm_arm_block(&a)` on EVERY call, plus an
unconditional `stack_headroom_ok()` probe, before `enter_native`. The in-IR fast link and the
§7.5 inline fast frame exist only for named heads.
- *Measured:* a Brood loop calling a closure reducer per element cost ~410 instructions more per
  call than the native loop's `hof_resolve`-once path (`closure.rs` `HofArm`), and ~7 400 per short
  fold — `nqueens` +24% when `fold` briefly walked per element in Brood (devlog 2026-10-13).
- *Why it matters:* every higher-order function written in Brood (`map`, `filter`, `seq/*`,
  user HOFs, `fold`'s own per-element walk) pays it; it is the "build the language up" fix
  CLAUDE.md asks for, where the alternative is more Rust loops.
- *Starting design:* give computed-head calls a site too, and cache at it the last closure
  TEMPLATE seen (the arm identity — instances differ per closure created, the arm does not) ->
  `(Arc<CompiledArm>, block bases)`, checked by epoch like the named-head IC (ADR-096,
  `core/heap/vm_cache.rs`). The env comes from the instance each call. Then the hit path can
  skip `compiled_arm_for`/`vm_arm_block`, and later take the in-IR fast link. Check the site
  allocation's interaction with `site_pos`, `staged` (KI-19) and `mono_devirtualize`.
- *Guards to keep green:* `crates/lisp/tests/frame_size_callsites.rs`, the JIT differential
  (`tests/jit*`, fuzz `tier_transition`), `make tier-audit`, a hot-reload test that rebinds a
  closure's defining fn mid-loop (the cache must miss on the epoch).

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
- The nightly workflow (`.github/workflows/nightly.yml`) runs the suite three times, the stress
  suite, ASAN, TSAN + loom, and a `perf-stats` build.
- A flake hunt keeps its logs on success too: a loop that deletes them when green cannot be
  believed afterwards.
