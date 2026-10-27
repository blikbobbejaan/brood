//! Interrupting a RUNNING process (ADR-491): Emacs's quit, as a kernel capability.
//!
//! A process opens an **interrupt window** (`proc/allow-interrupt`) around the work it is
//! willing to have stopped — a UI loop's `update` fold, its `view` — and closes it after
//! (`proc/inhibit-interrupt`). While the window is open:
//!
//! - another process may interrupt it (`proc/interrupt pid`);
//! - a message equal to its **quit key** (`proc/set-quit-key`) arriving on the PRIORITY lane
//!   interrupts it once the window has been open `busy-ms` (a window's C-g);
//! - the window's own **ceiling** (`proc/allow-interrupt ms`) interrupts it from inside.
//!
//! The interrupt is raised at the process's next safepoint — the once-per-quantum check
//! every tier already makes (a reduction rollover, ~1 ms of work) — as a catchable error of
//! kind `:interrupted`. Raising closes the window, so a handler is never itself interrupted.
//! Immutability is what makes this safe: the code it stops was building new values, never
//! changing old ones, so nothing is left half-written.
//!
//! **Why a window, not "since the process left `receive`".** A stamp at every receive is
//! reset by each `gen/call` a fold makes, so a slow fold that makes calls would never look
//! busy; and an interrupt landing in a loop's own glue (between its receive and the `try`
//! around a fold) would crash the loop. The window says exactly where an interrupt may land.
//!
//! **A parked process is not interrupted.** A request against a process blocked in
//! `receive` (its status `WAITING`) is a no-op that answers false and latches nothing, so it
//! cannot fire later at a point nobody chose. Its own ceiling is likewise checked only when
//! it runs.
//!
//! Hot-path cost: nothing new per iteration. The probe is one relaxed load of
//! [`Interrupt::flags`], made only at the quantum boundary beside the existing mailbox-bound
//! and hard-kill probes; the clock is read there only while a ceiling is armed.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// The window is open: requests may land.
const OPEN: u8 = 1;
/// A request landed; the next safepoint raises it.
const PENDING: u8 = 2;
/// The window has a ceiling (`deadline_us`); the safepoint compares the clock to it.
const DEADLINE: u8 = 4;

/// Microseconds on a monotonic clock shared by every thread (its epoch is the first call).
pub(super) fn monotonic_us() -> u64 {
    static EPOCH: std::sync::OnceLock<web_time::Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(web_time::Instant::now);
    epoch.elapsed().as_micros().min(u64::MAX as u128) as u64
}

/// One process's interrupt state, on its registry-reachable mailbox: the interrupter
/// writes it from another thread, the process reads it at its safepoints.
#[derive(Default)]
pub(super) struct Interrupt {
    flags: AtomicU8,
    /// When the window opened (`monotonic_us`): how long the process has been busy.
    opened_us: AtomicU64,
    /// The ceiling, meaningful while `DEADLINE` is set.
    deadline_us: AtomicU64,
}

impl Interrupt {
    /// Open the window, optionally with a ceiling `ceiling_ms` from now. Clears anything
    /// left from an earlier window.
    pub(super) fn open(&self, ceiling_ms: Option<u64>) {
        let now = monotonic_us();
        self.opened_us.store(now, Ordering::Relaxed);
        let flags = match ceiling_ms {
            Some(ms) => {
                self.deadline_us.store(
                    now.saturating_add(ms.saturating_mul(1000)),
                    Ordering::Relaxed,
                );
                OPEN | DEADLINE
            }
            None => OPEN,
        };
        // Release: an interrupter that sees OPEN sees the stamp it measures busy-ms by.
        self.flags.store(flags, Ordering::Release);
    }

    /// Close the window, dropping a request that landed but was not raised.
    pub(super) fn close(&self) {
        self.flags.store(0, Ordering::Relaxed);
    }

