//! A project's `:format-headers` (and its `:format-plugins`' rules) reach `nest format`.
//!
//! `project/setup` computed the table and `def`'d `format/*headers-extra*` from inside the
//! `project` module, which namespaced it to `project/*format-headers-extra*` — the formatter
//! reads the root dynamic var, so every project rule (Hatch's `deflive`/`on`/`tick` among
//! them) was silently ignored. The unit tests asserted on the helper that computes the
//! table, so they passed throughout; this one asserts on the file `nest format` writes.

use std::path::Path;
use std::process::Command;

fn format_project(manifest: &str) -> String {
    let root = std::env::temp_dir().join(format!(
        "brood-format-headers-{}-{}",
        std::process::id(),
        manifest.len()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("project.blsp"), manifest).unwrap();
    let source = root.join("src/a.blsp");
    std::fs::write(
        &source,
        "(defthing some-name [a b] (first-step-of-the-body with-several-arguments) \
         (second-step-of-the-body with-more-arguments here))\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nest"))
        .current_dir(&root)
        .arg("format")
        .output()
        .expect("run nest format");
    assert!(
        output.status.success(),
        "nest format failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let formatted = std::fs::read_to_string(&source).unwrap();
    let _ = std::fs::remove_dir_all(Path::new(&root));
    formatted
}

#[test]
fn a_project_header_rule_shapes_nest_format() {
    let formatted = format_project("(project :name scratch :format-headers {defthing 2})\n");
    assert_eq!(
        formatted,
        "(defthing some-name [a b]\n  \
         (first-step-of-the-body with-several-arguments)\n  \
         (second-step-of-the-body with-more-arguments here))\n",
        "the project's `defthing 2` rule did not reach the formatter"
    );
}

#[test]
fn without_a_rule_the_same_form_breaks_call_style() {
    // The control: proves the rule above is what kept `[a b]` on the head line.
    let formatted = format_project("(project :name scratch)\n");
    assert!(
        formatted.starts_with("(defthing some-name\n  [a b]\n"),
        "unexpected default shape:\n{formatted}"
    );
}
