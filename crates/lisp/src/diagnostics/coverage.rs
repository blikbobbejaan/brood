//! Line-coverage recording — the second tier of ADR-148.
//!
//! `nest test --cover` reports FUNCTION coverage and needs no kernel support at all
//! (`std/tool/coverage.blsp` rebinds each project function through a counting shim).
//! This module backs `--cover-lines`: which source LINES actually executed.
//!
//! # Where the hook is, and why not somewhere cheaper
//!
//! The recording point is a bytecode instruction, [`Inst::RecordLine`], emitted at
//! COMPILE time and only when [`enabled`] is true. So an ordinary run's bytecode is
//! byte-for-byte what it always was — the interpreter never even sees the opcode —
//! and there is no per-instruction runtime check to pay for.
//!
//! A cheaper seam was tried first and does not work, which is worth recording so it
//! isn't retried: hook the tree-walking evaluator, where `Heap::form_pos` already
//! carries a form's file AND line. It records top-level forms correctly and *nothing
//! inside a function body*, because a compiled closure's body executes in
//! `exec_chunk` (the sole VM executor since ADR-100 Stage 5), never in `eval`. The
//! bytecode path is where the code actually runs, so that is where coverage has to
//! live.
//!
//! The instruction carries the LINE only. The file comes from the executing arm's
//! `CompiledArm::src_file`, which `exec_chunk` already holds — so nothing has to be
//! threaded through the hot recursive executor.
//!
//! # Recording
//!
//! Hits are `(file, line)` pairs in one process-wide set behind a mutex. Green
//! processes are multiplexed across OS threads, so the set must be shared rather than
//! thread-local: a line executed by any process counts. A lock per recorded line is
//! tolerable only because this mode is explicitly opt-in and explicitly a diagnostic
//! run, never a timing one.
//!
//! # Why there are two sets
//!
//! A percentage needs a denominator drawn from the SAME population as the numerator.
//! Counting "every line holding a form" against "lines that recorded a hit" compares
//! different things and systematically under-reports: a `defmodule` header, a docstring
//! and a `defn`'s own line are all forms, none is an instrumented node, so a fully
//! exercised file reports a fraction of itself.
//!
//! So the denominator is not inferred from source text at all — [`note_instrumented`]
//! records what the compiler actually instrumented, at compile time. Arms compile on
//! first CALL, so that alone is not enough either: the reporting side forces every
//! project function to compile before the suite runs
//! (`eval::compile::precompile` / `%coverage-precompile`), which is what puts a
//! never-called function into the denominator and nowhere else.

use crate::core::sync::lock;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

/// `file -> set of executed lines`. Ordered containers so a report comes out stable
/// without sorting at the boundary.
type Hits = BTreeMap<String, BTreeSet<u32>>;

fn hits() -> &'static Mutex<Hits> {
    static HITS: OnceLock<Mutex<Hits>> = OnceLock::new();
    HITS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// `file -> set of lines the compiler instrumented` — the denominator.
fn instrumented_lines() -> &'static Mutex<Hits> {
    static LINES: OnceLock<Mutex<Hits>> = OnceLock::new();
    LINES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// A branch edge: `(line, col, taken)`. Keyed by the test's position so sibling `if`s
/// from one `cond`/`match` are distinct; a branch is fully covered when both `taken`
/// values are recorded.
type BranchHits = BTreeMap<String, BTreeSet<(u32, u32, bool)>>;
/// The branch denominator: `(line, col)` decision points the compiler instrumented.
type BranchSites = BTreeMap<String, BTreeSet<(u32, u32)>>;

fn branch_hits() -> &'static Mutex<BranchHits> {
    static B: OnceLock<Mutex<BranchHits>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(BTreeMap::new()))
}
fn branch_sites() -> &'static Mutex<BranchSites> {
    static B: OnceLock<Mutex<BranchSites>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Is line recording armed? Read once and cached, so this is a relaxed load at
