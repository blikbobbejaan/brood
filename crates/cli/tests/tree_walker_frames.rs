//! **The tree-walker's pending work lives on its heap frame stack** (ADR-416 stage 2).
//!
//! A non-tail call on the tree-walker used to be a nested Rust `eval`, so runaway recursion was
//! caught by the native-stack byte budget. Now every pending argument, test, binding and body
//! form is a continuation on the machine's own stack, and the limit is a frame count. Both
//! halves are observable from a program: a recursion deeper than the old byte budget allowed
//! completes, and a runaway one fails with the frame-limit message, catchably.
//!
//! Top-level forms run on the tree-walker under `BROOD_TIER=0` (a spawned process body still
//! runs on the VM until ADR-416 stage 5), so the recursion is driven from the top level.

use std::process::Command;

const PROGRAM: &str = r#"
(defn deep (n) (if (= n 0) 0 (+ 1 (deep (- n 1)))))
(io/puts (str "deep = " (deep 200000)))
(io/puts (str "runaway = " (try (deep 100000000) (catch e (get e :message)))))
"#;

#[test]
fn tier0_recursion_is_bounded_by_frames_not_native_stack() {
    let dir = std::env::temp_dir().join(format!("brood-tw-frames-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let program = dir.join("program.blsp");
    std::fs::write(&program, PROGRAM).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_brood"))
        .env("BROOD_NO_CHECK", "1")
        .env("BROOD_NO_CRASH_REPORT", "1")
        .env("BROOD_TIER", "0")
        .arg(&program)
        .output()
        .expect("run brood");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some("deep = 200000"),
        "a 200 000-deep non-tail recursion did not complete on the tree-walker.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let runaway = lines.next().unwrap_or_default();
    assert!(
        runaway.starts_with("runaway = recursion too deep: exceeded the tree-walker's"),
        "runaway recursion was not stopped by the tree-walker's frame limit: {runaway:?}\n\
         stderr:\n{stderr}"
    );
}