    /// Ask for an interrupt: lands only while the window is open (and the caller checked the
    /// process is not parked). `busy_us`: only if the window has been open that long — the
    /// quit key's threshold. True when it landed.
    pub(super) fn request(&self, busy_us: u64) -> bool {
        let mut current = self.flags.load(Ordering::Acquire);
        loop {
            if current & OPEN == 0 {
                return false;
            }
            if busy_us > 0
                && monotonic_us().saturating_sub(self.opened_us.load(Ordering::Relaxed)) < busy_us
            {
                return false;
            }
            match self.flags.compare_exchange_weak(
                current,
                current | PENDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    /// Is an interrupt due — a request landed, or the ceiling passed? A peek, for code that
    /// cannot raise (a native loop's poll, a parser's progress callback). One relaxed load
    /// when nothing is armed.
    #[inline]
    pub(super) fn due(&self) -> bool {
        let flags = self.flags.load(Ordering::Relaxed);
        if flags & (PENDING | DEADLINE) == 0 {
            return false;
        }
        flags & PENDING != 0 || monotonic_us() >= self.deadline_us.load(Ordering::Relaxed)
    }

    /// Take a due interrupt, closing the window: `Some(by_ceiling)` when the safepoint must
    /// raise one. Losing the race to a `close` means the window shut first — nothing to raise.
    pub(super) fn take(&self) -> Option<bool> {
        let flags = self.flags.load(Ordering::Relaxed);
        if flags & (PENDING | DEADLINE) == 0 {
            return None;
        }
        let by_ceiling = flags & PENDING == 0;
        if by_ceiling && monotonic_us() < self.deadline_us.load(Ordering::Relaxed) {
            return None;
        }
        self.flags
            .compare_exchange(flags, 0, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| by_ceiling)
    }
}

/// A process's quit key (`proc/set-quit-key`): the message that interrupts it when it
/// arrives on the PRIORITY lane, and how long the window must have been open first. An
/// atom — what a frontend delivers as a key — so the delivery compares without a heap.
pub(super) struct QuitKey {
    pub(super) key: QuitAtom,
    pub(super) busy_us: u64,
}

#[derive(PartialEq)]
pub(crate) enum QuitAtom {
    Keyword(crate::core::value::Symbol),
    Str(String),
    Int(i64),
}

impl QuitKey {
    /// Is the queued `msg` this key?
    pub(super) fn matches(&self, msg: &super::Message) -> bool {
        use super::Message;
        match (&self.key, msg) {
            (QuitAtom::Keyword(a), Message::Keyword(b)) => a == b,
            (QuitAtom::Str(a), Message::Str(b)) => a == b,
            (QuitAtom::Int(a), Message::Int(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_lands_only_in_an_open_window_and_is_taken_once() {
        let interrupt = Interrupt::default();
        assert!(!interrupt.request(0), "closed: nothing lands");
        assert!(!interrupt.due());
        interrupt.open(None);
        assert!(!interrupt.due(), "an open window is not a request");
        assert!(interrupt.request(0));
        assert!(interrupt.due());
        assert_eq!(interrupt.take(), Some(false));
        assert_eq!(interrupt.take(), None, "taking closes the window");
        assert!(!interrupt.request(0), "and a closed window takes no more");
    }

    #[test]
    fn closing_drops_a_request_that_was_not_raised() {
        let interrupt = Interrupt::default();
        interrupt.open(None);
        assert!(interrupt.request(0));
        interrupt.close();
        assert!(!interrupt.due());
        assert_eq!(interrupt.take(), None);
    }

    #[test]
    fn the_busy_threshold_counts_from_the_window_opening() {
        let interrupt = Interrupt::default();
        interrupt.open(None);
        assert!(!interrupt.request(60_000_000), "a minute has not passed");
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(interrupt.request(1_000), "a millisecond has");
    }

    #[test]
    fn a_ceiling_is_due_once_passed() {
        let interrupt = Interrupt::default();
        interrupt.open(Some(60_000));
        assert!(!interrupt.due(), "a minute away");
        assert_eq!(interrupt.take(), None);
        interrupt.open(Some(0));
        assert!(interrupt.due());
        assert_eq!(interrupt.take(), Some(true));
        assert!(!interrupt.due());
    }
}
