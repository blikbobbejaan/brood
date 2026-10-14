//! Shared harness for the `nest` integration tests: a scratch directory, the workspace
//! root and its `.blsp` files, and the one-shot `nest` run most files make.
//!
//! Every file here used to carry its own copy of each; one definition now.
#![allow(dead_code)] // each test binary uses a subset

use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory under the system temp dir, removed (best effort) on drop. The `nest`
/// tests have no `tempfile` dev-dependency; this is the one copy.
pub struct TempDir {
    pub path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create `<temp>/brood-{tag}-{pid}-{nanos}` — unique per test process and per call.
pub fn temp_dir(tag: &str) -> TempDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("brood-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp dir");
    TempDir { path }
}

/// The workspace root (`crates/nest` → two levels up).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Every `.blsp` file under `dir`, recursively, sorted.
pub fn blsp_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "blsp")
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Run `nest ARGS…` in `dir`; stdout then stderr as one string, and whether it succeeded.
pub fn nest(dir: &Path, args: &[&str]) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_nest"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run nest");
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        out.status.success(),
    )
}
