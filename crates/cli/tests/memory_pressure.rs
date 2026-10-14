//! **The soft memory limit stops the process holding the memory, not a bystander**
//! (KI-229).
//!
//! The soft limit counts every byte the allocator holds, so crossing it does not say which
//! process is to blame. It used to raise in whichever process next reached a safepoint: a
//! busy worker that makes only garbage reaches one constantly, so it died, run after run,
//! while the process actually accumulating kept going. Now the runtime measures every
//! process and stops the largest holder (`process/pressure.rs`).
//!
//! Each case runs a real program under `BROOD_MEM_LIMIT` with one culprit and one bystander,
//! and asserts on what a supervisor would see: which pid went down, and that the bystander
//! is still alive afterwards.

use std::process::{Command, Stdio};

/// Run `script` under a 512 MiB limit; return `(stdout, stderr)`.
fn run_limited(name: &str, script: &str) -> (String, String) {
    let directory = std::env::temp_dir().join(format!("brood-ki229-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    std::fs::write(directory.join("main.blsp"), script).expect("write the script");
    let output = Command::new(env!("CARGO_BIN_EXE_brood"))
        .arg("main.blsp")
        .current_dir(&directory)
        .env("BROOD_MEM_LIMIT", "512M")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run brood");
    let _ = std::fs::remove_dir_all(&directory);
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Wait for the culprit's `[:down …]`, then report who went down and whether the bystander
/// lives. Shared by both programs, which define `culprit` and `bystander`.
const VERDICT: &str = r#"
(monitor culprit)
(monitor bystander)
(io/puts "culprit" culprit)
(receive
  ([:down _ pid [:error {:code code}]] (io/puts "down" pid code))
  ([:down _ pid reason] (io/puts "down" pid reason))
  (after 60000 (io/puts "nothing went down")))
(io/puts "bystander alive" (proc/alive? bystander))
"#;

fn assert_culprit_stopped(stdout: &str, stderr: &str) {
    let culprit = stdout
        .lines()
        .find_map(|line| line.strip_prefix("culprit "))
        .unwrap_or_else(|| panic!("the program never started:\n{stdout}\n{stderr}"));
    assert!(
        stdout.contains(&format!("down {culprit} E0043")),
        "the culprit was not the process stopped:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("bystander alive true"),
        "the bystander did not survive:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("stopping process"),
        "nothing on stderr named the stopped process:\n{stderr}"
    );
}

#[test]
fn a_leak_is_stopped_and_a_busy_worker_beside_it_is_not() {
    let script = format!(
        r#"
(defn leaky (kept)
  (receive (after 2 (leaky (conj kept (string/repeat "x" 1000000))))))
(defn busy (n)
  (let (garbage (map (range 2000) (fn (i) [i (str i)])))
    (busy (+ n (count garbage)))))
(def culprit (spawn (leaky [])))
(def bystander (spawn (busy 0)))
{VERDICT}"#
    );
    let (stdout, stderr) = run_limited("leak", &script);
    assert_culprit_stopped(&stdout, &stderr);
}

#[test]
fn a_flooded_mailbox_is_stopped_and_a_ticker_beside_it_is_not() {
    let script = format!(
        r#"
(defn sink () (receive ([:never] nil)))
(defn flood (target n)
  (send target [:blob (string/repeat "x" 100000) n])
  (flood target (inc n)))
(defn ticker () (receive (after 10 (ticker))))
(def culprit (spawn (sink)))
(def bystander (spawn (ticker)))
(spawn (flood culprit 0))
{VERDICT}"#
    );
    let (stdout, stderr) = run_limited("flood", &script);
    assert_culprit_stopped(&stdout, &stderr);
}
