//! `nest test` and the structured runner behind `nest mcp`'s `run-tests` discover the same
//! corpus: `tests/**/*_test.blsp` PLUS source files carrying co-located `describe` forms
//! (ADR-225). They once had a discovery each, and the structured one never saw a co-located
//! test.

use std::path::Path;
use std::process::Command;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn nest_test_and_the_structured_runner_run_the_same_tests() {
    let dir = std::env::temp_dir().join(format!("nest-test-discovery-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir.join("project.blsp"), "(project :name \"colo\")\n");
    write(
        &dir.join("src/colo.blsp"),
        "(defmodule colo (:use test))\n\
         (defn one () 1)\n\
         (describe \"co-located\"\n\
           (test \"one\" (assert= 1 (one))))\n",
    );
    write(
        &dir.join("tests/plain_test.blsp"),
        "(defmodule plain-test (:use test))\n\
         (describe \"plain\" (test \"two\" (assert= 2 (+ 1 1))))\n",
    );
    write(
        &dir.join("probe.blsp"),
        "(io/puts (str \"structured-total=\" (get (project-run/run-tests-structured) :total)))\n",
    );

    let nest = Command::new(env!("CARGO_BIN_EXE_nest"))
        .current_dir(&dir)
        .arg("test")
        .output()
        .expect("nest runs");
    let nest_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&nest.stdout),
        String::from_utf8_lossy(&nest.stderr)
    );
    let structured = Command::new(env!("CARGO_BIN_EXE_nest"))
        .current_dir(&dir)
        .args(["run", "--no-check", "probe.blsp"])
        .output()
        .expect("nest run probe");
    let structured_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&structured.stdout),
        String::from_utf8_lossy(&structured.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        nest_text.contains("2 tests, 2 passed"),
        "nest test must run the tests/ file and the co-located one:\n{nest_text}"
    );
    assert!(
        structured_text.contains("structured-total=2"),
        "the structured runner must discover what nest test does:\n{structured_text}"
    );
}
