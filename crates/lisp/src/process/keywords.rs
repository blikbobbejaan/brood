//! Canonical spellings of the keyword *tags* the runtime puts into process
//! mailbox messages and exit/monitor reasons — the wire contract between the
//! Rust scheduler / monitor / link / dist machinery and the Brood code that
//! `receive`s these messages (`std/prelude.blsp`, `std/proc/supervisor.blsp`,
//! `std/tool/observer.blsp`, …).
//!
//! Each is interned with `value::intern(..)` at its construction site. Before
//! this module the bare strings `"down"` / `"EXIT"` / `"noconnection"` / … were
//! re-typed across `process/{scheduler,monitor,links,mailbox}.rs` and `dist.rs`;
//! now they live in one place, so a rename is a single edit and a Rust-side typo
//! is a compile error.
//!
//! **Scope caveat:** the *Brood* side (`[:down …]` / `[:EXIT …]` patterns in
//! `std/`) types these spellings independently — this module dedups and
//! documents the Rust half of the contract, it does not by itself keep the two
//! languages in sync. (These are runtime message tags, deliberately kept apart
//! from `core::keywords`, which holds only language special-form/macro spellings.)
//!
//! Conventionally imported as `use crate::process::keywords as pk;`.

// --- Monitor / link / node message tags (the leading keyword of the tuple
// delivered to a watcher's mailbox). ---

/// `[:down mref pid reason]` — a `monitor`ed process went down.
pub const DOWN: &str = "down";
/// `[:EXIT pid reason]` — a `link`ed (and `proc/trap-exit`ing) peer exited.
pub const EXIT: &str = "EXIT";
/// `[:nodedown name]` — a `monitor-node`'d node link dropped.
pub const NODEDOWN: &str = "nodedown";

// --- System-monitor event tags (`[:system <kind> <subject-pid> <detail>]`,
// delivered to the `(proc/system-monitor pid opts)` subscriber — `sysmon.rs`). ---

/// The leading tag of every proc/system-monitor event.
pub const SYSTEM: &str = "system";
/// `[:system :gc pid {:pause-us … :collections … :live …}]` — a collection ran.
pub const SYS_GC: &str = "gc";
/// `[:system :spawn child-pid parent-pid]` — a green process was spawned.
pub const SYS_SPAWN: &str = "spawn";
/// `[:system :exit pid reason]` — a green process exited (any reason).
pub const SYS_EXIT: &str = "exit";
/// `[:system :deopt pid fn-name-or-nil]` — a JIT'd arm fell back to the VM.
pub const SYS_DEOPT: &str = "deopt";
/// `[:system :long-schedule pid {:us n}]` — a scheduling quantum ran past the subscriber's
/// threshold (BEAM `long_schedule`, ADR-404).
pub const SYS_LONG_SCHEDULE: &str = "long-schedule";

// --- Exit / down reasons. ---

/// Clean exit — does not propagate to non-trapping linked peers.
pub const NORMAL: &str = "normal";
/// Untrappable hard-kill request flag (`(exit pid :kill)`).
pub const KILL: &str = "kill";
/// The reason delivered to watchers after a hard kill.
pub const KILLED: &str = "killed";
/// Uncaught error — `[:error msg]`.
pub const ERROR: &str = "error";
/// The monitor/link target was already dead.
pub const NOPROC: &str = "noproc";
/// The peer is unreachable (connection lost / net-split).
pub const NOCONNECTION: &str = "noconnection";

/// The sentinel node name before `(node-start)`.
pub const NONODE: &str = "nonode";

// --- Process run-status spellings returned by `process-info` / the observer.
// Not message tags, but the same Rust→Brood contract. ---

pub const STATUS_RUNNING: &str = "running";
pub const STATUS_RUNNABLE: &str = "runnable";
pub const STATUS_WAITING: &str = "waiting";

// --- The same spellings, interned once. ---
//
// The exit and down paths build or compare against these on every death and every
// monitor fire; `value::intern` is a per-thread string-map lookup each time. Symbols are
// process-wide (one interner per OS process), so a once-initialised id is valid in every
// runtime and on every thread — the shape `dist`'s `NONODE` already uses.

use std::sync::LazyLock;

use crate::core::value::{Symbol, intern};

/// [`DOWN`], interned.
pub static DOWN_SYMBOL: LazyLock<Symbol> = LazyLock::new(|| intern(DOWN));
/// [`EXIT`], interned.
pub static EXIT_SYMBOL: LazyLock<Symbol> = LazyLock::new(|| intern(EXIT));
/// [`NORMAL`], interned.
pub static NORMAL_SYMBOL: LazyLock<Symbol> = LazyLock::new(|| intern(NORMAL));
/// [`KILL`], interned.
pub static KILL_SYMBOL: LazyLock<Symbol> = LazyLock::new(|| intern(KILL));
/// [`KILLED`], interned.
pub static KILLED_SYMBOL: LazyLock<Symbol> = LazyLock::new(|| intern(KILLED));
/// [`NOPROC`], interned.
pub static NOPROC_SYMBOL: LazyLock<Symbol> = LazyLock::new(|| intern(NOPROC));
