//! Interrupting a running computation (ADR-491): an endless loop inside an interrupt window
//! (`proc/interruptible`) raises a catchable `:interrupted` at its next safepoint, at every
//! execution tier; the interrupt is sticky, so a catch-all inside the work cannot keep it;
//! and a long builtin that iterates natively is stopped from INSIDE.
//!
//! Each program interrupts ITSELF through its window's ceiling (`proc/interruptible 0 …`):
//! the ceiling is due the moment the window opens, so the first safepoint or builtin poll
//! the work reaches raises it. That is the same probe a quit key or `proc/interrupt` sets,
//! minus the second process — no timing, no threads to race.

use brood::Interp;
use brood::eval::compile::{Tier, set_forced_ceiling};

/// Evaluate `src` at `tier` in a fresh interpreter on a worker-sized stack, printed.
fn run_at(tier: Tier, src: String) -> String {
    std::thread::Builder::new()
        .stack_size(brood::process::WORKER_STACK_BYTES)
        .spawn(move || {
            set_forced_ceiling(Some(tier));
            let mut interp = Interp::new();
            match interp.eval_str(&src) {
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
    (let (r (proc/interruptible 0 (fn () (spin 0 -1))))
      [(first r) (:kind (second r)) (:code (second r))])
"#;

const INTERRUPTED: &str = "[:interrupted :interrupted \"E0047\"]";

#[test]
fn an_endless_loop_is_interrupted_on_the_tree_walker() {
    assert_eq!(run_at(Tier::TreeWalk, ENDLESS_LOOP.into()), INTERRUPTED);
}

#[test]
fn an_endless_loop_is_interrupted_on_the_bytecode_vm() {
    assert_eq!(run_at(Tier::Bytecode, ENDLESS_LOOP.into()), INTERRUPTED);
}

#[cfg(feature = "jit")]
#[test]
fn an_endless_loop_is_interrupted_in_native_code() {
    assert_eq!(run_at(Tier::Native, ENDLESS_LOOP.into()), INTERRUPTED);
}

#[test]
fn a_loop_inside_a_builtin_callback_is_interrupted() {
    // `fold` calls the reducer from a native frame: the nested run's own safepoint raises
    // it, and it unwinds through the builtin.
    let src = r#"
        (defn spin (n) (spin (inc n)))
        (first (proc/interruptible 0 (fn () (fold [1 2 3] 0 (fn (acc x) (spin x))))))
    "#;
    assert_eq!(run_at(Tier::Bytecode, src.into()), ":interrupted");
}

#[test]
fn a_catch_all_inside_the_window_cannot_swallow_the_interrupt() {
    // The work catches everything and goes on: the interrupt is sticky, raised again at the
    // first safepoint after each handler, so it escapes. Afterwards the window is closed and a
    // loop runs to its end.
    let src = r#"
        (defn count-to (n limit) (if (= n limit) n (count-to (inc n) limit)))
        (defn stubborn (swallowed)
          (do (try (count-to 0 -1) (catch e nil)) (stubborn (inc swallowed))))
        [(first (proc/interruptible 0 (fn () (stubborn 0)))) (count-to 0 100000)]
    "#;
    for tier in [Tier::TreeWalk, Tier::Bytecode] {
        assert_eq!(
            run_at(tier, src.into()),
            "[:interrupted 100000]",
            "{tier:?}"
        );
    }
}

#[test]
fn a_window_that_finishes_answers_its_value_and_closes() {
    let src = r#"
        (defn count-to (n limit) (if (= n limit) n (count-to (inc n) limit)))
        [(proc/interruptible nil (fn () (count-to 0 100000))) (proc/interrupt (self))]
    "#;
    assert_eq!(run_at(Tier::Bytecode, src.into()), "[[:ok 100000] false]");
}

/// `call` (a form over `big`) run in a window whose ceiling is already due, after `setup`
/// built `big`: `[:interrupted :stopped-inside]` when the builtin's own poll stopped it. Had
/// it run to its end, the `send` right behind it — no safepoint between two native calls —
/// would leave `:finished` in the mailbox. The `count-to` before the window passes a quantum
/// boundary, so the budget the builtin starts on is fresh and the window's first safepoint
/// is not reached before the builtin is.
fn builtin_is_stopped_inside(setup: &str, call: &str) {
    let src = format!(
        r#"
        (defn count-to (n limit) (if (= n limit) n (count-to (inc n) limit)))
        (def big {setup})
        (count-to 0 5000)
        (let (r (proc/interruptible 0 (fn () (do {call} (send (self) :finished)))))
          [(first r) (receive (:finished :finished) (after 0 :stopped-inside))])
    "#
    );
    assert_eq!(
        run_at(Tier::Bytecode, src),
        "[:interrupted :stopped-inside]",
        "{call}"
    );
}

#[test]
fn a_sort_without_a_callback_is_interrupted_inside() {
    builtin_is_stopped_inside(
        "(vec (map (range 300000) (fn (i) (math/mod (* i 7919) 300007))))",
        "(%sort-asc big)",
    );
    builtin_is_stopped_inside(
        "(vec (map (range 300000) (fn (i) [(math/mod (* i 7919) 300007)])))",
        "(%sort-cmp big)",
    );
}

#[test]
fn string_natives_are_interrupted_inside() {
    builtin_is_stopped_inside("(string/repeat \"ab,\" 100000)", "(string/split big \",\")");
    builtin_is_stopped_inside("(string/repeat \"ab\" 100000)", "(string/->codepoints big)");
    builtin_is_stopped_inside("(vec (range 100000))", "(%string-join \",\" big)");
}

#[test]
fn a_regex_scan_is_interrupted_between_searches() {
    builtin_is_stopped_inside("(string/repeat \"a\" 100000)", "(%regex-find-all \"a\" big)");
}

#[test]
fn a_rope_walk_is_interrupted_inside() {
    builtin_is_stopped_inside(
        "(%string->rope (string/repeat \" \" 100000))",
        "(%rope-skip big 0 true \" \" true)",
    );
}

#[test]
fn a_json_encode_is_interrupted_inside() {
    builtin_is_stopped_inside("(vec (range 100000))", "(%json-encode-plain big)");
}

#[test]
fn a_line_diff_is_interrupted_inside() {
    builtin_is_stopped_inside(
        "[(string/join (map (range 20000) (fn (i) (str \"a\" i))) \"\\n\") (string/join (map (range 20000) (fn (i) (str \"b\" i))) \"\\n\")]",
        "(%diff-line-hunks (first big) (second big))",
    );
}

#[test]
fn a_ui_harvest_is_interrupted_inside() {
    builtin_is_stopped_inside("(vec (map (range 100000) (fn (i) [:clear])))", "(%ui-harvest big false)");
}

#[cfg(feature = "treesit-ruby")]
#[test]
fn a_tree_sitter_parse_is_cancelled_from_inside() {
    // ~4 MB of Ruby: a full parse takes far longer than the first progress callback, which
    // sees the ceiling due and cancels.
    builtin_is_stopped_inside(
        "(string/repeat \"def f(x)\\n  x + 1\\nend\\n\" 200000)",
        "(%tree-sitter-parse big :ruby)",
    );
}
