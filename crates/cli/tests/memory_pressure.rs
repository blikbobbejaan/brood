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
    run_with_soft_limit(name, script, None)
}

/// Run `script` under a 512 MiB hard limit and the given soft one (default: three
/// quarters of it); return `(stdout, stderr)`.
fn run_with_soft_limit(name: &str, script: &str, soft: Option<&str>) -> (String, String) {
    let directory = std::env::temp_dir().join(format!("brood-ki229-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    std::fs::write(directory.join("main.blsp"), script).expect("write the script");
    let mut command = Command::new(env!("CARGO_BIN_EXE_brood"));
    if let Some(soft) = soft {
        command.env("BROOD_MEM_SOFT_LIMIT", soft);
    }
    let output = command
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

/// The leak grows in pieces under the allocator's large-allocation size, so it never
/// notices the crossing itself: the busy worker does, and must not be the one to pay.
#[test]
fn a_leak_is_stopped_and_a_busy_worker_beside_it_is_not() {
    let script = format!(
        r#"
(defn leaky (kept)
  (receive (after 2 (leaky (conj kept (map (range 20) (fn (_) (string/repeat "x" 50000))))))))
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

/// The largest holder is stopped while it is inside a long native call, so it reaches no
/// safepoint to die at until the call returns. A leak beside it kept growing, and while the
/// stopped process lived nobody else was stopped: the leak reached the hard limit and the
/// runtime aborted. Now a victim that has not died within the grace is looked past.
#[test]
fn a_victim_stuck_in_a_native_call_does_not_shield_the_next_holder() {
    let (stdout, stderr) = run_limited(
        "stuck",
        // The hoarder stops growing inside the last eighth below the soft limit (384 MiB
        // here; the eighth starts at 352 MB), where it has measured itself, then blocks.
        r#"
(defn hoard (kept parent)
  (if (> (%mem-bytes) 370000000)
    (do (send parent :hoarded) (os/cmd "sleep" ["20"]) (count kept))
    (hoard (conj kept (string/repeat "x" 1000000)) parent)))
(defn leaky (kept)
  (receive (after 5 (leaky (conj kept (string/repeat "x" 1000000))))))
(def me (self))
(def hoarder (spawn (hoard [] me)))
(receive (:hoarded nil) (after 60000 (io/puts "never hoarded")))
(def leak (spawn (leaky [])))
(monitor leak)
(io/puts "leak" leak)
(receive
  ([:down _ pid [:error {:code code}]] (io/puts "down" pid code))
  ([:down _ pid reason] (io/puts "down" pid reason))
  (after 60000 (io/puts "nothing went down")))
"#,
    );
    let leak = stdout
        .lines()
        .find_map(|line| line.strip_prefix("leak "))
        .unwrap_or_else(|| panic!("the leak never started:\n{stdout}\n{stderr}"));
    assert!(
        stderr.contains("stopping process"),
        "the hoarder was never chosen, so nothing was stuck:\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("down {leak} E0043")),
        "the leak beside the stuck victim was not stopped:\n{stdout}\n{stderr}"
    );
    assert!(
        !stderr.contains("hard memory limit reached"),
        "the runtime reached the hard limit:\n{stderr}"
    );
}

/// One allocation past the hard limit cannot be stopped gracefully — no safepoint runs
/// inside it — so the runtime aborts. It used to say only `memory allocation of N bytes
/// failed`, naming neither the limit nor that one was set. (The soft limit sits just under
/// the hard one so the 100 MB piece can be built first; the 500 MB `str` is the one step.)
#[test]
fn one_allocation_past_the_hard_limit_names_the_limit() {
    let (stdout, stderr) = run_with_soft_limit(
        "hard",
        r#"
(def quarter (string/repeat "x" 100000000))
(%gc-collect)
(io/puts "started")
(io/puts (string/length (str quarter quarter quarter quarter quarter)))"#,
        Some("500M"),
    );
    assert!(
        stdout.contains("started"),
        "the program never started:\n{stderr}"
    );
    assert!(
        stderr.contains("hard memory limit reached (512 MiB, BROOD_MEM_LIMIT)")
            && stderr.contains("one value this large"),
        "the abort did not name the limit and the cause:\n{stderr}"
    );
}

/// A loop that allocates a megabyte an iteration, with no `receive`, covered the soft
/// limit's whole headroom inside one scheduling quantum and reached the hard limit — an
/// abort — before the once-per-quantum check ran. A large allocation over the soft limit
/// now ends the quantum, so the loop stops with the catchable error.
#[test]
fn a_fast_allocating_loop_stops_at_the_soft_limit_not_the_hard_one() {
    let (_, stderr) = run_limited(
        "fast",
        r#"
(defn hoard (kept) (hoard (conj kept (string/repeat "x" 1000000))))
(hoard [])"#,
    );
    assert!(
        !stderr.contains("hard memory limit reached"),
        "the loop reached the hard limit:\n{stderr}"
    );
    assert!(
        stderr.contains("runtime error: memory limit exceeded"),
        "the loop was not stopped by the soft limit:\n{stderr}"
    );
}