/// compile time and nothing at all at run time.
///
/// NB the cache means the flag must be set before anything builds an `Interp` — the
/// prelude is compiled during construction, and a chunk compiled without the flag has
/// no `RecordLine` in it. `nest` sets it in `main`, before `Cli` dispatch.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    super::debug_flags::is_armed(&ON, "BROOD_COVERAGE")
}

/// Record that `line` of `file` executed.
pub fn record(file: &str, line: u32) {
    // Every map here is taken poison-tolerant (`core::sync::lock`): one panic while a map
    // was held must not drop every later hit and report the run as 0% covered — which is
    // what `if let Ok(…)` and `Err(_) => Vec::new()` did. A poisoned set is still a set.
    lock(hits())
        .entry(file.to_string())
        .or_default()
        .insert(line);
}

/// Register the lines of `file` that an arm's bytecode was instrumented for — every
/// line that COULD record a hit. Called once per arm compile, from `compile_arm`.
pub fn note_instrumented(file: &str, lines: impl IntoIterator<Item = u32>) {
    lock(instrumented_lines())
        .entry(file.to_string())
        .or_default()
        .extend(lines);
}

/// Record that one edge (`taken`) of the branch at `line:col` of `file` executed.
pub fn record_branch(file: &str, line: u32, col: u32, taken: bool) {
    lock(branch_hits())
        .entry(file.to_string())
        .or_default()
        .insert((line, col, taken));
}

/// Register the `(line, col)` decision points an arm was instrumented for — the branch
/// denominator. Called once per arm compile, from `compile_arm`.
pub fn note_branches(file: &str, sites: impl IntoIterator<Item = (u32, u32)>) {
    lock(branch_sites())
        .entry(file.to_string())
        .or_default()
        .extend(sites);
}

/// Every `file -> [entry …]` of one coverage map, for the reporting side.
fn listed<T: Copy + Ord>(map: &Mutex<BTreeMap<String, BTreeSet<T>>>) -> Vec<(String, Vec<T>)> {
    lock(map)
        .iter()
        .map(|(file, entries)| (file.clone(), entries.iter().copied().collect()))
        .collect()
}

/// Every recorded branch edge, `file -> [(line, col, taken) …]` (for the reporting side).
pub fn branch_snapshot() -> Vec<(String, Vec<(u32, u32, bool)>)> {
    listed(branch_hits())
}

/// Every instrumented branch site, `file -> [(line, col) …]` — the branch denominator.
pub fn branch_instrumented() -> Vec<(String, Vec<(u32, u32)>)> {
    listed(branch_sites())
}

/// Every instrumented `(file, lines)` pair — the denominator, mirroring [`snapshot`].
pub fn instrumented() -> Vec<(String, Vec<u32>)> {
    listed(instrumented_lines())
}

/// Every recorded `(file, lines)` pair, for the reporting side.
pub fn snapshot() -> Vec<(String, Vec<u32>)> {
    listed(hits())
}

#[cfg(test)]
mod tests {
    /// A panic while the hit map was held poisons it. Recording must carry on and the
    /// report must still show what was recorded — before, every later hit was dropped
    /// and the snapshot came back empty, so the run read as 0% covered.
    #[test]
    fn a_poisoned_hit_map_keeps_recording_and_reporting() {
        super::record("before.blsp", 1);
        let _ = std::thread::spawn(|| {
            let _held = super::hits().lock();
            panic!("poison the coverage map");
        })
        .join();
        assert!(super::hits().is_poisoned(), "the setup must poison the map");
        super::record("after.blsp", 7);
        let snapshot = super::snapshot();
        assert!(
            snapshot.contains(&("before.blsp".to_string(), vec![1]))
                && snapshot.contains(&("after.blsp".to_string(), vec![7])),
            "a poisoned map lost coverage: {snapshot:?}"
        );
    }
}
