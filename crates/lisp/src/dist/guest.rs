//! Guest links (ADR-415): a node link authenticated by a GRANT rather than the node's own
//! cookie, which can reach only what it was given and cannot carry code.
//!
//! A grant is a second cookie with limits — the registered names a guest may send to, and
//! an optional identity (`:as`) a server reads instead of trusting the client's claim. The
//! handshake's responder tries the node cookie, then each grant (`handshake.rs`); a link
//! that matched a grant carries a [`GuestLink`] from the moment it is registered, and every
//! frame its reader reads passes [`admit`] against that same `GuestLink`.
//!
//! What a guest can reach is the grant's names plus the local pids this node has SENT to
//! it ([`GuestLink::note_outbound`]) — a capability by introduction: a pid on this node is
//! a small sequential integer, so being able to name one proves nothing, but having been
//! told it does. What a guest can carry is data: a message holding a closure, a native
//! function or a shared function handle is dropped whole, since a closure crossing the
//! wire is code the receiver may run (ADR-033).
//!
//! The standing belongs to the LINK, not to the peer's name (KI-286). A name is what the
//! peer claimed in its `Hello`, and several links can hold it over time — a guest's, a
//! full peer's that reclaims it, a second guest's — while a link's reader is still
//! delivering frames it read before the name moved on. A per-frame lookup by name let
//! those frames be judged by whichever link held the name THEN, so a guest's buffered
//! frames ran with a full peer's privileges.

use std::collections::HashSet;
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

/// One guest link's standing: the grant it authenticated with, and the local pids it has
/// been given. Made once, in `establish`, for the one connection that authenticated with
/// the grant; the link's `Conn` and its reader hold the same `Arc`, so it is gone with the
/// link and nothing else — another link under the same name, a `forget` — can change what
/// that link's frames may do.
pub(crate) struct GuestLink {
    grant: Arc<Grant>,
    reachable: Mutex<HashSet<u64>>,
}

impl GuestLink {
    pub(super) fn new(grant: Arc<Grant>) -> Self {
        GuestLink {
            grant,
            reachable: Mutex::new(HashSet::new()),
        }
    }

    /// The grant this link authenticated with.
    pub(crate) fn grant(&self) -> &Arc<Grant> {
        &self.grant
    }

    /// A message is about to go out over this link: every local pid in it becomes
    /// reachable by the guest — it has been told about them.
    pub(super) fn note_outbound(&self, msg: &Message) {
        local_pids(msg, &mut crate::core::sync::lock(&self.reachable));
    }

    fn reaches_pid(&self, id: u64) -> bool {
        crate::core::sync::lock(&self.reachable).contains(&id)
    }
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

/// Whether a frame read from a link to `peer` may be acted on. `guest` is THAT link's
/// standing, captured when the link was established — `None` for a full link, whose
/// frames always may; a guest link's only if they reach what it was given and carry no
/// code. `peer` only names the link in the warning.
pub(super) fn admit(peer: Symbol, guest: Option<&GuestLink>, frame: &Frame) -> bool {
    let Some(guest) = guest else {
        return true;
    };
    let reaches = |target: &Target| match target {
        Target::Name(name) => guest.grant.names.contains(name),
        Target::Pid(id) => guest.reaches_pid(*id),
    };
    let admitted = match frame {
        Frame::Send { target, msg } => reaches(target) && !carries_code(msg),
        Frame::Monitor { target, .. } => guest.reaches_pid(*target),
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
        let link = GuestLink::new(Arc::new(Grant {
            cookie: "unit-test-guest-cookie".to_string(),
            names: HashSet::from([value::intern("docs")]),
            as_name: Some("alice".to_string()),
        }));
        let guest = Some(&link);
        let send = |target, msg| Frame::Send { target, msg };
        assert!(admit(
            peer,
            guest,
            &send(Target::Name(value::intern("docs")), keyword("hi"))
        ));
        assert!(!admit(
            peer,
            guest,
            &send(Target::Name(value::intern("remote-spawn")), keyword("hi"))
        ));
        assert!(!admit(peer, guest, &send(Target::Pid(42), keyword("hi"))));
        link.note_outbound(&Message::Pid {
            node: *super::super::NONODE,
            id: 42,
        });
        assert!(admit(peer, guest, &send(Target::Pid(42), keyword("hi"))));
        assert!(!admit(
            peer,
            guest,
            &send(Target::Pid(42), Message::Native(value::intern("os/cmd")))
        ));
        assert!(!admit(
            peer,
            guest,
            &Frame::Exit {
                from_pid: 1,
                to_pid: 42,
                reason: keyword("kill"),
                link: false,
            }
        ));
        assert_eq!(
            link.grant().as_name.as_deref(),
            Some("alice"),
            "the link carries who it is"
        );
        assert!(
            admit(peer, None, &send(Target::Pid(99), keyword("hi"))),
            "a full link is unfiltered"
        );
    }
}
