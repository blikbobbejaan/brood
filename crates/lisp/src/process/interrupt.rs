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
//! **Windows nest (ADR-493).** A window opened inside an open one is a window of its own: a
//! request is raised for the innermost, a ceiling for the window whose ceiling it is, and only
//! that window answers `[:interrupted e]` — see [`Interrupt`].
//!
//! **A parked process is not interrupted.** A request against a process blocked in
//! `receive` (its status `WAITING`) is a no-op that answers false and latches nothing, so it
//! cannot fire later at a point nobody chose. Its own ceiling is likewise checked only when
//! it runs.
//!
//! Hot-path cost: nothing new per iteration. The probe is one relaxed load of
//! [`Interrupt::flags`], made only at the quantum boundary beside the existing mailbox-bound
//! and hard-kill probes; the clock is read there only while a ceiling is armed.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

/// A window is open: requests may land.
const OPEN: u8 = 1;
/// A request landed; the next safepoint raises it.
const PENDING: u8 = 2;
/// An open window has a ceiling (`deadline_us`); the safepoint compares the clock to it.
const DEADLINE: u8 = 4;
/// The interrupt has been raised and is STICKY: every later safepoint raises it again until
/// the window it is for (`raised_window`) closes, so a catch-all inside the interrupted work
/// only delays it.
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
///
/// **Windows nest (ADR-493).** Each open window is numbered by its depth, 1 the outermost,
/// and an interrupt is raised FOR one of them: a request (`proc/interrupt`, the quit key)
/// for the innermost window open when it is raised — the work nearest the user that chose
/// to be stoppable, Emacs's `with-local-quit` — and a ceiling for the window it is the
/// ceiling of, since that window's own budget is what ran out. Only that window's
/// `proc/interruptible` answers `[:interrupted e]`; a window inside it lets the error pass
/// and closes, one outside it never sees it. So an interrupt an inner window consumed does
/// not also stop the outer one, and an outer ceiling is not swallowed by an inner window.
#[derive(Default)]
pub(super) struct Interrupt {
    flags: AtomicU8,
    /// When the OUTERMOST window opened (`monotonic_us`): how long the process has been
    /// busy — what the quit key's busy-ms and a busy cursor measure.
    opened_us: AtomicU64,
    /// The earliest ceiling of the open windows, meaningful while `DEADLINE` is set.
    deadline_us: AtomicU64,
    /// The window `deadline_us` belongs to (its depth, 1 the outermost).
    deadline_window: AtomicU32,
    /// The window a RAISED interrupt is for.
    raised_window: AtomicU32,
    /// Each open window's ceiling (an absolute `monotonic_us`), outermost first: its length
    /// is the innermost window's depth. Only the owning process touches it — on a window's
    /// open, its close, and the raise of a request — so the lock is never contended.
    windows: Mutex<Vec<Option<u64>>>,
}

impl Interrupt {
    /// Open a window inside any already open, optionally with a ceiling `ceiling_ms` from
    /// now; answers its depth, which [`close`](Self::close) takes. The outermost window clears
    /// anything left from an earlier one.
    pub(super) fn open(&self, ceiling_ms: Option<u64>) -> u32 {
        let now = monotonic_us();
        let mut windows = crate::core::sync::lock(&self.windows);
        windows.push(ceiling_ms.map(|ms| now.saturating_add(ms.saturating_mul(1000))));
        let depth = windows.len() as u32;
        let deadline = self.arm_deadline(&windows);
        if depth == 1 {
            self.opened_us.store(now, Ordering::Relaxed);
            // Release: an interrupter that sees OPEN sees the stamp it measures busy-ms by.
            self.flags.store(OPEN | deadline, Ordering::Release);
        } else if deadline != 0 {
            self.flags.fetch_or(DEADLINE, Ordering::AcqRel);
        }
        depth
    }

