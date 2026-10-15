//! A non-UTF-8 argument is a filename the OS allows, so `nest` must report on it, not
//! panic. `main` reads argv before clap to route the Brood-implemented subcommands, and
//! `std::env::args()` panics outright on a non-UTF-8 argument — `nest check <that file>`
//! died with exit 101 before printing anything of its own.

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

#[test]
fn a_non_utf8_argument_is_reported_not_a_panic() {
    let out = Command::new(env!("CARGO_BIN_EXE_nest"))
        .arg("check")
        .arg(OsStr::from_bytes(b"not-utf8-\xff.blsp"))
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(101), "nest panicked:\n{stderr}");
    assert!(!stderr.contains("panicked"), "nest panicked:\n{stderr}");
}
