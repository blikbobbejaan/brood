//! **The tier-0 escape audit names the path, and stays silent above tier 0** (ADR-416 stage 1).
//!
//! `BROOD_TIER0_AUDIT=<file>` makes `vm_run_bc` record, under a tier-0 ceiling, the call path
//! by which bytecode started. The audit is how ADR-416 sizes its later stages, so it must not
//! go quietly vacuous: the site is parsed out of a rendered backtrace, and a change in that
//! rendering (names qualified or not, locations present or not) would otherwise leave a file
//! of `unplaced` lines, or none, which reads as "tier 0 reaches everything".
//!
//! A spawned process body was the escape known before the audit existed (`run_process_body`
//! ran a VM-eligible body on the VM whatever the ceiling). Since ADR-416 stage 4 a tier-0 body
//! runs on the tree-walker's capturing machine, so `PROGRAM` — a spawned body that parks in
//! `receive`, and a top level that does too — must leave the audit empty.
//!
//! The rest of what the audit found (devlog 2026-10-19) were doors from tree-walked code into
//! the VM that ignored the ceiling; `DOORS` walks through each, and the audit must stay empty.
//!
//! An empty file is also what a broken audit writes, so the audit's positive control lives
//! where bytecode can still be started under a tree-walk ceiling — directly, in the lisp
//! crate (`the_tier0_audit_records_bytecode_started_under_a_tree_walk_ceiling`).

use std::process::Command;

const PROGRAM: &str = r#"
(def me (self))
(def p (spawn (fn () (receive (x (send me (+ x 1)))))))
(send p 1)
(io/puts (str "answer = " (receive (y y))))
"#;

/// One call per closed door: a primitive calling back into Brood (`num/add` and `compare-to`
/// on a record, `->seq` under `first`), a `receive` matcher, and `%eval-in`'s forms.
const DOORS: &str = r#"
(defrecord usd (cents))
(defmethod num/add [usd usd] (a b) (usd (+ (get a :cents) (get b :cents))))
(defmethod compare-to [usd usd] (a b) (compare (get a :cents) (get b :cents)))
(defrecord stack (items))
(impl Seqable stack (->seq (s) (get s :items)))
(io/puts (str "add = " (get (+ (usd 3) (usd 4)) :cents)))
(io/puts (str "lt = " (< (usd 3) (usd 4))))
(io/puts (str "first = " (first (stack (list 10 20)))))
;; A guard makes the matcher a closure (a plain pattern is a native spec, ADR-409), and the
;; `defn` makes its body shared code, which is what gives it a compiled arm to resolve.
(defn wait-hi () (receive ([:hi n] :when (> n 3) (+ n 1))))
(send (self) [:hi 5])
(io/puts (str "receive = " (wait-hi)))
(io/puts (str "eval-in = " (%eval-in "(+ x 1)" {:x 41})))
"#;

const DOORS_ANSWER: &str = "add = 7\nlt = true\nfirst = 10\nreceive = 6\neval-in = 42";

/// Run `source` at `tier`, auditing into a fresh file; check it printed `answer` and answer
/// the file's lines (`None` when no file was written).
fn run(source: &str, answer: &str, tier: &str) -> Option<String> {
    let dir = std::env::temp_dir().join(format!(
        "brood-tier0-audit-{}-{tier}-{}",
        std::process::id(),
        answer.len()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let program = dir.join("program.blsp");
    let audit = dir.join("audit.txt");
    let _ = std::fs::remove_file(&audit);
    std::fs::write(&program, source).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_brood"))
        .env("BROOD_NO_CHECK", "1")
        .env("BROOD_NO_CRASH_REPORT", "1")
        .env("BROOD_TIER", tier)
        .env("BROOD_TIER0_AUDIT", &audit)
        .arg(&program)
        .output()
        .expect("run brood");
    let lines = std::fs::read_to_string(&audit).ok();
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        answer,
        "the program did not run to its answer at tier {tier}.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    lines
}

/// The escape the audit was built to find: before stage 4 the scheduler ran this body on the
/// VM (`run_process_body`) whatever the ceiling. Both the spawned body and the top level park
/// in `receive`, so this also covers capture and resume on the tree-walker.
#[test]
fn a_tier0_process_body_stays_on_the_tree_walker() {
    let lines = run(PROGRAM, "answer = 2", "0");
    assert_eq!(
        lines, None,
        "a tier-0 process body (or its receive) started bytecode"
    );
}

#[test]
fn above_tier0_the_audit_writes_nothing() {
    for tier in ["1", "2"] {
        let lines = run(PROGRAM, "answer = 2", tier);
        assert_eq!(lines, None, "the audit wrote under BROOD_TIER={tier}");
    }
}

#[test]
fn tree_walked_callbacks_stay_on_the_tree_walker() {
    let lines = run(DOORS, DOORS_ANSWER, "0");
    assert_eq!(
        lines, None,
        "tier-0 top-level code reached the VM through a door that ignores the ceiling"
    );
}
