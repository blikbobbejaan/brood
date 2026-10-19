//! **The tier-0 escape audit names the path, and stays silent above tier 0** (ADR-416 stage 1).
//!
//! `BROOD_TIER0_AUDIT=<file>` makes `vm_run_bc` record, under a tier-0 ceiling, the call path
//! by which bytecode started. The audit is how ADR-416 sizes its later stages, so it must not
//! go quietly vacuous: the site is parsed out of a rendered backtrace, and a change in that
//! rendering (names qualified or not, locations present or not) would otherwise leave a file
//! of `unplaced` lines, or none, which reads as "tier 0 reaches everything".
//!
//! A spawned process body is the escape known before the audit existed (`run_process_body`
//! runs a VM-eligible body on the VM whatever the ceiling). When stage 5 makes tier 0 reach
//! process bodies, this test's first half inverts: the file must then stay empty.

use std::process::Command;

const PROGRAM: &str = r#"
(def me (self))
(def p (spawn (fn () (receive (x (send me (+ x 1)))))))
(send p 1)
(io/puts (str "answer = " (receive (y y))))
"#;

/// Run the program at `tier`, auditing into a fresh file; answer stdout and the file's lines
/// (`None` when no file was written).
fn run(tier: &str) -> (String, Option<String>) {
    let dir = std::env::temp_dir().join(format!("brood-tier0-audit-{}-{tier}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let program = dir.join("program.blsp");
    let audit = dir.join("audit.txt");
    let _ = std::fs::remove_file(&audit);
    std::fs::write(&program, PROGRAM).expect("write program");
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
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        stdout.trim(),
        "answer = 2",
        "the program did not run to its answer at tier {tier}.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (stdout, lines)
}

#[test]
fn a_tier0_process_body_is_recorded_by_name() {
    let (_, lines) = run("0");
    let lines = lines.expect("the audit wrote no file: no escape recorded at tier 0");
    assert!(
        lines
            .lines()
            .any(|l| l.starts_with("top\trun_process_body <- drive@")),
        "the audit did not name the process-body escape from the scheduler:\n{lines}"
    );
    assert!(
        !lines.lines().any(|l| l.starts_with("unplaced")),
        "the audit could not find vm_run_bc in its own backtrace:\n{lines}"
    );
}

#[test]
fn above_tier0_the_audit_writes_nothing() {
    for tier in ["1", "2"] {
        let (_, lines) = run(tier);
        assert_eq!(lines, None, "the audit wrote under BROOD_TIER={tier}");
    }
}
