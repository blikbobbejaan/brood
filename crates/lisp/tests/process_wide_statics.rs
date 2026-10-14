//! **Every process-wide table is process-wide on purpose.**
//!
//! Several runtimes in one OS process is a real configuration — an embedder with several
//! `Interp`s, `cargo test` threads, the nightly sanitizer jobs — and state that belongs to a
//! runtime belongs on `RuntimeCode`, never in a `static`. KI-224 (the held image file, the
//! `defdyn` marks) and KI-226 (the module-absence memo, the image kind index) were each a
//! `static` holding one runtime's facts, found only when two runtimes happened to share a
//! process.
//!
//! This gate makes the choice explicit. Every `static` in `crates/lisp/src` whose type holds a
//! lock or a shared append-only vector — the shapes a table takes — must be listed in
//! [`PROCESS_WIDE`] with the reason it is not per runtime; a listed entry that no longer
//! exists fails too, so the list stays the truth. Counters, flags and `OnceLock`s of plain
//! values are not tables and are not listed; `thread_local!`s are per thread.
//!
//! Adding a table? Ask first whether it records something about ONE runtime: its globals,
//! its load path, the images it opened, its modules. If so it is a `RuntimeCode` field.

use std::path::{Path, PathBuf};

/// The allowed reasons, so an entry has to commit to one.
#[derive(Clone, Copy, Debug)]
enum Reason {
    /// Keyed by pid, handle id, symbol id or another identity unique across the whole OS
    /// process: runtimes cannot collide in it, and processes of different runtimes may
    /// legitimately address each other (messages cross runtimes as copies).
    ProcessUniqueKeys,
    /// One instance per OS process by design (the scheduler's workers, the timer thread, the
    /// network reactor, the JIT compiler thread, the GUI event loop, the node — ADR-033's
    /// "one node per OS process").
    OneMachine,
    /// Facts about the binary itself — its baked-in std sources, curated tables, compiled
    /// grammars — identical for every runtime it hosts.
    FactOfTheBinary,
    /// Diagnostics: profiling, coverage, audits and once-only warnings, read by a person.
    Diagnostic,
    /// A cache whose key fully determines its value, so sharing it is only a speed-up.
    PureCache,
    /// A speed decision keyed by a name: an entry another runtime made can only cost this
    /// runtime time, never change a result.
    SpeedHeuristic,
}

use Reason::*;

/// `(path under crates/lisp/src, static name, reason)`.
const PROCESS_WIDE: &[(&str, &str, Reason)] = &[
    // The symbol interner: symbols are the same `u32` in every runtime, which is what lets
    // a message cross between runtimes without re-interning.
    ("core/value.rs", "IDS", OneMachine),
    ("core/value.rs", "NAMES", OneMachine),
    ("core/value.rs", "NAME_HASHES", OneMachine),
    // Process tables keyed by pid; pids are unique across the OS process.
    ("process/mailbox.rs", "REGISTRY", ProcessUniqueKeys),
    ("process/monitor.rs", "MONITORS", ProcessUniqueKeys),
    ("process/links.rs", "LINKS", ProcessUniqueKeys),
    ("process/links.rs", "REMOTE_LINKS", ProcessUniqueKeys),
    ("core/table.rs", "REGISTRY", ProcessUniqueKeys),
    ("host/net.rs", "REGISTRY", ProcessUniqueKeys),
    ("host/subprocess.rs", "REGISTRY", ProcessUniqueKeys),
    ("host/wasm.rs", "REGISTRY", ProcessUniqueKeys),
    // The scheduler, timer, offload pool and stdin are one per machine; sysmon observes them.
    ("process/timer.rs", "TIMERS", OneMachine),
    ("process/sysmon.rs", "MONITORS", OneMachine),
    ("builtins/offload.rs", "POOL", OneMachine),
    ("builtins/io.rs", "STDIN_READER", OneMachine),
    ("builtins/filesystem.rs", "LOCK", OneMachine),
    // The node: one per OS process (ADR-033).
    ("dist.rs", "NODE", OneMachine),
    ("dist.rs", "NODES", OneMachine),
    ("dist.rs", "NAMES", OneMachine),
    ("dist.rs", "NODE_MONITORS", OneMachine),
    ("dist.rs", "LISTEN_ADDRS", OneMachine),
    ("dist.rs", "DIAL_ROUTES", OneMachine),
    ("dist.rs", "PENDING_DIALS", OneMachine),
    ("process/monitor.rs", "PENDING_REMOTE", OneMachine),
    // Native code lives in one executable mapping for the process, keyed by code pointer.
    ("jit.rs", "GLOBAL_JIT", OneMachine),
    (
        "eval/compile/jit_runtime/compiler.rs",
        "JIT_ARM_KEEPALIVE",
        ProcessUniqueKeys,
    ),
    (
        "eval/compile/jit_lower.rs",
        "JIT_INLINE_CHUNK_KEEPALIVE",
        OneMachine,
    ),
    (
        "eval/compile/jit_lower/i64.rs",
        "I64_TOO_DEEP",
        SpeedHeuristic,
    ),
    // Host services that exist once: the GUI event loop and its windows, the clipboard,
    // dynamically loaded grammars.
    ("host/gui/backend.rs", "G", OneMachine),
    ("host/gui/backend.rs", "S", OneMachine),
    ("host/gui/backend.rs", "W", OneMachine),
    ("host/clipboard.rs", "CLIPBOARD", OneMachine),
    ("host/treesit.rs", "DYNAMIC", OneMachine),
    // Facts of the binary: the std sources baked into it, and the signatures derived from
    // them (the stdlib id is a build constant).
    ("types/check.rs", "STD_REQUIRES", FactOfTheBinary),
    ("types/check/sigs.rs", "CACHE", FactOfTheBinary),
    ("eval/derive.rs", "SIGS", FactOfTheBinary),
    // Caches whose key determines the value.
    ("builtins/regex_native.rs", "CACHES", PureCache),
    ("builtins/regex_native.rs", "LEXERS", PureCache),
    ("host/treesit.rs", "TREE_CACHE", PureCache),
    ("host/treesit.rs", "PARSER_POOL", PureCache),
    ("host/gui.rs", "NAMES", PureCache),
    // Diagnostics.
    ("diagnostics/coverage.rs", "B", Diagnostic),
    ("diagnostics/coverage.rs", "HITS", Diagnostic),
    ("diagnostics/coverage.rs", "LINES", Diagnostic),
    ("diagnostics/profile.rs", "SAMPLES", Diagnostic),
    ("core/heap/features_audit.rs", "EVER_BOUND", Diagnostic),
    ("core/heap/vm_cache.rs", "PUBLISHED_FAST_LINKS", Diagnostic),
    ("process/scheduler/pool.rs", "QUANTUM_LEDGER", Diagnostic),
    ("eval/compile/closure.rs", "SEEN", Diagnostic),
    ("dist.rs", "SEEN", Diagnostic),
];

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// The name of the `static` declared on `line`, if it declares one.
fn declared_static(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    for visibility in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(stripped) = rest.strip_prefix(visibility) {
            rest = stripped;
            break;
        }
    }
    let rest = rest.strip_prefix("static ")?;
    let end = rest.find(':')?;
    let name = rest[..end].trim();
    (!name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        }))
    .then_some(name)
}

