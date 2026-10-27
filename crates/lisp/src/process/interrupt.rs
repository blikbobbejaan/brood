//! Interrupting a RUNNING process (ADR-491): Emacs's quit, as a kernel capability.
//!
//! A process runs the work it is willing to have stopped — a UI loop's `update` fold, its
//! `view` — inside an **interrupt window**: `(proc/interruptible ceiling-ms thunk)`, which
//! opens the window, calls the thunk, closes the window and catches the interrupt, all in
//! Rust. While the window is open:
//!
//! - another process may interrupt it (`proc/interrupt pid`);
//! - a message equal to its **quit key** (`proc/set-quit-key`) arriving on the PRIORITY lane
//!   interrupts it once the window has been open `busy-ms` (a window's C-g);
//! - the window's own **ceiling** interrupts it from inside.
//!
//! The interrupt is raised at the process's next safepoint — the once-per-quantum check
//! every tier already makes (a reduction rollover, ~1 ms of work), and the polls inside long
//! builtins — as a catchable error of kind `:interrupted`. It is STICKY: until the window
//! closes it is raised again, and a `catch` that binds it ends the quantum
//! ([`super::mailbox::interrupt_caught`]), so the first safepoint after the handler raises it
//! once more. A catch-all inside the interrupted work therefore cannot keep the process in
//! its window; only `proc/interruptible` ends it, and it has no Brood code between the raise
//! and the close for a re-raise to land in. Immutability is what makes stopping safe: the code
//! it stops was building new values, never changing old ones, so nothing is half-written.
//!
//! **Why a window, not "since the process left `receive`".** A stamp at every receive is
//! reset by each `gen/call` a fold makes, so a slow fold that makes calls would never look
//! busy; and an interrupt landing in a loop's own glue (between its receive and the fold)
//! would crash the loop. The window says exactly where an interrupt may land.
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
/// The interrupt has been raised and is STICKY: every later safepoint raises it again until
/// the window closes, so a catch-all inside the interrupted work only delays it.
const RAISED: u8 = 8;
/// What a RAISED interrupt was: the ceiling passing, rather than a request.
const BY_CEILING: u8 = 16;

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

    /// Close the window, ending a raised interrupt and dropping one that landed but was not
    /// raised.
    pub(super) fn close(&self) {
        self.flags.store(0, Ordering::Relaxed);
    }

    /// How long the window has been open, or None when it is closed — what a frontend's
    /// busy cursor asks of the loop it delivers to.
    pub(super) fn open_for_us(&self) -> Option<u64> {
        if self.flags.load(Ordering::Acquire) & OPEN == 0 {
            return None;
        }
        Some(monotonic_us().saturating_sub(self.opened_us.load(Ordering::Relaxed)))
    }

    /// Ask for an interrupt: lands only while the window is open (and the caller checked the
    /// process is not parked). `busy_us`: only if the window has been open that long — the
    /// quit key's threshold. True when it landed (or one is already raised).
    pub(super) fn request(&self, busy_us: u64) -> bool {
        let mut current = self.flags.load(Ordering::Acquire);
        loop {
            if current & OPEN == 0 {
                return false;
            }
            if current & RAISED != 0 {
                return true;
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

    /// Is an interrupt due — raised and still open, a request landed, or the ceiling passed?
    /// A peek, for code that cannot raise (a native loop's poll, a parser's progress
    /// callback). One relaxed load when nothing is armed.
    #[inline]
    pub(super) fn due(&self) -> bool {
        let flags = self.flags.load(Ordering::Relaxed);
        if flags & (PENDING | DEADLINE | RAISED) == 0 {
            return false;
        }
        flags & (PENDING | RAISED) != 0
            || monotonic_us() >= self.deadline_us.load(Ordering::Relaxed)
    }

    /// The interrupt a safepoint must raise, if any: `Some(by_ceiling)`. The first raise
    /// marks it RAISED, and every later call answers it again until [`close`](Self::close) —
    /// STICKY, so a `catch` that swallows it inside the interrupted work is interrupted again
    /// at its next safepoint (Emacs's quit is not an error a `condition-case` on `error`
    /// catches; in Brood every `catch` catches, so the kernel insists instead). Losing a race
    /// to a `close` means the window shut first — nothing to raise.
    pub(super) fn take(&self) -> Option<bool> {
        loop {
            let flags = self.flags.load(Ordering::Relaxed);
            if flags & (PENDING | DEADLINE | RAISED) == 0 {
                return None;
            }
            if flags & RAISED != 0 {
                return Some(flags & BY_CEILING != 0);
            }
            let by_ceiling = flags & PENDING == 0;
            if by_ceiling && monotonic_us() < self.deadline_us.load(Ordering::Relaxed) {
                return None;
            }
            let raised = OPEN | RAISED | if by_ceiling { BY_CEILING } else { 0 };
            if self
                .flags
                .compare_exchange(flags, raised, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(by_ceiling);
            }
        }
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
    fn a_request_lands_only_in_an_open_window_and_sticks_until_it_closes() {
        let interrupt = Interrupt::default();
        assert!(!interrupt.request(0), "closed: nothing lands");
        assert!(!interrupt.due());
        interrupt.open(None);
        assert!(!interrupt.due(), "an open window is not a request");
        assert!(interrupt.request(0));
        assert!(interrupt.due());
        assert_eq!(interrupt.take(), Some(false));
        assert_eq!(interrupt.take(), Some(false), "raised again: sticky");
        assert!(interrupt.due());
        interrupt.close();
        assert_eq!(interrupt.take(), None, "until the window closes");
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
        assert_eq!(interrupt.take(), Some(true), "still the ceiling's, raised again");
        interrupt.close();
        assert!(!interrupt.due());
    }
}
