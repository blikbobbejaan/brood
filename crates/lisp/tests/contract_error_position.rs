//! An error leaving a contract shim is located at the user's call, on every tier.
//!
//! A thin contracted function's primitive runs at the shim's own `(%contract-orig …)` form,
//! and a failed argument check raises inside `std/contract.blsp`. The VM has never reported
//! either position (KI-184, `attach_vm_trace`); the tree-walker reported both, for every such
//! error, until 2026-10-18 — `std/contract.blsp:162` for a `(+ "s" 1)` the user wrote.
//!
//! This is a Rust test, not a case in `tests/contract_test.blsp`, because a `--test` body runs
//! in a spawned process, and a spawned process body runs on the VM whatever `BROOD_TIER` says:
//! written there, the tree-walker half of the guard passed with the fix removed.

use brood::eval::compile::{Tier, set_forced_ceiling};

/// `[file line]` of the error each probe catches, and every trace frame's file, printed.
const PROGRAM: &str = r#"
(defn thin (a b) (+ a b))
(sig! thin (any any -> any))
(defn checked (a b) (+ a b))
(sig! checked (int int -> int))
(defn call-thin (x)
  (let (r (thin x 1)) r))
(defn call-checked (x)
  (let (r (checked x 1)) r))
(defn where (e) [(get e :file) (get e :line) (seq/mapv (get e :trace) (fn (f) (get f :file)))])
[(where (try (call-thin "s") (catch e e)))
 (where (try (call-checked "s") (catch e e)))]
"#;

#[test]
fn an_error_leaving_a_contract_shim_names_the_users_call_on_every_tier() {
    for &tier in Tier::ALL {
        set_forced_ceiling(Some(tier));
        let mut interp = brood::Interp::new();
        let got = interp
            .eval_str(PROGRAM)
            .map(|v| interp.print(v))
            .unwrap_or_else(|e| panic!("{tier:?}: the program raised: {}", e.message));
        set_forced_ceiling(None);
        assert!(
            !got.contains("contract.blsp"),
            "{tier:?}: an error or a trace frame names the contract shim's file: {got}"
        );
        // The non-tail call sites: `(thin x 1)` on line 7 and `(checked x 1)` on line 9.
        assert!(
            got.starts_with("[[nil 7 ") && got.contains("] [nil 9 "),
            "{tier:?}: expected the errors at lines 7 and 9, got {got}"
        );
    }
}