/// Is a declared type a table — something holding a lock or a shared append-only vector?
fn is_table(declared_type: &str) -> bool {
    !declared_type.contains("Cell<")
        && ["Mutex", "RwLock", "boxcar", "Registry"]
            .iter()
            .any(|shape| declared_type.contains(shape))
}

/// Every table `static` as `(path, name)`, outside `thread_local!` blocks.
fn table_statics() -> Vec<(String, String)> {
    let root = source_root();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read a source file");
        let relative = file
            .strip_prefix(&root)
            .expect("under the source root")
            .to_string_lossy()
            .replace('\\', "/");
        let lines: Vec<&str> = text.lines().collect();
        let mut thread_local_depth: i32 = 0;
        for (index, line) in lines.iter().enumerate() {
            if thread_local_depth > 0 || line.contains("thread_local!") {
                thread_local_depth += line.matches('{').count() as i32;
                thread_local_depth -= line.matches('}').count() as i32;
                continue;
            }
            let Some(name) = declared_static(line) else {
                continue;
            };
            // The type runs from the colon to the initialiser or the end of the item.
            let mut declared_type = line[line.find(':').expect("a typed static") + 1..].to_string();
            let mut next = index + 1;
            while !declared_type.contains('=') && !declared_type.contains(';') && next < lines.len()
            {
                declared_type.push_str(lines[next]);
                next += 1;
            }
            let declared_type = declared_type.split('=').next().unwrap_or_default();
            if is_table(declared_type) {
                found.push((relative.clone(), name.to_string()));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn every_process_wide_table_says_why() {
    let found = table_statics();
    assert!(
        found.len() > 20,
        "the scan found only {} table statics under {} — the scanner is broken, not the tree",
        found.len(),
        source_root().display()
    );
    let unlisted: Vec<String> = found
        .iter()
        .filter(|(path, name)| {
            !PROCESS_WIDE
                .iter()
                .any(|(listed_path, listed_name, _)| listed_path == path && listed_name == name)
        })
        .map(|(path, name)| format!("{path} {name}"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "a process-wide table that does not say why it is not per runtime. If it records \
         anything about ONE runtime (its globals, load path, images, modules), make it a \
         `RuntimeCode` field instead (KI-224, KI-226); otherwise add it to PROCESS_WIDE with \
         its reason:\n{}",
        unlisted.join("\n")
    );
}

#[test]
fn every_listed_table_still_exists() {
    let found = table_statics();
    let stale: Vec<String> = PROCESS_WIDE
        .iter()
        .filter(|(path, name, _)| {
            !found
                .iter()
                .any(|(found_path, found_name)| found_path == path && found_name == name)
        })
        .map(|(path, name, reason)| format!("{path} {name} ({reason:?})"))
        .collect();
    assert!(
        stale.is_empty(),
        "PROCESS_WIDE lists a table the source no longer declares — remove the entry:\n{}",
        stale.join("\n")
    );
}

#[test]
fn the_scanner_sees_a_table_and_skips_a_counter() {
    assert_eq!(
        declared_static("pub(super) static REGISTRY: LazyLock<Registry> ="),
        Some("REGISTRY")
    );
    assert_eq!(
        declared_static("    static CACHE: OnceLock<RwLock<HashMap<Symbol, Sig>>> ="),
        Some("CACHE")
    );
    assert_eq!(declared_static("let static_value = 1;"), None);
    assert!(is_table("OnceLock<RwLock<HashMap<Symbol, Sig>>>"));
    assert!(is_table("LazyLock<boxcar::Vec<String>>"));
    assert!(!is_table("AtomicU64"));
    assert!(!is_table("std::sync::OnceLock<bool>"));
    assert!(!is_table("RefCell<HashMap<String, Symbol>>"));
}
