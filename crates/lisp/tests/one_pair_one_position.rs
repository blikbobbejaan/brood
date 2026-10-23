//! One pair, one position (KI-288). The compiling VM keys a compiled closure on the identity
//! of its first body form (`VmCacheKey::Body`), and bakes the names of the scope the first
//! instance was built in into what it compiles. So the expanded code must never hold one pair
//! in two code positions: two closures sharing a first body form are one closure to the VM,
//! and the second runs the first one's body.
//!
//! Three sources put one pair in two positions, all after the expansion walk's own rebuilding
//! (or inside what it returns unrebuilt, a `quote`):
//!
//! - the counted range loop placed the fold literal's body forms in the loop AS THEY WERE,
//!   beside the fallback `fold` that keeps the literal — every `fn` inside the body was
//!   built in two scopes;
//! - the linear-map rewrites (the fold literal's fast copy, the defn split's inner loop)
//!   rebuilt every list but returned a `(quote …)` as it was, so a body opening with `'tag`
//!   gave the fast copy and the unrewritten one the same first body form: different
//!   closures, one cache entry;
//! - the expansion walk returned a `quote` form as it was, so a macro splicing one quoted
//!   argument into two `fn` bodies did the same.
//!
//! These pin the expansion: every pair reachable from it is reached once (a quoted datum's
//! interior is data, not a position, and may be shared). Each case also asserts the rewrite it
//! is about fired, so a probe that stops admitting the shape fails by name instead of
//! passing vacuously. `tests/linmap_soundness_test.blsp` and `tests/vm_closures_test.blsp`
//! assert the values.

use brood::Interp;
use brood::core::heap::Heap;
use brood::core::value::{self, Value, ValueRef};
use std::collections::HashSet;

/// The printed form of every pair `form` reaches more than once, outside quoted data.
fn shared_positions(heap: &Heap, form: Value) -> Vec<String> {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut shared: Vec<String> = Vec::new();
    let mut pending: Vec<Value> = vec![form];
    while let Some(current) = pending.pop() {
        match current.unpack() {
            ValueRef::Pair(_) => {
                // Walk the spine: each cell is a position of its own.
                let mut items: Vec<Value> = Vec::new();
                let mut cursor = current;
                while let ValueRef::Pair(pair) = cursor.unpack() {
                    if !seen.insert(pair.0) {
                        shared.push(brood::syntax::printer::print(heap, cursor));
                        break;
                    }
                    let (head, tail) = heap.pair(pair);
                    items.push(head);
                    cursor = tail;
                }
                let quoted = matches!(items.first().map(|item| item.unpack()),
                    Some(ValueRef::Sym(symbol)) if value::symbol_is(symbol, "quote"));
                if !quoted {
                    pending.extend(items);
                }
            }
            ValueRef::Vector(_) => {
                pending.extend(heap.list_or_vector_items(current).unwrap_or_default());
            }
            ValueRef::Map(id) => {
                for (key, entry) in heap.map_entries(id) {
                    pending.push(key);
                    pending.push(entry);
                }
            }
            ValueRef::Set(id) => pending.extend(heap.set_elems(id)),
            _ => {}
        }
    }
    shared
}

/// Evaluate `setup` (a macro the form uses), expand `src`, and require that the expansion
/// contains `fired` — the rewrite under test ran — and holds no pair in two positions.
fn assert_one_position(setup: &[&str], src: &str, fired: &str) {
    let mut interp = Interp::new();
    for definition in setup {
        interp.eval_str(definition).expect("setup");
    }
    let forms = brood::syntax::reader::read_all(&mut interp.heap, src).expect("read");
    let global = interp.heap.global();
    let form = *forms.first().expect("one form");
    let expanded =
        brood::eval::macros::macroexpand_all(&mut interp.heap, form, global).expect("expand");
    let printed = interp.print(expanded);
    assert!(
        printed.contains(fired),
        "expected `{fired}` in the expansion of {src}:\n{printed}"
    );
    let shared = shared_positions(&interp.heap, expanded);
    assert!(
        shared.is_empty(),
        "one pair in two positions in the expansion of {src}:\n  {}\nexpansion:\n{printed}",
        shared.join("\n  ")
    );
}

