//! `tcp/connect` must never hold a scheduler worker while it resolves and handshakes.
//!
//! The dial used to run on the calling green process's worker, bounded only by a 10 s
//! per-address timeout — so a handful of connects to a host that drops SYNs took the
//! whole pool down for ten seconds. Now the dial runs on a helper thread and the caller
//! parks in a `receive` for `[:tcp-connected …]` / `[:tcp-error …]`.
//!
//! Deterministic construction: a loopback listener with backlog 0 whose single queue
//! slot is already taken. Linux drops further SYNs while the accept queue is full, so
//! every later connect hangs until its timeout — no external network involved. The pool
//! is pinned to two workers and two processes connect to that listener; a third process
//! must still run. With the dial on the worker both workers are stuck for 10 s and the
//! third process never answers within the 5 s window.
//!
//! Own test binary: `set_max_parallel` is process-wide.

#![cfg(target_os = "linux")]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::FromRawFd;
use std::time::Duration;

use brood::{process, Interp};

/// A listener on 127.0.0.1 with an accept backlog of 0 (one queued connection).
fn backlog_zero_listener() -> TcpListener {
    // SAFETY: plain socket syscalls on a fresh fd that we own; the fd is handed to
    // `TcpListener`, which closes it on drop.
    unsafe {
        let descriptor = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        assert!(descriptor >= 0, "socket() failed");
        let mut address: libc::sockaddr_in = std::mem::zeroed();
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_port = 0;
        address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        let bound = libc::bind(
            descriptor,
            &address as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        assert_eq!(bound, 0, "bind() failed");
        assert_eq!(libc::listen(descriptor, 0), 0, "listen() failed");
        TcpListener::from_raw_fd(descriptor)
    }
}

#[test]
fn a_hanging_connect_does_not_starve_the_pool() {
    process::set_max_parallel(2);
    let listener = backlog_zero_listener();
    let address: SocketAddr = listener.local_addr().expect("local addr");
    // Take the one queue slot (never accepted), then prove the next SYN is dropped —
    // the precondition the whole test rests on.
    let _occupant = TcpStream::connect(address).expect("first connect fills the queue");
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_err(),
        "precondition: a connect to the full listener must hang"
    );

    let mut interp = Interp::new();
    let program = format!(
        r#"
        (def me (self))
        (defn dial () (try (tcp/connect "127.0.0.1" {port}) (catch _ nil)))
        (spawn (dial))
        (spawn (dial))
        (receive (:never nil) (after 300 nil))
        (spawn (send me :alive))
        (receive (:alive :alive) (after 5000 :starved))
        "#,
        port = address.port()
    );
    let result = interp.eval_str(&program).expect("program errored");
    assert_eq!(
        interp.print(result),
        ":alive",
        "a process could not run while two connects were in flight — the dial is holding \
         scheduler workers"
    );
}
