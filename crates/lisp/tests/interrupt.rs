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
    builtin_is_stopped_inside(
        "(string/repeat \"a\" 100000)",
        "(%regex-find-all \"a\" big)",
    );
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
    builtin_is_stopped_inside(
        "(vec (map (range 100000) (fn (i) [:clear])))",
        "(%ui-harvest big false)",
    );
}

// Compiled only with a Ruby grammar linked (`--features treesit-ruby`, or `treesit-grammars`):
// a plain `cargo test --test interrupt` runs one test fewer, and that is this one.
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

#[test]
fn case_maps_and_normalisation_are_interrupted_inside() {
    builtin_is_stopped_inside("(string/repeat \"ab \" 400000)", "(string/upper big)");
    builtin_is_stopped_inside("(string/repeat \"AB \" 400000)", "(string/lower big)");
    // A `Σ` makes lower-casing cut only after whitespace; it still polls.
    builtin_is_stopped_inside("(string/repeat \"ΑΣ \" 400000)", "(string/lower big)");
    builtin_is_stopped_inside(
        "(string/repeat \"é \" 400000)",
        "(string/normalize big :nfd)",
    );
}

#[test]
fn a_piecewise_case_map_answers_what_the_whole_string_would() {
    // `Σ` lower-cases to `ς` at a word's end and `σ` elsewhere, by its neighbours: pieces cut
    // anywhere else would answer differently near a cut.
    let text = "ΑΣ ΣΑ ΣΣ.Σ x".repeat(2000);
    let src = format!("(string/lower \"{text}\")");
    assert_eq!(
        run_at(Tier::Bytecode, src),
        format!("\"{}\"", text.to_lowercase())
    );
    let src = format!("(string/upper \"{text}ß\")");
    assert_eq!(
        run_at(Tier::Bytecode, src),
        format!("\"{}\"", format!("{text}ß").to_uppercase())
    );
}

#[test]
fn a_string_search_is_interrupted_inside() {
    builtin_is_stopped_inside("(string/repeat \"a\" 1000000)", "(%str-index-of big \"z\")");
    builtin_is_stopped_inside(
        "(string/repeat \"a\" 1000000)",
        "(%str-last-index-of big \"z\" nil)",
    );
}

#[test]
fn a_windowed_string_search_finds_a_match_across_a_window_edge() {
    // The search runs 16 KB windows; a needle straddling an edge, after multi-byte text,
    // is found at the char index a whole-string search gives, forwards and backwards.
    for pad in [16_382usize, 16_383, 16_384, 32_767] {
        let text = format!(
            "{}é{}bcd{}",
            "a".repeat(pad),
            "a".repeat(5),
            "a".repeat(40_000)
        );
        let expected = text.chars().count() - 40_000 - 3;
        let src = format!(
            "[(%str-index-of \"{text}\" \"bcd\") (%str-last-index-of \"{text}\" \"bcd\" nil) (%str-index-of \"{text}\" \"zz\") (%str-last-index-of \"{text}\" \"zz\" nil)]"
        );
        assert_eq!(
            run_at(Tier::Bytecode, src),
            format!("[{expected} {expected} -1 -1]"),
            "pad {pad}"
        );
    }
}

#[test]
fn compression_is_interrupted_inside() {
    let big = "(%string->utf8-bytes (string/repeat \"abcdefgh\" 400000))";
    builtin_is_stopped_inside(big, "(%gzip big 9)");
    builtin_is_stopped_inside(big, "(%zlib-compress big)");
    builtin_is_stopped_inside(big, "(%deflate big)");
    builtin_is_stopped_inside(big, "(%brotli big)");
    builtin_is_stopped_inside(
        "(%gzip (%string->utf8-bytes (string/repeat \"abcdefgh\" 400000)))",
        "(%gunzip big)",
    );
}

#[test]
fn hashing_is_interrupted_inside() {
    let big = "(%string->utf8-bytes (string/repeat \"abcdefgh\" 400000))";
    builtin_is_stopped_inside(big, "(%digest :sha256 big)");
    builtin_is_stopped_inside(big, "(%digest :blake3 big)");
    builtin_is_stopped_inside(big, "(%hmac :sha256 (%string->utf8-bytes \"k\") big)");
    builtin_is_stopped_inside(
        "(%string->utf8-bytes \"salt\")",
        "(%pbkdf2-sha256-bytes big big 100000000 32)",
    );
}