#[test]
fn the_counted_range_loop_copies_the_body_it_runs() {
    // The inner `fn` captures `k`; unfixed, the loop and the fallback `fold` held the same
    // `(fn (y) (+ y k))` pair, built in two scopes.
    assert_one_position(
        &[],
        "(def rp-range (fn (n k) (fold (range n) 0 (fn (acc i) (+ acc ((fn (y) (+ y k)) i))))))",
        "rng-loop",
    );
}

#[test]
fn the_fold_literal_s_fast_copy_has_its_own_leading_quote() {
    // Unfixed, the table-built literal and the unrewritten one shared `(quote tally)` — their
    // first body form, so the VM ran one body for both.
    assert_one_position(
        &[],
        "(def rp-fold (fn (xs seed) (fold xs seed (fn (m x) 'tally (assoc m x (inc (get m x 0)))))))",
        "%table-from-map",
    );
}

#[test]
fn the_split_loop_and_its_unrewritten_copy_have_their_own_leading_quote() {
    assert_one_position(
        &[],
        "(def rp-tally (fn (xs m) 'tally (if (empty? xs) m (rp-tally (rest xs) (%map-int-add m (first xs) 1)))))",
        "linmap-loop",
    );
}

#[test]
fn a_fused_pipeline_s_loop_copies_its_body() {
    assert_one_position(
        &[],
        "(def rp-pipe (fn (n) (fold (seq/lmap (range n) (fn (x) (* x 2))) 0 (fn (acc y) '(t) (+ acc y)))))",
        "rng-loop",
    );
}

#[test]
fn a_quoted_argument_spliced_into_two_bodies_is_two_forms() {
    // `(quote tag)` is the macro's ONE argument, spliced as the first body form of both.
    assert_one_position(
        &["(defmacro rp-both (q) `[(fn () ~q 1) (fn () ~q 2)])"],
        "(def rp-fns (rp-both 'tag))",
        "(quote tag) 2)",
    );
}

/// The same check on what a MODULE's code becomes: expanded and then resolved, through the
/// whole compile pass. `resolve_walk` hands back a list none of whose items it changed, so in
/// a module a pair the rewrites shared now reaches the compiler shared, exactly as at root —
/// before, the walk's rebuilding of every list hid it there.
fn assert_one_position_in_module(src: &str, fired: &str) {
    let mut interp = Interp::new();
    let forms = brood::syntax::reader::read_all(&mut interp.heap, src).expect("read");
    let global = interp.heap.global();
    let form = *forms.first().expect("one form");
    interp.heap.set_compile_ns(Some(value::intern("rp")));
    let compiled = brood::eval::macros::compile(&mut interp.heap, form, global).expect("compile");
    let printed = interp.print(compiled);
    assert!(
        printed.contains(fired),
        "expected `{fired}` in the compiled form of {src}:\n{printed}"
    );
    assert!(
        printed.contains("rp/"),
        "the form was not resolved into the module:\n{printed}"
    );
    let shared = shared_positions(&interp.heap, compiled);
    assert!(
        shared.is_empty(),
        "one pair in two positions in the compiled form of {src}:\n  {}\ncompiled:\n{printed}",
        shared.join("\n  ")
    );
}

#[test]
fn a_module_s_resolved_code_keeps_one_pair_in_one_position() {
    assert_one_position_in_module(
        "(def rp-range (fn (n k) (fold (range n) 0 (fn (acc i) (+ acc ((fn (y) (+ y k)) i))))))",
        "rng-loop",
    );
    assert_one_position_in_module(
        "(def rp-fold (fn (xs seed) (fold xs seed (fn (m x) 'tally (assoc m x (inc (get m x 0)))))))",
        "%table-from-map",
    );
}
