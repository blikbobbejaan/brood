//! A `receive` inside an interrupt window, on a pool of ONE worker (ADR-494).
//!
//! The VM runs `proc/interruptible` as a region on its own frame stack (ADR-491), so a
//! `receive` in the thunk parks. ADR-494 closed the paths that still ran the thunk nested
//! under the builtin: `(apply proc/interruptible …)` on the VM, and every call on the
//! tier-0 machine (`BROOD_TIER=0`, which CI runs this binary under too), where the call is
//! now a frame. With one worker, a process that blocked its thread would leave the next
//! process to an overflow thread; the dirty-receive count is what tells a park from a block.
//!
//! What still runs nested — a window opened inside a builtin's callback, under a native
//! frame — blocks its worker, which leaves the executors so the queue moves on (ADR-100
//! §7.4), and an interrupt reaches it once it runs again, as it reaches a parked one.
//!
//! Its own binary: the pool size is process-wide and fixed at the first spawn. The tests
//! share the dirty-receive counter, so they run one at a time.

use std::sync::Mutex;

use brood::{Interp, process};

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Evaluate `src` in a fresh interpreter on a pool of one worker, printed.
fn run(src: &str) -> String {
    process::set_max_parallel(1);
    let mut interp = Interp::new();
    match interp.eval_str(src) {
        Ok(value) => interp.print(value),
        Err(error) => format!("error: {}", error.message),
    }
}

/// Helpers every program here shares: `await-waiting` polls a pid until it is parked in
/// `receive` (a barrier, not a guessed delay; `:dead` if it died instead), `interrupt-until` asks until a running
/// process takes the interrupt.
const PRELUDE: &str = r#"
    (def root (self))
    (defn spin (n) (spin (inc n)))
    (defn await-waiting (pid)
      (let (info (proc/info pid))
        (cond (nil? info) :dead
              (= :waiting (:status info)) :parked
              :else (do (sleep 1) (await-waiting pid)))))
    (defn interrupt-until (pid tries)
      (cond (proc/interrupt pid) :landed
            (= tries 0) :never
            :else (do (sleep 1) (interrupt-until pid (dec tries)))))
"#;

/// A process waits in a window opened by `open` (`(open ceiling thunk)`), and the program
/// checks: another process runs on the one worker meanwhile, the waiter answers once
/// released, a ceiling passed while it waited lands once it runs, and so does
/// `proc/interrupt` — which a waiting process refuses.
fn waits_in_a_window(open: &str) -> String {
    run(&format!(
        r#"{PRELUDE}
        (def open {open})
        (def waiter (spawn (send root [:waited (open nil (fn () (receive (:go :went))))])))
        (await-waiting waiter)
        (spawn (send root :ran))
        (def ran (receive (:ran :ran) (after 10000 :starved)))
        (send waiter :go)
        (def waited (receive ([:waited r] r) (after 10000 :waiter-lost)))
        (def late (spawn (send root [:late (first (open 20 (fn () (do (receive (:go nil)) (spin 0)))))])))
        (await-waiting late)
        (sleep 30)
        (send late :go)
        (def woken (spawn (send root [:woken (first (open nil (fn () (do (receive (:go nil)) (spin 0)))))])))
        (await-waiting woken)
        (def refused (proc/interrupt woken))
        (send woken :go)
        [ran waited
         (receive ([:late r] r) (after 10000 :late-lost))
         refused
         (interrupt-until woken 10000)
         (receive ([:woken r] r) (after 10000 :woken-lost))]
        "#
    ))
}

const ANSWERED: &str = "[:ran [:ok :went] :interrupted false :landed :interrupted]";

#[test]
fn a_receive_in_a_window_parks() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dirty = process::dirty_receive_block_count();
    assert_eq!(
        waits_in_a_window("(fn (ceiling thunk) (proc/interruptible ceiling thunk))"),
        ANSWERED
    );
    assert_eq!(
        process::dirty_receive_block_count(),
        dirty,
        "a receive inside a window blocked its worker"
    );
}

#[test]
fn a_receive_in_an_applied_window_parks() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dirty = process::dirty_receive_block_count();
    assert_eq!(
        waits_in_a_window("(fn (ceiling thunk) (apply proc/interruptible [ceiling thunk]))"),
        ANSWERED
    );
    assert_eq!(
        process::dirty_receive_block_count(),
        dirty,
        "a receive inside an applied window blocked its worker"
    );
}

#[test]
fn a_window_under_a_builtin_blocks_but_starves_nobody_and_is_still_interrupted() {
    // Opened in a `reduce` callback, the window's thunk runs under the builtin's native frame,
    // which nothing captures: the receive blocks the one worker. It leaves the executors, so
    // the next process runs all the same, and the interrupts land as they do on a parked one.
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dirty = process::dirty_receive_block_count();
    assert_eq!(
        waits_in_a_window(
            "(fn (ceiling thunk) (reduce [ceiling] nil (fn (_ c) (proc/interruptible c thunk))))"
        ),
        ANSWERED
    );
    assert!(
        process::dirty_receive_block_count() > dirty,
        "a receive under a builtin's frame no longer blocks: this test no longer covers that path"
    );
}

#[test]
fn a_receive_lane_parks_inside_a_window_and_out() {
    // `proc/receive-lane` (ADR-490) waits as `%receive` does, so on a capturing machine it
    // suspends: the tier-0 machine has to root and redo it as it does a `%receive`, or the
    // suspend surfaces as an empty `runtime error:`. Inside a window — how `ui-run` polls a
    // terminal once the window is a frame — and outside one.
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dirty = process::dirty_receive_block_count();
    let printed = run(&format!(
        r#"{PRELUDE}
        (def windowed (spawn (send root [:windowed (proc/interruptible nil (fn () (proc/receive-lane nil)))])))
        (def bare (spawn (send root [:bare (proc/receive-lane nil)])))
        (await-waiting windowed)
        (await-waiting bare)
        (send windowed :go)
        (proc/send-priority bare :key)
        [(receive ([:windowed r] r) (after 10000 :windowed-lost))
         (receive ([:bare r] r) (after 10000 :bare-lost))]
        "#
    ));
    assert_eq!(printed, "[[:ok [:go :normal]] [:key :priority]]");
    assert_eq!(
        process::dirty_receive_block_count(),
        dirty,
        "a receive-lane blocked its worker"
    );
}