#[test]
fn a_region_in_tail_position_answers_and_closes() {
    // The driver runs `proc/interruptible` over a VM thunk as a region on its own frame
    // stack; in tail position its answer is the caller's return value, both ways it ends.
    let src = r#"
        (defn spin (n) (spin (inc n)))
        (defn finishes () (proc/interruptible nil (fn () 7)))
        (defn stopped () (proc/interruptible 0 (fn () (spin 0))))
        [(finishes) (first (stopped)) (proc/interrupt (self))]
    "#;
    for tier in [Tier::Bytecode, Tier::Native] {
        assert_eq!(
            run_at(tier, src.into()),
            "[[:ok 7] :interrupted false]",
            "{tier:?}"
        );
    }
}

#[test]
fn an_error_passes_through_a_region_and_closes_its_window() {
    let src = r#"
        (defn count-to (n limit) (if (= n limit) n (count-to (inc n) limit)))
        [(try (proc/interruptible nil (fn () (raise :boom))) (catch e :caught))
         (proc/interrupt (self))
         (count-to 0 100000)]
    "#;
    assert_eq!(run_at(Tier::Bytecode, src.into()), "[:caught false 100000]");
}

#[test]
fn a_region_inside_an_open_window_leaves_the_interrupt_to_the_outer() {
    let src = r#"
        (defn spin (n) (spin (inc n)))
        (first (proc/interruptible 0
                 (fn () [:inner (proc/interruptible nil (fn () (spin 0)))])))
    "#;
    assert_eq!(run_at(Tier::Bytecode, src.into()), ":interrupted");
}

#[test]
fn a_receive_inside_a_region_parks_and_an_interrupt_lands_after_it_resumes() {
    // More processes than any machine has workers, each parked in a `receive` inside an
    // interrupt region. Run nested under the builtin, each would block its worker thread —
    // the dirty count would climb and, with every worker held, nothing else would run. As a
    // region on the driver's own stack, each parks like any receive.
    let dirty = brood::process::dirty_receive_block_count();
    let src = r#"
        (def root (self))
        (defn spin (n) (spin (inc n)))
        (defn gather (k acc)
          (if (= k 0) acc
              (gather (dec k) (conj acc (receive ([:done r] r) (after 10000 :lost))))))
        (defn start (k acc)
          (if (= k 0) acc
              (start (dec k)
                     (conj acc (spawn (send root [:done (proc/interruptible nil
                                                           (fn () (receive (:go :went))))]))))))
        (defn release (ws) (when (not (empty? ws)) (do (send (first ws) :go) (release (rest ws)))))
        (def waiters (start 256 []))
        (sleep 200)
        (spawn (send root :ran))
        (def ran (receive (:ran :ran) (after 10000 :starved)))
        (release waiters)
        (def answers (gather 256 []))
        ;; Parked inside a region past its ceiling, then woken into an endless loop: the
        ;; ceiling is checked once it runs, and the interrupt lands.
        (def late (spawn (send root [:late (first (proc/interruptible 50
                                                   (fn () (do (receive (:go nil)) (spin 0)))))])))
        (sleep 150)
        (send late :go)
        ;; Woken from a region with no ceiling, then interrupted from outside.
        (def woken (spawn (send root [:woken (first (proc/interruptible nil
                                                     (fn () (do (receive (:go nil)) (spin 0)))))])))
        (sleep 100)
        (defn interrupt-until (pid tries)
          (cond (proc/interrupt pid) :landed
                (= tries 0) :never
                :else (do (sleep 10) (interrupt-until pid (dec tries)))))
        (send woken :go)
        [ran
         (= answers (vec (repeat 256 [:ok :went])))
         (receive ([:late r] r) (after 10000 :late-lost))
         (interrupt-until woken 500)
         (receive ([:woken r] r) (after 10000 :woken-lost))]
    "#;
    let mut interp = brood::Interp::new();
    let printed = match interp.eval_str(src) {
        Ok(value) => interp.print(value),
        Err(error) => format!("error: {}", error.message),
    };
    assert_eq!(printed, "[:ran true :interrupted :landed :interrupted]");
    assert_eq!(
        brood::process::dirty_receive_block_count(),
        dirty,
        "a receive inside a region blocked its worker"
    );
}
