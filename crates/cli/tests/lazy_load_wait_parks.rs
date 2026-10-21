//! **A first-use load another process is running makes its waiters park, not block.**
//!
//! A qualified call into an unloaded module loads it at the global-lookup miss (ADR-335).
//! When another process already holds that load, the miss waits for it in
//! `%require-await`'s `receive`, and from the miss path that `receive` sat under a native
//! frame: it could not park, so it blocked its OS worker. 200 processes making their first
//! call into one module together dirty-blocked up to 27 workers a run (2026-10-20); with the
//! load held in flight, 596-804 waits a run. The VM now sends that miss through
//! `%lazy-global` as an ordinary frame, where the wait parks.
//! Counted by the runtime's `dirty-receive-block` line under `BROOD_JIT_BAIL_TRACE`.

use std::process::Command;

const PROGRAM: &str = r#"
(def me (self))
;; A holder takes the load claim for `http` and keeps it 200 ms, so every child's first-use
;; miss finds the load in flight in ANOTHER process — the wait this is about, every run.
;; (Left to the scheduler, the race reproduced 0 to 27 times a run.)
(def holder
  (spawn
    (do (%registry-update! '*features-loading* :assoc-new ["http"] (self))
      (send me :claimed)
      (sleep 200)
      (%registry-update! '*features-loading* :dissoc ["http"] nil)
      (%require-release! "http"))))
(receive (:claimed nil))
;; `http/parse-url` is the FIRST use of `http`: the checker is off (it would load the module
;; eagerly), so each child loads it at its own lookup miss.
(defn child () (send me [:r (if (get (http/parse-url "http://a/b") :host) 1 0)]))
(def kids (map (range 0 200) (fn (i) (spawn (child)))))
(defn collect (left acc) (if (= left 0) acc (collect (dec left) (+ acc (receive ([:r v] v))))))
(io/puts (str "sum=" (collect 200 0)))
"#;

#[test]
fn processes_racing_a_first_use_load_park_while_it_finishes() {
    let path = std::env::temp_dir().join(format!("brood-lazy-wait-{}.blsp", std::process::id()));
    std::fs::write(&path, PROGRAM).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_brood"))
        .env("BROOD_NO_CHECK", "1")
        .env("BROOD_NO_CRASH_REPORT", "1")
        .env("BROOD_JIT_BAIL_TRACE", "1")
        .env("BROOD_IMAGE_TRACE", "1")
        // The tree-walker job runs everything on tier 0, where this path is not the VM's.
        .env_remove("BROOD_TIER")
        .env_remove("BROOD_NO_LAZY_LOAD")
        .arg(&path)
        .output()
        .expect("run brood");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("sum=200"),
        "every child should have parsed its url:\n{stdout}\n{stderr}"
    );
    // The case is a module MATERIALISED from the stdlib image. Loaded from source instead
    // (`BROOD_NO_STDIMAGE`, or no image built for this binary), the race never showed —
    // the unfixed runtime read 2 there and 596-804 from the image — so say so rather than
    // pass on a path that cannot fail.
    if !stderr
        .lines()
        .any(|l| l.starts_with("[image] install:") && !l.contains("nil sections"))
    {
        eprintln!("lazy_load_wait_parks: no stdlib image installed — the race needs one; vacuous");
        return;
    }
    let dirty = stderr
        .lines()
        .filter(|l| l.contains("dirty-receive-block"))
        .count();
    assert_eq!(
        dirty, 0,
        "{dirty} processes blocked their OS worker waiting for another process's load of \
         `http` (596-804 a run before the miss path handed the wait to an ordinary frame)\n{stderr}"
    );
}
