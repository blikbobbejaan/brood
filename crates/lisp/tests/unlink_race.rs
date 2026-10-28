//! The link half of KI-213. `links::notify_peers` took a dead process's peers under `LINKS`,
//! released it, then delivered each `[:EXIT …]`; a trapping peer's `unlink` plus a
//! zero-timeout flush in that gap found no link and no message, and the `[:EXIT]` landed
//! after. The local deliveries now happen under the same hold of the lock as the take, as
//! the monitor side's `[:down]` pushes do.
//!
//! Evaluated as the ROOT process through `Interp`, for the reason `demonitor_race.rs` gives:
//! the test runner's own load hides the window.

use brood::Interp;

const STRESS: &str = r#"
(defn round (n leaks)
  (if (= n 0)
    leaks
    (let (me (self)
          t (spawn (receive ([:next from] (send from [:val 1])))))
      (link t)
      (send t [:next me])
      (receive ([:val _] nil) ([:EXIT ^t _] nil))
      (unlink t)
      (receive ([:EXIT ^t _] nil) (after 0 nil))
      (proc/sleep 0)
      (round (- n 1) (+ leaks (receive ([:EXIT ^t _] 1) (after 0 0)))))))
(defn worker (parent n)
  (proc/trap-exit true)
  (send parent [:leaks (round n 0)]))
(let (me (self) w 40 n 2000)
  (map (range 0 w) (fn (i) (spawn (worker me n))))
  (fold (range 0 w) 0 (fn (acc i) (receive ([:leaks k] (+ acc k))))))
"#;

#[test]
fn an_unlink_then_flush_never_leaves_an_exit_behind() {
    let mut interp = Interp::new();
    let leaked = interp.eval_str(STRESS).expect("the stress evaluates");
    assert_eq!(
        interp.print(leaked),
        "0",
        "[:EXIT]s left behind after unlink + flush across 80 000 rounds"
    );
}
