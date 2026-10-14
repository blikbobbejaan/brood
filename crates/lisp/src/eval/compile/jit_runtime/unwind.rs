//! Native preempt-unwind (ADR-401): how a reduction budget that runs out inside a chain of
//! native activations reaches the scheduler.
//!
//! The bytecode driver can only park a process whose whole stack is interpreter frames — a
//! native activation's state lives in registers and on the OS stack. So a native loop used
//! to be preemptible only when it was the activation the driver itself had entered: one
//! called from another native arm took its preempt as a deopt, and the caller re-ran it on a
//! *nested* interpreter that cannot capture either — the loop finished there, slower and
//! still unpreempted. Any warm request handler calling a warm loop was one quantum, however
//! long the loop ran (the latency row's fat handlers read 1–2 ms each).
//!
//! Now the preempt travels outward instead. The preempted activation's caller handler sees
//! outcome 2, records the callee here and parks [`LispError::preempt_unwind`]; every caller
//! between it and the driver exits through its error path, which first journals its own
//! state at the call in flight ([`begin_in_call_journal`]), and its handler records it in
//! turn. The driver then rebuilds each recorded activation as an interpreter frame — a
//! caller paused just after its `Call`, its operand stack restored from the journal; the
//! preempted one at its back edge — and parks the process. When it resumes, the innermost
//! frame re-tiers into native code and the callers finish interpreted.
//!
//! The in-call journal is a side record, not a frame slot: the operands go to the top of
//! `roots`, which nothing touches until the driver rebuilds (every handler on the way out
//! leaves `roots` as it lies), so they are rooted without a journal area in the frame. A
//! pure self-recursive arm, which journals nothing else, needs no area at all — that area
//! and its per-activation reset were 10% of `bintree`.
//!
//! Only an activation entered with [`crate::jit::JitCallCtx::unwind`] set may start this, and
//! that bit is 1 only where every frame up to a top-level driver can be rebuilt: the driver
//! sets it, a native→native call passes it on, everything else (a builtin calling back, a
//! nested evaluation, the tree-walker) builds its context with 0.

use super::*;

/// Where a caller the unwind passed was paused: just after the `Call` at `ip`, with `depth`
/// operands beneath its result, held at `roots[at..at + depth]`.
#[derive(Clone, Copy)]
pub(crate) struct InCallJournal {
    pub(crate) ip: usize,
    pub(crate) depth: usize,
    pub(crate) at: usize,
}