    /// Close window `depth` and every window inside it. An interrupt raised for one of them
    /// is consumed; one raised for an outer window stays raised, for that window to answer.
    /// Closing the outermost drops a request that landed but was not raised; closing an
    /// inner one leaves it for the windows still open — it is raised for whichever is
    /// innermost then, as Emacs's `quit-flag` is handled by whatever checks it next.
    pub(super) fn close(&self, depth: u32) {
        let mut windows = crate::core::sync::lock(&self.windows);
        windows.truncate(depth.saturating_sub(1) as usize);
        if windows.is_empty() {
            self.flags.store(0, Ordering::Relaxed);
            return;
        }
        let deadline = self.arm_deadline(&windows);
        let consumed = self.raised_window.load(Ordering::Relaxed) >= depth;
        let mut current = self.flags.load(Ordering::Acquire);
        loop {
            let mut next = (current & !DEADLINE) | deadline;
            if consumed {
                next &= !(RAISED | BY_CEILING);
            }
            match self.flags.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(seen) => current = seen,
            }
        }
    }

    /// Store the earliest ceiling among `windows` (the outermost on a tie) and the window it
    /// is; answers `DEADLINE` when there is one, 0 when no open window has a ceiling.
    fn arm_deadline(&self, windows: &[Option<u64>]) -> u8 {
        let earliest = windows
            .iter()
            .enumerate()
            .filter_map(|(index, deadline)| deadline.map(|at| (at, index)))
            .min();
        match earliest {
            Some((at, index)) => {
                self.deadline_us.store(at, Ordering::Relaxed);
                self.deadline_window
                    .store(index as u32 + 1, Ordering::Relaxed);
                DEADLINE
            }
            None => 0,
        }
    }

    /// How long the outermost window has been open, or None when none is — what a
    /// frontend's busy cursor asks of the loop it delivers to.
    pub(super) fn open_for_us(&self) -> Option<u64> {
        if self.flags.load(Ordering::Acquire) & OPEN == 0 {
            return None;
        }
        Some(monotonic_us().saturating_sub(self.opened_us.load(Ordering::Relaxed)))
    }

    /// Ask for an interrupt: lands only while a window is open (and the caller checked the
    /// process is not parked). `busy_us`: only if the outermost window has been open that
    /// long — the quit key's threshold. True when it landed (or one is already raised).
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

    /// Is an interrupt due — raised and still open, a request landed, or a ceiling passed?
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
    /// marks it RAISED for its window (a request: the innermost; a ceiling: its own), and
    /// every later call answers it again until that window closes — STICKY, so a `catch`
    /// that swallows it inside the interrupted work is interrupted again at its next
    /// safepoint (Emacs's quit is not an error a `condition-case` on `error` catches; in
    /// Brood every `catch` catches, so the kernel insists instead). Losing a race to a
    /// `close` means the window shut first — nothing to raise.
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
            // Only the owner raises, so the window recorded here is read back by the
            // same process when a window asks whether the interrupt is its own.
            let window = if by_ceiling {
                self.deadline_window.load(Ordering::Relaxed)
            } else {
                crate::core::sync::lock(&self.windows).len() as u32
            };
            self.raised_window.store(window, Ordering::Relaxed);
            let raised =
                (flags & DEADLINE) | OPEN | RAISED | if by_ceiling { BY_CEILING } else { 0 };
            if self
                .flags
                .compare_exchange(flags, raised, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(by_ceiling);
            }
        }
    }

    /// Is the raised interrupt window `depth`'s own — the one its `proc/interruptible`
    /// answers `[:interrupted e]` for, rather than lets pass to an outer window?
    pub(super) fn raised_for(&self, depth: u32) -> bool {
        self.flags.load(Ordering::Acquire) & RAISED != 0
            && self.raised_window.load(Ordering::Relaxed) == depth
    }
}

