//! Guest links (ADR-415): a node link authenticated by a GRANT rather than the node's own
//! cookie, which can reach only what it was given and cannot carry code.
//!
//! A grant is a second cookie with limits — the registered names a guest may send to, and
//! an optional identity (`:as`) a server reads instead of trusting the client's claim. The
//! handshake's responder tries the node cookie, then each grant (`handshake.rs`); a link
//! that matched a grant is registered here, and every frame from it passes [`admit`].
//!
//! What a guest can reach is the grant's names plus the local pids this node has SENT to
//! it ([`note_outbound`]) — a capability by introduction: a pid on this node is a small
//! sequential integer, so being able to name one proves nothing, but having been told it
//! does. What a guest can carry is data: a message holding a closure, a native function or
//! a shared function handle is dropped whole, since a closure crossing the wire is code the
//! receiver may run (ADR-033).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use crate::core::value::{self, Symbol};
use crate::process::Message;

use super::wire::Frame;
use super::{Target, is_local};

/// A guest grant: the cookie a guest authenticates with, what it may reach by name, and
/// who it is.
pub(crate) struct Grant {
    pub(crate) cookie: String,
    pub(crate) names: HashSet<Symbol>,
    pub(crate) as_name: Option<String>,
}

/// Every grant this node will accept, newest last.
static GRANTS: LazyLock<RwLock<Vec<Arc<Grant>>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// What one connected guest may reach: its grant, and the local pids it has been given.
struct GuestPeer {
    grant: Arc<Grant>,
    reachable: HashSet<u64>,
}

/// The connected guests, by their authenticated node name. A full peer is never here.
static GUESTS: LazyLock<Mutex<HashMap<Symbol, GuestPeer>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `GUESTS.len()`, republished under the `GUESTS` lock by every write to it, so the
/// per-frame filters ([`note_outbound`], [`admit`]) skip the lock and the lookup on a
/// node with no guests — every frame of every link, on almost every node.
///
/// A zero read stands for the lookup that would have missed, and is exact where it
/// guards anything. A guest link's frames are filtered only on its own reader thread,
/// which `establish` spawns AFTER [`register`] stored the count: the spawn orders that
/// store before every load on the thread, so a guest's frame never reads a zero older
/// than its own grant. A sender's `note_outbound` racing a guest's registration from
/// another thread reads either side of it, as its lock acquisition landed either side
/// of the insert (the link is in `NODES` before it is a guest at all, so that window
/// predates this count).
static GUEST_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Whether any guest is connected — see [`GUEST_COUNT`].
fn any_guests() -> bool {
    GUEST_COUNT.load(Ordering::Acquire) != 0
}

/// Register (or replace) a grant for `cookie`.
pub(crate) fn grant(cookie: String, names: HashSet<Symbol>, as_name: Option<String>) {
    let mut grants = crate::core::sync::write(&GRANTS);
    grants.retain(|existing| existing.cookie != cookie);
    grants.push(Arc::new(Grant {
        cookie,
        names,
        as_name,
    }));
}

/// Forget the grant for `cookie`. A guest already connected under it stays connected —
/// `node/disconnect` is how to end that — but no new link can authenticate with it.
pub(crate) fn revoke(cookie: &str) -> bool {
    let mut grants = crate::core::sync::write(&GRANTS);
    let before = grants.len();
    grants.retain(|existing| existing.cookie != cookie);
    grants.len() != before
}

/// A snapshot of the grants, for the handshake to try in turn.
pub(super) fn grants() -> Vec<Arc<Grant>> {
    crate::core::sync::read(&GRANTS).clone()
}

/// Record that `peer` authenticated with `grant` — every frame from it is now filtered.
pub(super) fn register(peer: Symbol, grant: Arc<Grant>) {
    let mut guests = crate::core::sync::lock(&GUESTS);
    guests.insert(
        peer,
        GuestPeer {
            grant,
            reachable: HashSet::new(),
        },
    );
    GUEST_COUNT.store(guests.len(), Ordering::Release);
}

/// `peer`'s link is gone; so is what it was given.
pub(super) fn forget(peer: Symbol) {
    let mut guests = crate::core::sync::lock(&GUESTS);
    guests.remove(&peer);
    GUEST_COUNT.store(guests.len(), Ordering::Release);
}

/// Whether `peer` is a guest.
pub(super) fn is_guest(peer: Symbol) -> bool {
    crate::core::sync::lock(&GUESTS).contains_key(&peer)
}

/// The grant `peer` authenticated with, or `None` for a full peer.
pub(crate) fn grant_of(peer: Symbol) -> Option<Arc<Grant>> {
    crate::core::sync::lock(&GUESTS)
        .get(&peer)
        .map(|guest| Arc::clone(&guest.grant))
}

/// A message is about to go to `peer`: if it is a guest, every local pid in it becomes
/// reachable by that guest — it has been told about them.
pub(super) fn note_outbound(peer: Symbol, msg: &Message) {
    if !any_guests() {
        return;
    }
    let mut guests = crate::core::sync::lock(&GUESTS);
    if let Some(guest) = guests.get_mut(&peer) {
        local_pids(msg, &mut guest.reachable);
    }
}

