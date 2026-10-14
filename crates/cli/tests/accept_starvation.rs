//! **A listener that ran out of file descriptors accepts its backlog once they are free**
//! (KI-227).
//!
//! The reactor registers a listener edge-triggered and drains `accept` until it would block.
//! A drain stopped by `EMFILE` used to end there, so the connections still queued in the
//! backlog waited for some LATER connection to re-arm the listener — forever, if none came.
//! The clients sat connected and unanswered while the server had descriptors to spare.
//!
//! The server runs under `ulimit -n 40` and a separate client process (unlimited) holds 50
//! connections. The server accepts until it runs out, closes everything it accepted, and
//! then — with no new connection arriving — must accept the rest.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SERVER: &str = r#"
(def listener (tcp/listen "127.0.0.1" 0))
(file/spit "port.txt" (str (tcp/local-port listener)))
(defn drain-accepts (acc quiet-ms)
  (receive ([:tcp-accept _ client] (drain-accepts (cons client acc) quiet-ms)) (after quiet-ms acc)))
(def first-wave (drain-accepts (list) 3000))
(map first-wave tcp/close)
(def second-wave (drain-accepts (list) 5000))
(io/puts "accepted" (+ (count first-wave) (count second-wave)))
"#;

const CLIENT: &str = r#"
(defn wait-port ()
  (if (file/exists? "port.txt")
    (string/->number (file/slurp "port.txt"))
    (do (sleep 20) (wait-port))))
(def port (wait-port))
(def sockets (map (range 50) (fn (_) (tcp/connect "127.0.0.1" port))))
(file/spit "connected.txt" "50")
(sleep 20000)
"#;

fn brood(directory: &Path, script: &str, descriptor_limit: Option<u32>) -> std::process::Child {
    let binary = env!("CARGO_BIN_EXE_brood");
    let command = match descriptor_limit {
        Some(limit) => format!("ulimit -n {limit}; exec '{binary}' {script}"),
        None => format!("exec '{binary}' {script}"),
    };
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start brood")
}

#[test]
fn the_backlog_is_accepted_once_descriptors_free_up() {
    let directory = std::env::temp_dir().join(format!("brood-ki227-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    std::fs::write(directory.join("server.blsp"), SERVER).expect("write the server");
    std::fs::write(directory.join("client.blsp"), CLIENT).expect("write the client");

    let server = brood(&directory, "server.blsp", Some(40));
    let mut client = brood(&directory, "client.blsp", None);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !directory.join("connected.txt").exists() {
        assert!(
            Instant::now() < deadline,
            "the client never connected all 50"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = server.wait_with_output().expect("the server finishes");
    let _ = client.kill();
    let _ = client.wait();
    let _ = std::fs::remove_dir_all(&directory);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot accept"),
        "the server never ran out of descriptors, so this run proves nothing — stderr: {stderr}"
    );
    assert!(
        stdout.contains("accepted 50"),
        "connections queued while the listener was out of descriptors were never accepted \
         after descriptors freed up (KI-227) — stdout: {stdout} stderr: {stderr}"
    );
}
