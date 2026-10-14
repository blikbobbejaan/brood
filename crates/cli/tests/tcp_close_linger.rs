//! **`tcp/close` delivers the whole response even when the peer's request was never read**
//! (KI-228).
//!
//! A server that answers without reading the whole request — an error response, a `413`,
//! anything that stops reading early — then closes. Once the reactor had flushed the
//! response it dropped the socket, and a socket closed with unread input sends a RESET
//! instead of a FIN; the peer's kernel then discards whatever of the response the peer had
//! not read yet. A client that sent 50 000 bytes, paused, and then read received 128 000 of
//! a 200 000-byte response. The close now shuts the write half and reads to discard until
//! the peer's EOF (bounded by `LINGER`), so the peer reads every byte and then EOF.
//!
//! The client is plain Rust rather than Brood: a Brood client reads eagerly into its
//! mailbox, which hides the loss.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RESPONSE_BYTES: usize = 200_000;

const SERVER: &str = r#"
(def listener (tcp/listen "127.0.0.1" 0))
(file/spit "port.txt" (str (tcp/local-port listener)))
(def response (string/join (map (range 20000) (fn (_) "0123456789")) ""))
(defn serve ()
  (receive
    ([:tcp-accept _ client]
      (tcp/controlling-process client (self))
      (tcp/send client response)
      (tcp/close client)
      (serve))
    (after 10000 :done)))
(serve)
"#;

#[test]
fn a_close_with_the_request_unread_still_delivers_the_whole_response() {
    let directory = std::env::temp_dir().join(format!("brood-ki228-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    std::fs::write(directory.join("server.blsp"), SERVER).expect("write the server");
    let mut server = Command::new(env!("CARGO_BIN_EXE_brood"))
        .arg("server.blsp")
        .current_dir(&directory)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the server");

    let port_file = directory.join("port.txt");
    let deadline = Instant::now() + Duration::from_secs(60);
    let port: u16 = loop {
        if let Some(port) = std::fs::read_to_string(&port_file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break port;
        }
        assert!(Instant::now() < deadline, "the server never wrote its port");
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut received = Vec::new();
    for attempt in 0..3 {
        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("set a read timeout");
        client
            .write_all(&vec![b'x'; 50_000])
            .expect("send the request");
        // Let the response — and, before the fix, the reset — arrive before reading any.
        std::thread::sleep(Duration::from_millis(500));
        let mut response = Vec::new();
        let outcome = client.read_to_end(&mut response);
        received.push((
            attempt,
            response.len(),
            outcome.err().map(|error| error.to_string()),
        ));
    }
    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&directory);

    for (attempt, length, error) in &received {
        assert!(
            *length == RESPONSE_BYTES && error.is_none(),
            "attempt {attempt}: read {length} of {RESPONSE_BYTES} bytes, error {error:?} — the \
             server's close reset the connection and the peer lost the unread rest of the \
             response (KI-228)"
        );
    }
}
