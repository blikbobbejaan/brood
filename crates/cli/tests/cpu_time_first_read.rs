//! **The read that arms CPU accounting times the rest of its own quantum.**
//!
//! `process-info`'s `:cpu-us` is off until something asks (`process/cputime.rs`), and a
//! green process's figure counts from that moment. The first read used to arm accounting
//! without starting a clock for the quantum it ran in, so that quantum was never timed: a
//! process reading its `:cpu-us` before some work and again after it — when the `before`
//! was the runtime's first read — got 0 for the work. `tests/cpu_time_test.blsp` caught it
//! only when its self-timing test happened to make the suite's first read (the nightly's GC
//! stress job, 2026-10-18); a fresh runtime makes that the case every time.

use std::process::Command;

const SCRIPT: &str = r#"
(defn spin (n acc) (if (= n 0) acc (spin (dec n) (+ acc 1))))
(defn cpu-us () (:cpu-us (proc/info (self))))
(def me (self))
(spawn (let (before (cpu-us) _ (spin 300000 0) after (cpu-us)) (send me (- after before))))
(io/puts "used" (receive (n n) (after 30000 -1)))
"#;

#[test]
fn the_first_read_in_a_runtime_times_the_work_after_it() {
    let directory = std::env::temp_dir().join(format!("brood-cpu-first-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the scratch directory");
    std::fs::write(directory.join("main.blsp"), SCRIPT).expect("write the script");
    let output = Command::new(env!("CARGO_BIN_EXE_brood"))
        .arg("main.blsp")
        .current_dir(&directory)
        .output()
        .expect("run brood");
    let _ = std::fs::remove_dir_all(&directory);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let used: i64 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("used "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "no figure:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    // Linux only has the per-thread clock; elsewhere every figure is 0 by design.
    if cfg!(target_os = "linux") {
        assert!(
            used > 0,
            "300 000 iterations after the arming read were timed as {used} µs"
        );
    }
}
