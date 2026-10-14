//! **Memory pressure: which process the soft memory limit stops** (KI-229).
//!
//! The soft limit (ADR-043) is a count of every byte the allocator holds, so crossing it
//! says the runtime as a whole holds too much — not which process does. Raising it in
//! whichever process next reached a safepoint stopped a bystander: a busy worker making
//! only garbage died, run after run, while the process actually accumulating kept going.
//!
//! So the runtime stops the process that holds the most. Only a process may walk its own
//! heap while it runs, so measuring starts before the crossing: within the last eighth of
//! the limit, every running process that reaches a safepoint measures itself and publishes
//! the figure, at most once per [`PERIOD`]. At the crossing the selector measures every
//! parked process itself, under its mailbox lock, adds each mailbox's queued messages, and
//! stops the largest at once — a hard exit carrying the `E0043` error, so its monitors,
//! links and supervisor see why — and names it on stderr. Nobody else is interrupted.
//! (It cannot wait for fresh figures: a fast runaway crosses the remaining quarter to the
//! hard limit in milliseconds.)
//!
//! Two cases keep the old behaviour, an `E0043` raised in the current process: the largest
//! holder IS the current process (it raises, catchably, as a runaway loop always did), and
//! no process holds even an eighth of what the allocator counts — the memory is in tables,
//! shared code or native buffers, and stopping a process would free nothing attributable.
//!
//! The figures are estimates. A shared blob counts in full for every holder (each keeps it
//! alive); a running process that has not reached a safepoint since the approach began is
//! compared by an older figure. The hard limit stays the backstop for anything this
//! misjudges.

use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::mailbox::{is_alive, Payload, REGISTRY};
use super::message::message_bytes;
use super::scheduler;
use crate::core::heap::Heap;
use crate::error::LispError;

/// How often a running process near the limit re-measures itself.
const PERIOD: Duration = Duration::from_millis(50);

/// A holder below `1/ATTRIBUTABLE` of the allocator's count is not worth stopping.
const ATTRIBUTABLE: usize = 8;

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// The current measuring period's number: a process publishes once per period.
fn period() -> u64 {
    (EPOCH.elapsed().as_millis() / PERIOD.as_millis()) as u64 + 1
}

struct Pressure {
    /// The process last stopped: nobody else is stopped until it is gone, and its memory
    /// with it.
    stopping: Option<u64>,
    /// When a selection last found no process worth stopping. Within a [`PERIOD`] of it
    /// the current process raises without selecting again — a selection walks every
    /// parked heap, and over the limit every safepoint asks.
    unattributable: Option<Instant>,
}

static PRESSURE: Mutex<Pressure> = Mutex::new(Pressure {
    stopping: None,
    unattributable: None,
});

/// Live memory is near the soft limit: measure this process, once per period.
pub(crate) fn near_soft_limit(heap: &Heap) {
    publish(heap);
}

/// The soft limit is crossed (`live` bytes): decide who pays. `Some(error)` means the
/// current process raises it; `None` means carry on — a stopped process is still dying,
/// or another process was just stopped.
pub(crate) fn soft_limit_crossed(heap: &Heap, live: usize) -> Option<LispError> {
    let me = scheduler::current_pid();
    publish(heap);
    // Contended means another process is deciding right now; its decision covers this one.
    let Ok(mut pressure) = PRESSURE.try_lock() else {
        return None;
    };
    if let Some(pid) = pressure.stopping {
        if is_alive(pid) {
            return None;
        }
        pressure.stopping = None;
    }
    if pressure
        .unattributable
        .is_some_and(|since| since.elapsed() < PERIOD)
    {
        return Some(crate::eval::memory_limit_error(live));
    }
    match largest_holder() {
        Some((pid, held)) if held >= live / ATTRIBUTABLE && Some(pid) != me => {
            stop(pid, held, live);
            pressure.stopping = Some(pid);
            None
        }
        Some((pid, held)) if held >= live / ATTRIBUTABLE => {
            debug_assert_eq!(Some(pid), me);
            Some(crate::eval::memory_limit_error(live))
        }
        _ => {
            pressure.unattributable = Some(Instant::now());
            Some(crate::eval::memory_limit_error(live))
        }
    }
}

/// Publish this process's footprint, once per period. A probe at every safepoint near the
/// limit, so it reads the mailbox from the thread's context, not the registry.
fn publish(heap: &Heap) {
    let period = period();
    scheduler::with_current_mailbox(|mailbox| {
        if mailbox.footprint_period.swap(period, Ordering::Relaxed) != period {
            mailbox.footprint.store(heap.footprint(), Ordering::Relaxed);
        }
    });
}

/// The local process holding the most memory — heap plus queued messages — and how much.
fn largest_holder() -> Option<(u64, usize)> {
    REGISTRY
        .entries()
        .into_iter()
        .map(|(pid, mailbox)| {
            let state = crate::core::sync::lock(&mailbox.state);
            let queued: usize = state
                .queue
                .iter()
                .map(|envelope| match &envelope.msg {
                    Payload::Wire(message) => message_bytes(message),
                    // Already in the receiver's heap, which is measured below.
                    Payload::Local { .. } => 0,
                })
                .sum();
            let heap = match &state.waiter {
                Some(process) => process.footprint(),
                None => mailbox.footprint.load(Ordering::Relaxed),
            };
            (pid, heap + queued)
        })
        .max_by_key(|&(_, held)| held)
}

/// Stop `pid` with the memory-limit error as its exit reason, and say so.
fn stop(pid: u64, held: usize, live: usize) {
    let limit = crate::core::alloc::soft_limit();
    let error = crate::eval::memory_holder_error(held, live, limit);
    eprintln!(
        "brood: memory over the soft limit ({} of {} MiB): stopping process {}, which holds {} MiB",
        live >> 20,
        limit >> 20,
        scheduler::proc_descr(pid),
        held >> 20
    );
    scheduler::exit_propagate(pid, super::error_reason(&error));
}
