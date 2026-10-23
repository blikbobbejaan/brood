//! A module defining a root MACRO's name (ADR-423): inside the module its own definition
//! wins, so every bare `(name …)` there stops expanding the macro — and the definition says
//! so on stderr, unless the header declared the shadow with `(:shadow name)`. Shadowing a
//! root FUNCTION stays silent (ADR-236's designed style). Asserted on the real binary's
//! stderr because the warning is the kernel's, written before any Brood port exists to
//! capture it.

use std::process::Command;

fn run(dir: &std::path::Path, main: &str) -> (String, String) {
    std::fs::write(dir.join("main.blsp"), main).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_brood"))
        .current_dir(dir)
        .arg("main.blsp")
        .output()
        .expect("run brood");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_root_macro_shadow_warns_unless_declared_and_the_module_definition_wins() {
    let dir = std::env::temp_dir().join(format!("brood-macro-shadow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Undeclared: warned, and the module's own `spy` (a private one, like the case that found
    // this) is what a bare call reaches; `/spy` still reaches the macro, which prints `spy: 3`.
    let (stdout, stderr) = run(
        &dir,
        "(defmodule user)\n\
         (defn- spy (x) [:mine x])\n\
         (defn get (m k) :mine)\n\
         (io/puts (pr-str [(spy 1) (/spy 3)]))\n",
    );
    assert!(
        stdout.contains("[[:mine 1] 3]"),
        "the module's own `spy` must win a bare call, `/spy` reach the macro:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("`user/spy` shadows the prelude macro `spy`"),
        "an undeclared root-macro shadow must warn:\n{stderr}"
    );
    assert!(
        !stderr.contains("`get`"),
        "shadowing a root FUNCTION is the designed style and must stay silent:\n{stderr}"
    );

    // Declared: the same program, silent.
    let (stdout, stderr) = run(
        &dir,
        "(defmodule user (:shadow spy))\n\
         (defn- spy (x) [:mine x])\n\
         (io/puts (pr-str (spy 1)))\n",
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        stdout.contains("[:mine 1]"),
        "the declared shadow must still win:\n{stdout}\n{stderr}"
    );
    assert!(
        !stderr.contains("shadows the prelude macro"),
        "(:shadow spy) must silence the warning:\n{stderr}"
    );
}
