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

/// A recursion that runs through an `&optional` default: each level's call is pending inside
/// the default of the level above, which entering a closure evaluates as a frame (stage 2b),
/// not as a nested native `eval`.
const DEFAULTS: &str = r#"
(defn depth (n &optional (acc (if (= n 0) 0 (+ 1 (depth (- n 1)))))) acc)
(io/puts (str "depth = " (depth 100000)))
"#;

/// Run `source` at tier 0; its stdout and stderr.
fn run_tier0(name: &str, source: &str) -> (String, String) {
    let dir = std::env::temp_dir().join(format!("brood-tw-frames-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let program = dir.join("program.blsp");
    std::fs::write(&program, source).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_brood"))
        .env("BROOD_NO_CHECK", "1")
        .env("BROOD_NO_CRASH_REPORT", "1")
        .env("BROOD_TIER", "0")
        .arg(&program)
        .output()
        .expect("run brood");
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn tier0_recursion_is_bounded_by_frames_not_native_stack() {
    let (stdout, stderr) = run_tier0("deep", PROGRAM);
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

#[test]
fn tier0_optional_defaults_run_as_frames() {
    let (stdout, stderr) = run_tier0("defaults", DEFAULTS);
    assert_eq!(
        stdout.lines().next(),
        Some("depth = 100000"),
        "a 100 000-deep recursion through an `&optional` default did not complete on the \
         tree-walker.\nstderr:\n{}",
        stderr.lines().take(8).collect::<Vec<_>>().join("\n")
    );
}

/// A native calling back into Brood (`%try`, under `try`) enters the thunk across a boundary.
/// An error raised without a position by the thunk's tail form (`(let)` has no bindings to
/// complain at) takes the position of the body's last form, the `do` on line 2, as the
/// boundary's own loop gave it and as the VM does.
const TAIL_POSITION: &str = r#"(io/puts (try
  (do
    (io/puts "a")
    (let))
  (catch e (str "line = " (get e :line)))))
"#;

#[test]
fn tier0_boundary_tail_error_takes_the_tail_forms_position() {
    let (stdout, stderr) = run_tier0("tail-pos", TAIL_POSITION);
    assert_eq!(
        stdout.lines().nth(1),
        Some("line = 2"),
        "an untagged error leaving a native-boundary closure's tail lost the tail form's \
         position.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
