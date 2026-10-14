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
//! hard limit in milliseconds.) A victim that has not died within [`STOP_GRACE`] is inside
//! a long native call and reaches no safepoint to die at; the next crossing looks past it
//! and stops the largest holder that can.
//!
//! Two cases keep the old behaviour, an `E0043` raised in the current process: the largest
//! holder IS the current process (it raises, catchably, as a runaway loop always did), and
//! no process holds even an eighth of what the allocator counts — the memory is in tables,
//! shared code or native buffers, and stopping a process would free nothing attributable.
//! Then the tables are measured too, and the error names the one holding the most, if it
//! holds an eighth: a table belongs to no process, so naming it is all the limit can do.
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

/// How long a stopped process has to die before the selector looks past it. A hard exit
/// lands at the victim's next safepoint, normally within microseconds; one inside a long
/// native call (a subprocess wait, a huge sort) reaches none until the call returns, and
/// meanwhile the rest of the runtime keeps allocating toward the hard limit.
const STOP_GRACE: Duration = Duration::from_millis(250);

struct Pressure {
    /// The processes stopped and not yet gone, with when. Nobody else is stopped while one
    /// is within [`STOP_GRACE`] — its memory is about to go — and once past it a victim is
    /// left out of the next selection, which stops the largest holder still able to die.
    stopped: Vec<(u64, Instant)>,
    /// When a selection last found no process worth stopping, and the table holding the
    /// most then, if one held an attributable share. Within a [`PERIOD`] of it the current
    /// process raises without selecting again — a selection walks every parked heap (and
    /// then every table), and over the limit every safepoint asks.
    unattributable: Option<(Instant, Option<(u64, usize)>)>,
}

static PRESSURE: Mutex<Pressure> = Mutex::new(Pressure {
    stopped: Vec::new(),
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
    pressure.stopped.retain(|&(pid, _)| is_alive(pid));
    if pressure
        .stopped
        .iter()
        .any(|&(_, since)| since.elapsed() < STOP_GRACE)
    {
        return None;
    }
    if let Some((since, table)) = pressure.unattributable {
        if since.elapsed() < PERIOD {
            return Some(unattributable_error(live, table));
        }
    }
    let stuck: Vec<u64> = pressure.stopped.iter().map(|&(pid, _)| pid).collect();
    match largest_holder(&stuck) {
        Some((pid, held)) if held >= live / ATTRIBUTABLE && Some(pid) != me => {
            stop(pid, held, live);
            pressure.stopped.push((pid, Instant::now()));
            None
        }
        // The current process holds the most — but its figure counts blobs it shares with a
        // table it just filled (until a collection drops its own copies), so name the
        // table too when one holds a share.
        Some((pid, held)) if held >= live / ATTRIBUTABLE => {
            debug_assert_eq!(Some(pid), me);
            Some(unattributable_error(live, largest_table(live)))
        }
        _ => {
            // No process holds it: look in the tables, which belong to none.
            let table = largest_table(live);
            pressure.unattributable = Some((Instant::now(), table));
            Some(unattributable_error(live, table))
        }
    }
}

/// The table holding the most memory, if it holds an attributable share of `live`.
fn largest_table(live: usize) -> Option<(u64, usize)> {
    crate::core::table::largest_table().filter(|&(_, bytes)| bytes >= live / ATTRIBUTABLE)
}

/// The `E0043` raised in place — naming the table that holds the most, when one holds a
/// share. Stopping a process frees nothing a table stores, so the error says where the
/// memory is instead.
fn unattributable_error(live: usize, table: Option<(u64, usize)>) -> LispError {
    let error = crate::eval::memory_limit_error(live);
    match table {
        Some((id, bytes)) => error.with_hint(format!(
            "table #{id} holds about {} MiB; a table belongs to no process, so stopping one \
             frees nothing it stores — `table/delete` its entries or `table/drop` it",
            bytes >> 20
        )),
        None => error,
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

/// The local process holding the most memory — heap plus queued messages — and how much,
/// leaving out the `stuck`: stopped already, and not dying.
fn largest_holder(stuck: &[u64]) -> Option<(u64, usize)> {
    REGISTRY
        .entries()
        .into_iter()
        .filter(|(pid, _)| !stuck.contains(pid))
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
