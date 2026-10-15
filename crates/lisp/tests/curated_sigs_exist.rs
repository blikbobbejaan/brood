//! Every name the checker curates a signature for must actually exist.
//!
//! `types/check/sigs.rs` hand-writes signatures for names `infer_sig` cannot walk. The
//! checker treats a curated name as KNOWN — so if a rename moves the function and the
//! entry is left behind, the checker stops reporting that name as unbound. Not "reports
//! the wrong type": stops reporting it at all. `(max 1 2)` then checks clean in ordinary
//! code, has no warning of any kind, and raises at run time.
//!
//! A stale entry is silent by construction, so the guard has to be mechanical: take the
//! table's own keys and require each to resolve in a fresh image.

use brood::Interp;

#[test]
fn every_curated_signature_names_something_that_exists() {
    // The table's own keys (`types::check::curated_sig_names`), not a hand-copied list.
    let names = brood::types::check::curated_sig_names();
    assert!(
        names.iter().any(|n| n == "io/puts") && names.iter().any(|n| n == "seq/filter"),
        "the curated table no longer holds `io/puts` and `seq/filter` among its {} names",
        names.len()
    );

    let mut interp = Interp::new();
    // The curated names span modules (`math/…`, `string/…`), and a bare image holds only
    // the prelude, so load everything before asking.
    let _ = interp
        .eval_str("(doseq (m (reflect/builtin-modules)) (try (require-one m) (catch _ nil)))");

    let mut stale = Vec::new();
    for name in &names {
        let query = format!("(bound? '{name})");
        match interp.eval_str(&query) {
            Ok(value) => {
                if interp.print(value).trim() != "true" {
                    stale.push(name.clone());
                }
            }
            Err(_) => stale.push(name.clone()),
        }
    }

    assert!(
        stale.is_empty(),
        "these names are curated in types/check/sigs.rs but no longer exist: {stale:?}\n\
         A curated name is treated as KNOWN by the checker, so each of these is a name it \
         will never report as unbound — silently accepting code that raises at run time. \
         Move the entry to the new name, or delete it."
    );
}
