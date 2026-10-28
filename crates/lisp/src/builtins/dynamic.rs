//! Dynamic variables — the kernel for `defdyn`/`binding`; the surface macros are in the
//! prelude. A dynamic variable's *value* resolves through the per-process binding stack
//! in the `Heap` (see `Heap::env_get`), so reads need no primitive here — only the
//! declaration, the scoped rebind, and the predicate.

use crate::core::heap::Heap;
use crate::core::value::{self, EnvId, Symbol, Value};
use crate::error::{LispError, LispResult};
use crate::eval::compile::apply_engine;

use super::args::{arg, expect_symbol};

/// Every primitive this file contributes: name, arity, signature, arglist, docstring.
pub(super) fn register(primitives: &mut super::Primitives) {
    use super::signature_types::*;
    use crate::core::value::Arity;
    use crate::types::Sig;
    // dynamic variables (the `defdyn`/`binding` surface is Brood — see prelude)
    primitives.def(
        "%declare-dynamic",
        Arity::exact(1),
        Sig::new(vec![sym], nil_ty),
        &[],
        "",
        declare_dynamic,
    );
    // `%binding`'s first arg is the *list/vector of names*, second is the
    // *list/vector of values*, third is the thunk — the macro `binding` emits
    // these as `(quote (*a* *b* …))` + `[v1 v2 …]` + `(fn () …)`.
    primitives.def(
        "%binding",
        Arity::exact(3),
        Sig::new(vec![seq, seq, callable], any),
        &[],
        "",
        binding,
    );
    primitives.def(
        "reflect/dynamic?",
        Arity::exact(1),
        Sig::new(vec![any], bool_ty),
        &["x"],
        "Whether x is a symbol declared dynamic with defdyn. Quote it: (reflect/dynamic? '*foo*).\n\n    (reflect/dynamic? 'map)   ;=> false",
        dynamic_p,
    );
}

/// `(%declare-dynamic 'name)` — mark a symbol as a dynamic variable, so
/// `binding` will accept it (and `reflect/dynamic?` reports it). `defdyn` expands to
/// this plus a plain `def` of the default value.
pub(super) fn declare_dynamic(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let sym = expect_symbol(heap, "%declare-dynamic", arg(args, 0))?;
    heap.mark_dynamic(sym);
    Ok(Value::symbol(sym))
}

/// `(reflect/dynamic? x)` — true when `x` is a symbol declared dynamic with `defdyn`.
/// A non-symbol is simply not dynamic (no error), so it composes in predicates.
pub(super) fn dynamic_p(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    Ok(Value::boolean(
        matches!(arg(args, 0), Value::Sym(s) if heap.is_dynamic(s)),
    ))
}

/// `(%binding syms vals thunk)` — run `thunk` (no args) with each dynamic var in
/// `syms` bound to the matching value in `vals` for the dynamic extent of the
/// call, restoring the previous bindings on return *or* error. `syms` (a quoted
/// list) and `vals` (a vector) are equal-length sequences built by the `binding`
/// macro — both emitted as unshadowable literals, so a local rebinding of `list`
/// can't break the form. Every name must be declared dynamic (else it's almost
/// certainly a typo — a plain global won't track the rebind). The bindings live
/// in this process's heap, so they don't reach a `spawn`ed child.
pub(super) fn binding(args: &[Value], env: EnvId, heap: &mut Heap) -> LispResult {
    let len = binding_enter(heap, arg(args, 0), arg(args, 1))?;
    let result = apply_engine(heap, arg(args, 2), &[], env);
    heap.truncate_dynamics(len);
    result
}

/// `%binding`'s entry, shared with the tree-walker's `Binding` frame: push `syms` (a sequence
/// of dynamic names) bound to `vals`, answering the dynamic stack's depth from before, which
/// the thunk's end — value or error — truncates back to.
pub(crate) fn binding_enter(heap: &mut Heap, syms: Value, vals: Value) -> Result<usize, LispError> {
    let syms = heap.seq_items(syms)?;
    let vals = heap.seq_items(vals)?;
    let mut names = Vec::with_capacity(syms.len());
    for s in &syms {
        names.push(expect_symbol(heap, "binding", *s)?);
    }
    let len = heap.dynamics_len();
    push_bindings(heap, &names, &vals)?;
    Ok(len)
}

/// Push `names` bound to `vals` on the dynamic stack — the one implementation behind the
/// `%binding` native, the VM's inline `binding` (`Inst::BindEnter`, ADR-402) and its
/// evaluator twin. Every name must be dynamic (else it is almost certainly a typo — a plain
/// global won't track the rebind), and the counts must match: matched lengths are a
/// guarantee of the `binding` MACRO, not of the primitive, and a raw
/// `(%binding '(*x*) [] thunk)` would otherwise bind a missing value as `nil` silently.
/// Both are checked before anything is pushed, so a bad `binding` leaves the stack as it was.
pub(crate) fn push_bindings(
    heap: &mut Heap,
    names: &[Symbol],
    vals: &[Value],
) -> Result<(), LispError> {
    for &sym in names {
        if !heap.is_dynamic(sym) {
            return Err(LispError::runtime(format!(
                "binding: {} is not a dynamic variable (declare it with defdyn)",
                value::symbol_name(sym)
            )));
        }
    }
    if names.len() != vals.len() {
        return Err(LispError::type_err(format!(
            "binding: {} name(s) but {} value(s) — each name needs exactly one value",
            names.len(),
            vals.len()
        )));
    }
    for (&sym, &val) in names.iter().zip(vals.iter()) {
        heap.push_dynamic(sym, val);
    }
    Ok(())
}