/// A process's quit key (`proc/set-quit-key`): the message that interrupts it, and how long
/// the window must have been open first. Two shapes, both compared without a heap:
///
/// - an ATOM (keyword, string or integer) — a key as a window or the terminal forwarder
///   delivers it — matched on the PRIORITY lane only, where the frontend says it is input;
/// - a VECTOR of atoms — a key as a wire frontend ships it (`[:key :ctrl-g]` from a thin
///   client) — matched as the leading elements of a vector message on ANY lane: lanes do
///   not cross a node link, and the message's own tag says it is input, as ADR-490's
///   `:poll-tagged` reads it.
pub(super) struct QuitKey {
    pub(super) key: QuitPattern,
    pub(super) busy_us: u64,
    /// The node a key from another node must come from: the thin client this loop serves
    /// (`proc/set-quit-key`'s `from`). None: any full peer, and never a guest link.
    pub(super) from: Option<crate::core::value::Symbol>,
}

/// Where a delivered message came from, for the quit key (ADR-491): this node, or a peer
/// over a link — whether that link is a GUEST's (ADR-415).
#[derive(Clone, Copy)]
pub(crate) enum Origin {
    Local,
    Peer {
        node: crate::core::value::Symbol,
        guest: bool,
    },
}

thread_local! {
    /// The peer whose frame this thread is delivering, while a node link's reader delivers
    /// one (`dist::deliver_inbound`); every other delivery is local.
    static INBOUND: std::cell::Cell<Origin> = const { std::cell::Cell::new(Origin::Local) };
}

/// Run `deliver` as a delivery from `origin` — what a link's reader wraps around an inbound
/// `Send`, so the quit key can ask where it came from without a field on every envelope.
pub(crate) fn delivering_from<T>(origin: Origin, deliver: impl FnOnce() -> T) -> T {
    let previous = INBOUND.with(|inbound| inbound.replace(origin));
    let result = deliver();
    INBOUND.with(|inbound| inbound.set(previous));
    result
}

/// Where the message being delivered on this thread came from.
pub(super) fn delivery_origin() -> Origin {
    INBOUND.with(|inbound| inbound.get())
}

pub(crate) enum QuitPattern {
    Atom(QuitAtom),
    Prefix(Vec<QuitAtom>),
}

#[derive(PartialEq)]
pub(crate) enum QuitAtom {
    Keyword(crate::core::value::Symbol),
    Str(String),
    Int(i64),
}

impl QuitAtom {
    fn matches(&self, msg: &super::Message) -> bool {
        use super::Message;
        match (self, msg) {
            (QuitAtom::Keyword(a), Message::Keyword(b)) => a == b,
            (QuitAtom::Str(a), Message::Str(b)) => a == b,
            (QuitAtom::Int(a), Message::Int(b)) => a == b,
            _ => false,
        }
    }
}

impl QuitKey {
    /// May a key from `origin` interrupt (ADR-491)? From this node, always. From a peer, only
    /// the one the key names (`from`), or — when it names none — a full peer: a GUEST link
    /// (ADR-415) that can reach the loop's pid must not stop its turn unless it is the
    /// loop's own client.
    pub(super) fn admits(&self, origin: Origin) -> bool {
        match (origin, self.from) {
            (Origin::Local, _) => true,
            (Origin::Peer { node, .. }, Some(from)) => node == from,
            (Origin::Peer { guest, .. }, None) => !guest,
        }
    }

