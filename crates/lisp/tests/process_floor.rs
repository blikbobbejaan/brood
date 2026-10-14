//! **The per-process memory floor is a ratchet.** Counted bytes (the counting allocator,
//! `%mem-bytes`) per PARKED process — the state every one of N live processes is in at
//! once, so the state that sets peak memory — must not exceed the figure below.
//!
//! It exists because the floor regressed by 328 B per process without anything noticing:
//! `spawn` propagated the (empty) package context to every child through `cold_mut()`,
//! which allocated the `ColdHeap` that M1 (`runtime-frontier.md` §B) had moved off the
//! worker floor — for two months, on every process ever spawned. Found 2026-09-21 by an
//! allocation histogram, not by any gate. This is that gate: it reads the per-process
//! figure and refuses a step up. Move the bound DOWN when a shaving lands; move it up
//! only with a sentence saying what the bytes buy.
//!
//! Measured in the `test` profile (opt-level 2, debug assertions): 4 439 B/proc before the
//! ColdHeap fix, 4 120 after it, 3 824 after the per-process maps became `SmallMap`s
//! (release: 4 690 / 4 345 / 3 957). The bound sits ~4% above the measured figure, which
//! is the only way a 328 B step reads as a step: at 4 600 the sabotage passed.
//! Sabotage-verified: restoring the unconditional `cold_mut()` read 4 439 against a 4 300
//! bound and reds this.
//!
//! Raised 4 000 → 4 020 (2026-10-14): the computed-call memo (ADR-406) costs 16 B per process
//! — one nullable pointer, the table itself allocated only by a process that calls a closure
//! value from native code — and buys ~240 instructions on every such call. Measured 3 976–3 985
//! before it, 3 995–3 996 after, which left 4 B under the old bound against ~9 B of run-to-run
//! jitter.
//!
//! Raised 4 020 → 4 040 (2026-10-14): KI-246's hash memo (`Heap::hash_memo`, a lazily boxed
//! `OnceCell`) is a second 8 B pointer, and it buys nested-set hashing in linear time.
//! Measured 4 020–4 021 with both memos, which left no headroom against the jitter above.

use brood::Interp;

const FLOOR_BYTES: i64 = 4040;

#[test]
fn a_parked_process_costs_no_more_than_the_floor() {
    // The floor is the VM's with shared arms, the shipped configuration. The tree-walker
    // (`BROOD_TIER=0`, CI's differential job) has no floor to ratchet: its `spawn` copies the
    // spawner's whole env frame into the child — here the growing `acc` list — so a process
    // costs O(n) and the fleet O(n²). Skipped by ceiling rather than absorbed into the bound.
    if brood::eval::compile::tier_ceiling() == brood::eval::compile::Tier::TreeWalk {
        eprintln!("process_floor: not the shipped engine configuration — nothing to ratchet");
        return;
    }
    let mut interp = Interp::new();
    let prog = r#"
        (def n 20000)
        (defn parked () (receive ([:go] nil)))
        ;; NOT `(spawn-hold (+ i 1) (cons (spawn (parked)) acc))`: under the tree-walker
        ;; (`BROOD_TIER=0`, CI's differential job) a spawn copies the spawner's whole env
        ;; frame into the child, and a frame holding the growing `acc` makes the fleet
        ;; O(n²) — 20k children asked for a 7 GiB block and a 7 GB GitHub runner was
        ;; shut down rather than failed, three pushes running (2026-09-21). A `mapv`
        ;; frame holds one index and nothing that grows.
        (defn spawn-all () (mapv (range n) (fn (_) (spawn (parked)))))
        (def b0 (%mem-bytes))
        (def kids (spawn-all))
        (sleep 200)
        (def per (math/quot (- (%mem-bytes) b0) n))
        ;; Release them so the runtime tears down cleanly.
        (fold kids nil (fn (_ p) (send p [:go])))
        per
    "#;
    let v = interp.eval_str(prog).expect("the probe ran");
    let per: i64 = interp.print(v).parse().expect("an integer byte count");
    eprintln!("parked process: {per} B/proc counted (floor {FLOOR_BYTES})");
    assert!(
        per <= FLOOR_BYTES,
        "a parked process costs {per} B of counted allocation, over the {FLOOR_BYTES} B floor — \
         something new is allocated per spawn; run the allocation histogram \
         (docs/runtime-frontier.md §B) before raising this number"
    );
}
