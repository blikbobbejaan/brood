//! Helpers more than one `brood` integration test needs, defined once.
//!
//! Deliberately NOT here: anything that mutates the process environment. `env_isolation`
//! requires a file touching `set_var`/`remove_var` to hold at most one `#[test]`, and it
//! counts per FILE — a helper here that set a variable would let every test binary that
//! includes this module mutate the environment unseen.
#![allow(dead_code)] // each test binary uses a subset

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use brood::eval::compile::{set_forced_ceiling, Tier};
use brood::types::check::check_file;
use brood::Interp;

/// The workspace root (`crates/lisp` → two levels up).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Every `.rs` file under `directory`, recursively, appended to `out`.
pub fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
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

/// Is `sym` globally bound in this image?
pub fn bound(interp: &mut Interp, sym: &str) -> bool {
    let out = interp
        .eval_str(&format!("(bound? '{sym})"))
        .unwrap_or_else(|error| panic!("evaluating (bound? '{sym}) failed: {error}"));
    interp.print(out) == "true"
}

/// Has `module` been loaded into this image (is it in `*features*`)?
pub fn loaded(interp: &mut Interp, module: &str) -> bool {
    let value = interp
        .eval_str(&format!("(contains? *features* \"{module}\")"))
        .expect("read *features*");
    interp.print(value) == "true"
}

/// The checker warnings a self-contained snippet draws on the single-file path: a fresh
/// `Interp` (prelude only — no project, no pre-loaded modules) and the same `check_file`
/// entry point `brood --check` and the LSP call. A fresh `Interp` per call keeps cases
/// independent.
pub fn warnings(src: &str) -> Vec<String> {
    let mut interp = Interp::new();
    let forms = brood::syntax::reader::read_all(&mut interp.heap, src).expect("parse");
    check_file(&mut interp.heap, &forms)
        .into_iter()
        .map(|(_, message)| message)
        .collect()
}

static MEM_GUARD: LazyLock<()> = LazyLock::new(|| {
    brood::core::alloc::init_limits_with_default(
        brood::core::alloc::TEST_DEFAULT_HARD,
        brood::core::alloc::TEST_DEFAULT_SOFT,
    );
});

/// Evaluate `src` in a fresh interpreter pinned to one engine. `Ok(printed)` or
/// `Err(message)` — the message alone (engine-independent), not the position, which
/// is asserted separately in `basic.rs`.
pub fn eval_on(src: &str, ceiling: Tier) -> Result<String, String> {
    LazyLock::force(&MEM_GUARD);
    set_forced_ceiling(Some(ceiling));
    let mut interp = Interp::new();
    let out = match interp.eval_str(src) {
        Ok(value) => Ok(interp.print(value)),
        Err(error) => Err(error.message.clone()),
    };
    set_forced_ceiling(None);
    out
}