    /// Is `msg`, queued on the priority lane or not (`priority`), this key?
    pub(super) fn matches(&self, msg: &super::Message, priority: bool) -> bool {
        match &self.key {
            QuitPattern::Atom(atom) => priority && atom.matches(msg),
            QuitPattern::Prefix(atoms) => match msg {
                super::Message::Vector(items) => {
                    items.len() >= atoms.len()
                        && atoms
                            .iter()
                            .zip(items.iter())
                            .all(|(atom, item)| atom.matches(item))
                }
                _ => false,
            },
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
        assert_eq!(interrupt.open(None), 1);
        assert!(!interrupt.due(), "an open window is not a request");
        assert!(interrupt.request(0));
        assert!(interrupt.due());
        assert_eq!(interrupt.take(), Some(false));
        assert_eq!(interrupt.take(), Some(false), "raised again: sticky");
        assert!(interrupt.due());
        interrupt.close(1);
        assert_eq!(interrupt.take(), None, "until the window closes");
        assert!(!interrupt.request(0), "and a closed window takes no more");
    }

    #[test]
    fn closing_drops_a_request_that_was_not_raised() {
        let interrupt = Interrupt::default();
        assert_eq!(interrupt.open(None), 1);
        assert!(interrupt.request(0));
        interrupt.close(1);
        assert!(!interrupt.due());
        assert_eq!(interrupt.take(), None);
    }

    #[test]
    fn the_busy_threshold_counts_from_the_window_opening() {
        let interrupt = Interrupt::default();
        assert_eq!(interrupt.open(None), 1);
        assert!(!interrupt.request(60_000_000), "a minute has not passed");
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(interrupt.request(1_000), "a millisecond has");
    }

    #[test]
    fn how_long_a_window_has_been_open_is_what_a_busy_cursor_reads() {
        let interrupt = Interrupt::default();
        assert_eq!(interrupt.open_for_us(), None, "closed: the loop is idle");
        assert_eq!(interrupt.open(None), 1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(interrupt.open_for_us().is_some_and(|us| us >= 1_000));
        interrupt.close(1);
        assert_eq!(interrupt.open_for_us(), None);
    }

    #[test]
    fn a_vector_quit_key_matches_a_tagged_message_on_any_lane() {
        use super::super::Message;
        let ctrl_g = crate::core::value::intern("ctrl-g");
        let key = crate::core::value::intern("key");
        let wire = QuitKey {
            key: QuitPattern::Prefix(vec![QuitAtom::Keyword(key), QuitAtom::Keyword(ctrl_g)]),
            busy_us: 0,
            from: None,
        };
        let shipped = Message::Vector(vec![
            Message::Keyword(key),
            Message::Keyword(ctrl_g),
            Message::Bool(true),
        ]);
        assert!(wire.matches(&shipped, false));
        assert!(!wire.matches(&Message::Keyword(ctrl_g), true));
        let atom = QuitKey {
            key: QuitPattern::Atom(QuitAtom::Keyword(ctrl_g)),
            busy_us: 0,
            from: None,
        };
        assert!(atom.matches(&Message::Keyword(ctrl_g), true));
        assert!(
            !atom.matches(&Message::Keyword(ctrl_g), false),
            "priority lane only"
        );
    }

    #[test]
    fn a_quit_key_from_another_node_must_come_from_its_client_or_a_full_peer() {
        let client = crate::core::value::intern("client@host");
        let other = crate::core::value::intern("other@host");
        let key = |from| QuitKey {
            key: QuitPattern::Atom(QuitAtom::Int(7)),
            busy_us: 0,
            from,
        };
        let peer = |node, guest| Origin::Peer { node, guest };
        let open = key(None);
        assert!(open.admits(Origin::Local));
        assert!(open.admits(peer(other, false)), "a full peer");
        assert!(
            !open.admits(peer(other, true)),
            "never a guest it does not name"
        );
        let served = key(Some(client));
        assert!(served.admits(Origin::Local));
        assert!(
            served.admits(peer(client, true)),
            "its own client, a guest or not"
        );
        assert!(!served.admits(peer(other, false)), "no other peer");
        assert!(
            delivering_from(peer(client, true), || matches!(
                delivery_origin(),
                Origin::Peer { guest: true, .. }
            )),
            "the reader's delivery is seen as its peer's"
        );
        assert!(
            matches!(delivery_origin(), Origin::Local),
            "and only while it delivers"
        );
    }

    #[test]
    fn a_ceiling_is_due_once_passed() {
        let interrupt = Interrupt::default();
        let window = interrupt.open(Some(60_000));
        assert!(!interrupt.due(), "a minute away");
        assert_eq!(interrupt.take(), None);
        interrupt.close(window);
        let window = interrupt.open(Some(0));
        assert!(interrupt.due());
        assert_eq!(interrupt.take(), Some(true));
        assert_eq!(
            interrupt.take(),
            Some(true),
            "still the ceiling's, raised again"
        );
        interrupt.close(window);
        assert!(!interrupt.due());
    }

    #[test]
    fn a_request_is_for_the_innermost_window_and_closing_it_consumes_it() {
        let interrupt = Interrupt::default();
        let outer = interrupt.open(None);
        let inner = interrupt.open(None);
        assert_eq!((outer, inner), (1, 2));
        assert!(interrupt.request(0));
        assert_eq!(interrupt.take(), Some(false));
        assert!(interrupt.raised_for(inner), "the innermost window's");
        assert!(!interrupt.raised_for(outer));
        interrupt.close(inner);
        assert!(!interrupt.due(), "consumed: the outer window runs on");
        assert_eq!(interrupt.take(), None);
        assert!(interrupt.open_for_us().is_some(), "the outer is still open");
        assert!(interrupt.request(0), "and takes a request of its own");
        assert_eq!(interrupt.take(), Some(false));
        assert!(interrupt.raised_for(outer));
        interrupt.close(outer);
        assert_eq!(interrupt.open_for_us(), None);
    }

    #[test]
    fn an_outer_ceiling_is_the_outer_windows_and_survives_the_inner_closing() {
        let interrupt = Interrupt::default();
        let outer = interrupt.open(Some(0));
        let inner = interrupt.open(None);
        assert_eq!(interrupt.take(), Some(true));
        assert!(interrupt.raised_for(outer), "the ceiling's own window");
        assert!(!interrupt.raised_for(inner));
        interrupt.close(inner);
        assert_eq!(interrupt.take(), Some(true), "still raised for the outer");
        assert!(interrupt.raised_for(outer));
        interrupt.close(outer);
        assert!(!interrupt.due());
    }

    #[test]
    fn an_inner_ceiling_is_the_inner_windows_and_the_outer_is_unarmed_after() {
        let interrupt = Interrupt::default();
        let outer = interrupt.open(Some(60_000));
        let inner = interrupt.open(Some(0));
        assert_eq!(interrupt.take(), Some(true));
        assert!(interrupt.raised_for(inner));
        interrupt.close(inner);
        assert!(!interrupt.due(), "the outer's minute has not passed");
        assert_eq!(interrupt.take(), None);
        interrupt.close(outer);
    }

    #[test]
    fn a_request_not_yet_raised_outlives_an_inner_window_but_not_the_outermost() {
        let interrupt = Interrupt::default();
        let outer = interrupt.open(None);
        let inner = interrupt.open(None);
        assert!(interrupt.request(0));
        interrupt.close(inner);
        assert_eq!(
            interrupt.take(),
            Some(false),
            "raised for the window still open"
        );
        assert!(interrupt.raised_for(outer));
        interrupt.close(outer);
        let outer = interrupt.open(None);
        let _inner = interrupt.open(None);
        assert!(interrupt.request(0));
        interrupt.close(outer);
        assert!(
            !interrupt.due(),
            "closing the outermost closes every window"
        );
        assert_eq!(interrupt.open_for_us(), None);
    }

    #[test]
    fn busy_counts_from_the_outermost_window() {
        let interrupt = Interrupt::default();
        let outer = interrupt.open(None);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _inner = interrupt.open(None);
        assert!(
            interrupt.request(1_000),
            "a turn busy past busy-ms is busy, whatever window it is in now"
        );
        interrupt.close(outer);
    }
}
