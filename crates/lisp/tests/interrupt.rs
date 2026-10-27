//! Interrupting a running computation (ADR-491): an endless loop inside an open interrupt
//! window raises a catchable `:interrupted` at its next safepoint, at every execution tier,
//! and a long builtin that iterates natively (a tree-sitter parse) is cancelled from inside.
//!
//! Each program interrupts ITSELF through its window's ceiling (`proc/allow-interrupt 0`):
//! the ceiling is due the moment the window opens, so the first quantum boundary the loop
//! reaches raises it. That is the same probe a quit key or `proc/interrupt` sets, minus the
//! second process — no timing, no threads to race.

use brood::Interp;
use brood::eval::compile::{Tier, set_forced_ceiling};

/// Evaluate `src` at `tier` in a fresh interpreter on a worker-sized stack, printed.
fn run_at(tier: Tier, src: &'static str) -> String {
    std::thread::Builder::new()
        .stack_size(brood::process::WORKER_STACK_BYTES)
        .spawn(move || {
            set_forced_ceiling(Some(tier));
            let mut interp = Interp::new();
            match interp.eval_str(src) {
                Ok(value) => interp.print(value),
                Err(error) => format!("error: {}", error.message),
            }
        })
        .expect("spawn interrupt test thread")
        .join()
        .expect("interrupt test thread panicked")
}

/// A self-tail loop that never ends unless `limit` is reached, warmed past the JIT's tiering
/// threshold with a finite run first so the endless one runs natively at the Native ceiling.
const ENDLESS_LOOP: &str = r#"
    (defn spin (n limit) (if (= n limit) n (spin (inc n) limit)))
    (defn warm (k) (if (= k 0) :warm (do (spin 0 1000) (warm (dec k)))))
    (warm 300)
    (try
      (do (proc/allow-interrupt 0) (spin 0 -1))
      (catch e [(:kind e) (:code e)]))
"#;

#[test]
fn an_endless_loop_is_interrupted_on_the_tree_walker() {
    assert_eq!(
        run_at(Tier::TreeWalk, ENDLESS_LOOP),
        "[:interrupted \"E0047\"]"
    );
}

#[test]
fn an_endless_loop_is_interrupted_on_the_bytecode_vm() {
    assert_eq!(
        run_at(Tier::Bytecode, ENDLESS_LOOP),
        "[:interrupted \"E0047\"]"
    );
}

#[cfg(feature = "jit")]
#[test]
fn an_endless_loop_is_interrupted_in_native_code() {
    assert_eq!(
        run_at(Tier::Native, ENDLESS_LOOP),
        "[:interrupted \"E0047\"]"
    );
}

#[test]
fn a_loop_inside_a_builtin_callback_is_interrupted() {
    // `fold` calls the reducer from a native frame: the nested run's own safepoint raises
    // it, and it unwinds through the builtin to the `try`.
    let src = r#"
        (defn spin (n) (spin (inc n)))
        (try
          (do (proc/allow-interrupt 0) (fold [1 2 3] 0 (fn (acc x) (spin x))))
          (catch e (:kind e)))
    "#;
    assert_eq!(run_at(Tier::Bytecode, src), ":interrupted");
}

#[test]
fn the_window_closes_as_the_interrupt_is_raised() {
    // The handler runs a loop of its own past several quantum boundaries: had the window
    // stayed open with its ceiling due, the handler would be interrupted too.
    let src = r#"
        (defn count-to (n limit) (if (= n limit) n (count-to (inc n) limit)))
        (try
          (do (proc/allow-interrupt 0) (count-to 0 -1))
          (catch e [(:kind e) (count-to 0 100000)]))
    "#;
    assert_eq!(run_at(Tier::Bytecode, src), "[:interrupted 100000]");
}

#[test]
fn a_closed_window_runs_to_the_end() {
    let src = r#"
        (defn count-to (n limit) (if (= n limit) n (count-to (inc n) limit)))
        (proc/allow-interrupt 0)
        (proc/inhibit-interrupt)
        (count-to 0 100000)
    "#;
    assert_eq!(run_at(Tier::Bytecode, src), "100000");
}

#[cfg(feature = "treesit-ruby")]
#[test]
fn a_tree_sitter_parse_is_cancelled_from_inside() {
    // ~4 MB of Ruby: a full parse takes far longer than the first progress callback, which
    // sees the ceiling due and cancels. The source is built BEFORE the window opens, so the
    // only code that can be interrupted is the parse itself.
    let src = r#"
        (def source (string/repeat "def f(x)\n  x + 1\nend\n" 200000))
        (try
          (do (proc/allow-interrupt 0) (%tree-sitter-parse source :ruby) :parsed)
          (catch e (:kind e)))
    "#;
    assert_eq!(run_at(Tier::Bytecode, src), ":interrupted");
}