thread_local! {
    /// The native activations a preempt is unwinding through, innermost first: the preempted
    /// one, then each caller paused mid-call with its journal. Pushed by the native-call
    /// handlers as the signal passes them, drained by the driver that rebuilds them
    /// ([`take_unwound`]). Per thread, not per process: an unwind runs start to finish on the
    /// worker it began on — nothing can park or migrate a process while a native frame of it
    /// is still on that worker's stack.
    static UNWOUND: std::cell::RefCell<Vec<(UnwindFrame, Option<InCallJournal>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The journal the activation that just left wrote on its way out, waiting for the
    /// handler that records it ([`record_unwound`]).
    static IN_CALL: std::cell::Cell<Option<InCallJournal>> = const { std::cell::Cell::new(None) };
}

/// Hand the driver the activations the current unwind passed, innermost first.
pub(crate) fn take_unwound() -> Vec<(UnwindFrame, Option<InCallJournal>)> {
    UNWOUND.with(|u| std::mem::take(&mut *u.borrow_mut()))
}

/// Record one more activation the current unwind passed, with the in-call journal it wrote
/// leaving (the driver's own frame, last).
pub(crate) fn record_unwound(frame: UnwindFrame) {
    let journal = IN_CALL.take();
    UNWOUND.with(|u| u.borrow_mut().push((frame, journal)));
}

/// An arm's error path, leaving while the call at `ip` is in flight with `depth` operands
/// beneath it (`brood_rt_in_call_journal`). For a preempt unwinding through, reserve `depth`
/// slots at the top of `roots` for the caller to store those operands into, note where,
/// and return them; for a real error, null — nothing will rebuild this activation.
///
/// # Safety
/// On a non-null return the caller must store `depth` valid `Value`s into the room before
/// anything can allocate or collect: the reserved slots are live roots holding
/// uninitialised memory until then (`Heap::push_roots_room`).
pub(crate) unsafe fn begin_in_call_journal(heap: &mut Heap, ip: usize, depth: usize) -> *mut Value {
    let unwinding = heap
        .jit_pending_error
        .as_ref()
        .is_some_and(|e| e.is_preempt_unwind());
    if !unwinding {
        return std::ptr::null_mut();
    }
    let at = heap.roots_len();
    // SAFETY: forwarded to our caller, the native error path, which stores every slot at once.
    let room = unsafe { heap.push_roots_room(depth) };
    IN_CALL.set(Some(InCallJournal { ip, depth, at }));
    room
}

/// One native activation a preempt is unwinding through — see [`UNWOUND`].
pub(crate) struct UnwindFrame {
    /// The arm whose native code was running.
    pub(crate) arm: Arc<CompiledArm>,
    /// Where its frame starts in `roots`, and the size it was built to (which tells the
    /// small layout from the leaf-spliced one — [`jit_frame_layout`]).
    pub(crate) base: usize,
    pub(crate) nslots: usize,
    /// Its environment and IC-block cursors, as the frame that called it installed them.
    pub(crate) env: EnvId,
    pub(crate) bases: (u32, u32),
}

/// A native callee came back with `outcome`, having been entered with
/// `JitCallCtx::unwind == unwind`. If that is a preempt this activation may honour, or the
/// unwind of one passing through, record the callee (built by `callee`) and leave its frame
/// where it lies: returns `true`, and the caller must report the parked error WITHOUT
/// touching `roots`. Otherwise `false`, and the caller handles the outcome as it always has.
pub(crate) fn jit_unwind_through(
    heap: &mut Heap,
    outcome: i64,
    unwind: bool,
    callee: impl FnOnce(&Heap) -> Option<UnwindFrame>,
) -> bool {
    let passing = outcome == 3
        && heap
            .jit_pending_error
            .as_ref()
            .is_some_and(|e| e.is_preempt_unwind());
    if passing {
        // Only a callee entered with `unwind` set can start or pass an unwind — the bit
        // cannot be 0 here unless the protocol itself broke, and
        // then the frames already recorded could not be rebuilt by anyone.
        assert!(
            unwind,
            "a preempt unwound out of an activation entered without `unwind` (ADR-401)"
        );
        let frame = callee(heap)
            .expect("a native activation the preempt unwound through could not be named (ADR-401)");
        record_unwound(frame);
        return true;
    }
    if outcome == 2 && unwind {
        let Some(frame) = callee(heap) else {
            // Cannot name the arm (a swap moved it under us): take the preempt the old
            // way — the caller re-runs it on the VM. Correct, only unpreempted.
            return false;
        };
        UNWOUND.with(|u| {
            let mut u = u.borrow_mut();
            debug_assert!(
                u.is_empty(),
                "a second preempt began while one was unwinding"
            );
            u.clear();
            // The preempted activation left through its preempt exit, not its error path,
            // so it wrote no journal; clear one a real error may have left behind.
            IN_CALL.set(None);
            u.push((frame, None));
        });
        heap.jit_pending_error = Some(crate::error::LispError::preempt_unwind());
        crate::perf_bump!(jit_preempt_unwind);
        return true;
    }
    false
}

/// The arm a caller paused mid-call continues in on the VM: the one whose bytecode its
/// native code was lowered from — the leaf-spliced layout's resume arm, or the arm itself.
/// `None` for the self-spliced layout, whose ips match no chunk — and whose callees may
/// never unwind (`JitCallCtx::no_child_unwind`), so it is never recorded mid-call.
pub(crate) fn jit_in_call_resume_arm(
    arm: &Arc<CompiledArm>,
    frame_nslots: usize,
) -> Option<Arc<CompiledArm>> {
    match jit_frame_layout(arm, frame_nslots) {
        FrameLayout::LeafSpliced => Some(arm.leaf.as_ref()?.resume.clone()),
        FrameLayout::Small => Some(arm.clone()),
        FrameLayout::SelfSpliced => None,
    }
}
