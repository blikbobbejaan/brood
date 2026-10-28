//! Every function name the checker keys a rule on must still exist.
//!
//! Beyond signatures, the checker knows things about particular functions by name: which
//! operators are ring or extremum operators (`types/check/known_ops.rs`), which sequence
//! builtins have element-aware result types (`infer::seq_aware_call_ty`). A rename that
//! leaves one of those names behind does not fail anything — the rule just stops firing, and
//! the checker quietly knows less. The sibling `curated_sigs_exist.rs` covers the curated
//! signature table; this covers the rest, read out of the source rather than re-listed.

use brood::Interp;

/// Every double-quoted string in `src[from..]` up to the first line that is exactly `}`
/// (the end of the item that starts at `from`).
fn quoted_names(src: &str, from: usize) -> Vec<String> {
    let body = &src[from..];
    let end = body.find("\n}\n").unwrap_or(body.len());
    let body = &body[..end];
    let mut names = Vec::new();
    for line in body.lines() {
        // Skip comments: their backticked or quoted prose is not a key.
        let code = line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            names.push(after[..close].to_string());
            rest = &after[close + 1..];
        }
    }
    names
}

fn known_names() -> Vec<String> {
    let known_ops = include_str!("../src/types/check/known_ops.rs");
    let infer = include_str!("../src/types/check/infer.rs");
    let mut names = Vec::new();
    // Every `const NAME: &[&str] = &[ … ];` in known_ops.rs.
    for (at, _) in known_ops.match_indices(": &[&str] =") {
        let tail = &known_ops[at..];
        let end = tail.find("];").expect("a name set ends with `];`");
        names.extend(quoted_names(&tail[..end], 0));
    }
    let seq_aware = infer
        .find("fn seq_aware_call_ty(")
        .expect("infer.rs still defines `seq_aware_call_ty`");
    names.extend(quoted_names(infer, seq_aware));
    names.sort();
    names.dedup();
    names
}

#[test]
fn every_name_a_checker_rule_keys_on_exists() {
    let names = known_names();
    // Anchored by name from each source, so a reader that silently parses nothing fails
    // here rather than making the guard vacuous.
    for anchor in ["math/max", "string/->number", "seq/filter", "map/get-in"] {
        assert!(
            names.iter().any(|n| n == anchor),
            "the reader did not find `{anchor}` among {} parsed names — it has drifted \
             from the shape of known_ops.rs / seq_aware_call_ty",
            names.len()
        );
    }

    let mut interp = Interp::new();
    // The names span modules (`math/…`, `datetime/…`), and a bare image holds only the
    // prelude, so load everything before asking.
    let _ = interp
        .eval_str("(doseq (m (reflect/builtin-modules)) (try (require-one m) (catch _ nil)))");

    let mut stale = Vec::new();
    for name in &names {
        match interp.eval_str(&format!("(bound? '{name})")) {
            Ok(value) if interp.print(value).trim() == "true" => {}
            _ => stale.push(name.clone()),
        }
    }
    assert!(
        stale.is_empty(),
        "the checker keys a rule on these names, but none of them exists any more: \
         {stale:?}\nEach rule silently stopped firing. Update types/check/known_ops.rs or \
         infer::seq_aware_call_ty to the new name."
    );
}
