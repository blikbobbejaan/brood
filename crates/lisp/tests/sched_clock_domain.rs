//! Pins the **clock domain** of a `receive` deadline: it lives on the scheduler clock
//! (`process::timer::sched_now`), and every gate that decides "has it elapsed?" must read
//! that same clock — never `Instant::now()`.
//!
//! **On native the two are the same clock**, so no behavioural test here can tell a
//! correct gate from a broken one. The divergence exists only under
//! `wasm32-unknown-unknown`, where there is no timer thread and `sched_now()` is a FROZEN
//! logical clock that only `timer::fire_next_timer` advances. (`scripts/wasm-suite.sh` —
//! `make wasm-test`, and a CI step — runs the real wasm artifact under node, and its
//! clock-domain case hangs if a gate regresses.)
//!
//! What the mismatch costs: a deadline minted as `sched_now() + ms` but compared against
//! real time in `park_on_receive` reads "passed" as soon as a snippet has computed for
//! `ms` of wall-clock, and re-queues; the re-scan's own gate on the logical clock reads
//! "not yet" and suspends again. suspend → park → re-queue, forever: 100% CPU, a frozen
//! browser tab (verified on a real wasm32 build, 2026-08-25).
//!
//! **Since 2026-10-15 the rule is enforced by construction.** A deadline is a
//! `SchedDeadline`, whose `Instant` field is private to `process/timer.rs`, and `sched_now`
//! is private there too: outside that module a deadline can only be minted
//! (`after_millis`), tested (`reached`) or waited on (`remaining`) against the scheduler
//! clock, so mixing in `Instant::now()` does not compile. This test pins the construction —
//! the privacy that makes it hold, and that every carrier of a deadline holds the newtype
//! rather than a bare `Instant` — so loosening either is a deliberate act that has to edit
//! this file.

use std::path::Path;

/// Read a repo file relative to the workspace root.
fn source(rel: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

#[test]
fn the_scheduler_clock_and_a_deadline_s_instant_never_leave_the_timer_module() {
    let timer = source("crates/lisp/src/process/timer.rs");
    assert!(
        timer.contains("pub struct SchedDeadline(Instant);"),
        "`SchedDeadline`'s `Instant` must stay a PRIVATE field — a `pub` field (or a \
         reshaped struct) lets a caller compare it against real time again"
    );
    for line in timer.lines().filter(|l| l.contains("fn sched_now(")) {
        assert!(
            line.trim_start().starts_with("fn sched_now("),
            "`sched_now` must stay private to the timer module; outside it the clock is \
             read only through a `SchedDeadline` (offending line: {})",
            line.trim()
        );
    }
}

#[test]
fn every_carrier_of_a_receive_deadline_holds_the_newtype() {
    let carriers = [
        (
            "crates/lisp/src/process/mailbox.rs",
            "recv_deadline: Option<SchedDeadline>",
        ),
        (
            "crates/lisp/src/process/mailbox.rs",
            "deadline: Option<SchedDeadline>) {",
        ),
        (
            "crates/lisp/src/error.rs",
            "deadline: Option<crate::process::SchedDeadline>",
        ),
        (
            "crates/lisp/src/eval/compile.rs",
            "deadline: Option<crate::process::SchedDeadline>",
        ),
        (
            "crates/lisp/src/eval/compile/ir.rs",
            "deadline: Option<crate::process::SchedDeadline>",
        ),
    ];
    for (file, needle) in carriers {
        assert!(
            source(file).contains(needle),
            "{file} no longer carries a receive deadline as `SchedDeadline` (`{needle}`) — \
             a bare `Instant` there is outside the clock-domain guarantee"
        );
    }
}