/// Whether a frame from `peer` may be acted on. A full peer's always may; a guest's only
/// if it reaches what the guest was given and carries no code.
pub(super) fn admit(peer: Symbol, frame: &Frame) -> bool {
    if !any_guests() {
        return true; // no guest is connected, so `peer` is a full one
    }
    let guests = crate::core::sync::lock(&GUESTS);
    let Some(guest) = guests.get(&peer) else {
        return true;
    };
    let reaches = |target: &Target| match target {
        Target::Name(name) => guest.grant.names.contains(name),
        Target::Pid(id) => guest.reachable.contains(id),
    };
    let admitted = match frame {
        Frame::Send { target, msg } => reaches(target) && !carries_code(msg),
        Frame::Monitor { target, .. } => guest.reachable.contains(target),
        Frame::MonitorName { name, .. } => guest.grant.names.contains(name),
        // Dropping a monitor it holds, or being told one of ITS monitors fired, couples
        // nothing new.
        Frame::Demonitor { .. } | Frame::Down { .. } => true,
        // liveness — the reader has already stamped `last_seen`
        Frame::Ping | Frame::Pong => true,
        // A link ties a process here to the guest's fate, and an exit signal is how a peer
        // kills a process: neither is a guest's to send. Nor is the mesh — a guest is
        // never gossiped and its gossip is not followed.
        _ => false,
    };
    if !admitted {
        warn_refused(peer, frame);
    }
    admitted
}

fn warn_refused(peer: Symbol, frame: &Frame) {
    let what = match frame {
        Frame::Send { msg, .. } if carries_code(msg) => "a message carrying code",
        Frame::Send { .. } => "a message to something it was not given",
        _ => "a frame a guest may not send",
    };
    eprintln!(
        "dist: refused {what} from guest {}",
        value::symbol_name(peer)
    );
}

/// The children of a container message, for the two walks below.
fn children(msg: &Message) -> Vec<&Message> {
    match msg {
        Message::List(items, _)
        | Message::Dotted(items)
        | Message::Vector(items)
        | Message::Set(items) => items.iter().collect(),
        Message::Map(pairs) | Message::Failure(pairs) => {
            pairs.iter().flat_map(|(k, v)| [k, v]).collect()
        }
        _ => Vec::new(),
    }
}

/// Whether `msg` holds a function anywhere — a closure, a native function or a shared
/// function handle. An explicit stack, so a deeply nested message costs no native stack.
pub(super) fn carries_code(msg: &Message) -> bool {
    let mut stack = vec![msg];
    while let Some(next) = stack.pop() {
        match next {
            Message::Closure(_) | Message::Native(_) | Message::FnShared { .. } => return true,
            _ => stack.extend(children(next)),
        }
    }
    false
}

/// Every pid of THIS node in `msg`, added to `out`.
fn local_pids(msg: &Message, out: &mut HashSet<u64>) {
    let mut stack = vec![msg];
    while let Some(next) = stack.pop() {
        match next {
            Message::Pid { node, id } => {
                if is_local(*node) {
                    out.insert(*id);
                }
            }
            _ => stack.extend(children(next)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyword(name: &str) -> Message {
        Message::Keyword(value::intern(name))
    }

    #[test]
    fn plain_data_carries_no_code_however_deep() {
        let nested = Message::Vector(vec![
            keyword("key"),
            Message::Map(vec![(
                keyword("a"),
                Message::List(vec![Message::Int(1)], None),
            )]),
        ]);
        assert!(!carries_code(&nested));
    }

    #[test]
    fn a_function_anywhere_in_a_message_is_code() {
        let native = Message::Native(value::intern("os/cmd"));
        assert!(carries_code(&native));
        let buried = Message::Vector(vec![
            keyword("key"),
            Message::Map(vec![(keyword("run"), Message::Set(vec![native]))]),
        ]);
        assert!(carries_code(&buried));
    }

    #[test]
    fn only_this_nodes_pids_are_collected() {
        let here = Message::Pid {
            node: *super::super::NONODE,
            id: 7,
        };
        let elsewhere = Message::Pid {
            node: value::intern("elsewhere@nowhere"),
            id: 8,
        };
        let mut found = HashSet::new();
        local_pids(
            &Message::Vector(vec![here, Message::List(vec![elsewhere], None)]),
            &mut found,
        );
        assert_eq!(found, HashSet::from([7]));
    }

    #[test]
    fn a_guest_reaches_its_names_and_the_pids_it_was_sent_and_nothing_else() {
        let peer = value::intern("guest-test@nowhere");
        register(
            peer,
            Arc::new(Grant {
                cookie: "unit-test-guest-cookie".to_string(),
                names: HashSet::from([value::intern("docs")]),
                as_name: Some("alice".to_string()),
            }),
        );
        let send = |target, msg| Frame::Send { target, msg };
        assert!(admit(
            peer,
            &send(Target::Name(value::intern("docs")), keyword("hi"))
        ));
        assert!(!admit(
            peer,
            &send(Target::Name(value::intern("remote-spawn")), keyword("hi"))
        ));
        assert!(!admit(peer, &send(Target::Pid(42), keyword("hi"))));
        note_outbound(
            peer,
            &Message::Pid {
                node: *super::super::NONODE,
                id: 42,
            },
        );
        assert!(admit(peer, &send(Target::Pid(42), keyword("hi"))));
        assert!(!admit(
            peer,
            &send(Target::Pid(42), Message::Native(value::intern("os/cmd")))
        ));
        assert!(!admit(
            peer,
            &Frame::Exit {
                from_pid: 1,
                to_pid: 42,
                reason: keyword("kill"),
                link: false,
            }
        ));
        assert_eq!(
            grant_of(peer).and_then(|g| g.as_name.clone()),
            Some("alice".to_string())
        );
        forget(peer);
        assert!(
            admit(peer, &send(Target::Pid(99), keyword("hi"))),
            "a full peer is unfiltered"
        );
    }
}
