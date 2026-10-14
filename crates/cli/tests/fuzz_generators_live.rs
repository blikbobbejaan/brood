//! **Every fuzz generator must emit programs that RUN on today's language.**
//!
//! `scripts/fuzz/run.sh` is a differential: a program's output under the tree-walker, the
//! VM, the JIT and GC-stress must agree. That is evidence only when the program executes —
//! a generator emitting a renamed-away name makes every program die on its first form, all
//! four engines agree on an empty stdout, and the runner reports "0 divergences". The
//! generators are Brood scripts, but the programs they emit live in string templates no
//! static sweep reads, so this runs them.
//!
//! One program pair per generator, run once on the reference engine, must report no
//! `unbound symbol` and — for the generators whose programs are meant to run — print
//! something. Two generators' programs are MEANT to be rejected: `checker` (a malformed
//! `sig`; its preamble is real code, so the unbound-symbol rule still applies) and `syntax`
//! (random tokens — an unbound name there is the point, so it is held only to `run.sh`'s
//! no-crash rule and skipped here).
use std::path::PathBuf;
use std::process::Command;

mod support;
use support::repo_root;

#[test]
fn every_fuzz_generator_emits_programs_that_run() {
    let root = repo_root();
    let generators_dir = root.join("scripts/fuzz/generators");
    let work = std::env::temp_dir().join(format!("brood-fuzz-live-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let mut names: Vec<String> = std::fs::read_dir(&generators_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".blsp"))
        .map(|name| name.trim_end_matches(".blsp").to_string())
        .collect();
    names.sort();
    assert!(
        names.len() >= 8,
        "expected the generator directory to be populated, saw {names:?}"
    );
    let mut problems = Vec::new();
    for generator in &names {
        let out = work.join(generator);
        std::fs::create_dir_all(&out).unwrap();
        // Two programs, so a generator whose stale name sits on a random branch has two
        // draws to show it.
        let generation = Command::new(env!("CARGO_BIN_EXE_brood"))
            .arg(generators_dir.join(format!("{generator}.blsp")))
            .arg("--")
            .args(["2", "20260915"])
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            generation.status.success(),
            "{generator}.blsp failed: {}",
            String::from_utf8_lossy(&generation.stderr)
        );
        let mut files: Vec<PathBuf> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "blsp")
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "{generator}.blsp wrote no .blsp");
        if generator == "syntax" {
            continue;
        }
        let must_print = generator != "checker";
        for file in files {
            let run = Command::new(env!("CARGO_BIN_EXE_brood"))
                .env("BROOD_TIER", "0")
                .arg(&file)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&run.stderr);
            let stdout = String::from_utf8_lossy(&run.stdout);
            if let Some(line) = stderr.lines().find(|line| line.contains("unbound symbol")) {
                problems.push(format!("{generator}: {} — {}", file.display(), line.trim()));
            } else if must_print && stdout.trim().is_empty() {
                let first_error = stderr
                    .lines()
                    .find(|line| line.contains("error"))
                    .unwrap_or("")
                    .trim();
                problems.push(format!(
                    "{generator}: {} printed nothing — {first_error}",
                    file.display()
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    assert!(
        problems.is_empty(),
        "STALE GENERATOR(S) — the differential fuzz runs are vacuous until these run:\n  {}",
        problems.join("\n  ")
    );
}
