//! The parallel cold load (ADR-426, the default; `BROOD_LOAD_SEQUENTIAL=1` opts out): a
//! cold `nest run` of a project with many modules loads them in a pool of processes, each
//! `require-one`ing modules off a queue. It must build the SAME project as the sequential walk
//! — the opt-out, and the control arm here — so every case runs both and compares:
//!
//! - the program's answer, over modules that depend on each other through `(:use …)`, a
//!   macro whose expansion names another module, a registry another module reads, and a
//!   headerless file's root-level global;
//! - the record beside the image (owners, pins, registries — a parallel build once carried
//!   the require-waiters registry, pids and all, and the `dev` module's registries);
//! - what a WARM start materialises: the walk's own requires once became edges of the root
//!   section, which a warm start replays, loading the whole project (0.2 s → 1.3 s);
//! - a module that throws at load fails the build, naming its file, instead of hanging.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MODULES: usize = 24;

fn fixture(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("brood-parallel-load-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("project.blsp"),
        "(project\n  :name pl\n  :main main)\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/mac.blsp"),
        "(defmodule mac)\n(def here true)\n(defmacro twice (x) `(* 2 ~x m0/base))\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/regs.blsp"),
        "(defmodule regs)\n(def here true)\n(registry/put! :pl/hooks :a 1)\n(registry/put! :pl/hooks :b 2)\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/loose.blsp"), "(def loose-k 7)\n").unwrap();
    for i in 0..MODULES {
        // a chain: every module after the first `:use`s its predecessor, and every third one
        // also expands `mac`'s macro (whose expansion names `m0`)
        let header = if i == 0 {
            String::new()
        } else {
            format!(" (:use m{})", i - 1)
        };
        let body = if i == 0 {
            "(def base 3)\n(defn v () base)".to_string()
        } else if i % 3 == 0 {
            format!("(defn v () (+ (m{}/v) (mac/twice {i})))", i - 1)
        } else {
            format!("(defn v () (+ (m{}/v) {i}))", i - 1)
        };
        std::fs::write(
            dir.join(format!("src/m{i}.blsp")),
            format!("(defmodule m{i}{header})\n(def here true)\n{body}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        dir.join("src/main.blsp"),
        format!(
            "(defmodule main)\n\
             (defn main ()\n\
               (io/puts (str \"ANSWER \" (m{}/v) \" \" loose-k \" \"\n\
                          (sort (keys (or (registry/entries :pl/hooks) {{}}))))))\n",
            MODULES - 1
        ),
    )
    .unwrap();
    dir
}

fn nest(dir: &Path, args: &[&str], sequential: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nest"));
    cmd.args(args)
        .current_dir(dir)
        .env("BROOD_NO_CRASH_REPORT", "1");
    if sequential {
        cmd.env("BROOD_LOAD_SEQUENTIAL", "1");
    } else {
        cmd.env_remove("BROOD_LOAD_SEQUENTIAL");
    }
    cmd.output().expect("spawn nest")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn answer(out: &Output) -> String {
    let all = text(out);
    assert!(out.status.success(), "nest failed:\n{all}");
    all.lines()
        .find(|l| l.starts_with("ANSWER "))
        .unwrap_or_else(|| panic!("no ANSWER:\n{all}"))
        .to_string()
}

/// Cold build, then a warm run that reports how many image sections it materialised.
fn build(dir: &Path, sequential: bool) -> (String, String, usize) {
    let _ = std::fs::remove_dir_all(dir.join(".brood"));
    let cold = answer(&nest(dir, &["run", "--no-check"], sequential));
    let record = std::fs::read_to_string(dir.join(".brood/image.modules")).expect("the record");
    let mut warm = Command::new(env!("CARGO_BIN_EXE_nest"));
    warm.args(["run", "--no-check"])
        .current_dir(dir)
        .env("BROOD_NO_CRASH_REPORT", "1")
        .env("BROOD_IMAGE_TRACE", "1");
    let warm = warm.output().expect("spawn nest");
    assert_eq!(
        answer(&warm),
        cold,
        "the warm run answers like the cold one"
    );
    let sections = text(&warm)
        .lines()
        .filter(|l| l.starts_with("[image-section]"))
        .count();
    (cold, record, sections)
}

#[test]
fn a_parallel_cold_load_builds_what_the_sequential_one_does() {
    let dir = fixture("same");
    let (seq_answer, seq_record, seq_sections) = build(&dir, true);
    let (par_answer, par_record, par_sections) = build(&dir, false);
    assert_eq!(par_answer, seq_answer);
    assert!(par_answer.contains(" 7 (:a :b)"), "{par_answer}");
    assert_eq!(
        par_record, seq_record,
        "the record beside the image differs"
    );
    assert_eq!(
        par_sections, seq_sections,
        "a warm start materialised a different number of sections after a parallel build"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_module_that_throws_at_load_fails_the_parallel_build() {
    let dir = fixture("throws");
    std::fs::write(
        // the LAST module of the chain: nothing depends on it, so no waiter retries its load
        // (a failing load deep in a chain is slow on both paths — ROADMAP)
        dir.join(format!("src/m{}.blsp", MODULES - 1)),
        format!(
            "(defmodule m{} (:use m{}))\n(def here true)\n(error \"boom at load\")\n(defn v () 0)\n",
            MODULES - 1,
            MODULES - 2
        ),
    )
    .unwrap();
    let out = nest(&dir, &["run", "--no-check"], false);
    let all = text(&out);
    assert!(
        !out.status.success(),
        "a failing module must fail the build:\n{all}"
    );
    assert!(
        all.contains("boom at load"),
        "the error is reported:\n{all}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
