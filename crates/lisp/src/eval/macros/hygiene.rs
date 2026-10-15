//! Automatic binding hygiene (ADR-066 amendment): a quasiquote template's own binders
//! are alpha-renamed before the template is built.

use super::*;

// ============================================================================
// Automatic binding hygiene (ADR-066 amendment — "Option A")
// ============================================================================
//
// A quasiquote template's OWN lexical binders — the `let`/`letrec`/`fn` binders it
// introduces as *literal* symbols — are alpha-renamed to fresh gensyms before the
// template is built, so a template binder can neither capture nor be captured by
// caller code spliced in via `~`/`~@`. Hygiene is thus the DEFAULT: a macro no
// longer needs `x#` or `(gensym)` for a safe temp binding.
//
// - Free-reference hygiene (a template's `helper`/`map` resolving to the *defining*
//   namespace) is already handled by the auto-qualifying resolver (ADR-065 §7); this
//   closes the remaining half — *introduced-binding* capture (ADR-066 concern #2) —
//   without the per-symbol lexical context (fat `Value::Sym`) full Scheme hygiene
//   needs, which ADR-066 rejected on ship-by-name/homoiconicity/GC grounds.
// - Scope-aware: a binder renames only the references it actually binds, so a
//   same-named prelude reference elsewhere in the template is untouched (correct
//   even when a binder shadows a prelude name). Hence a scope map, not `#`'s flat
//   name→gensym table.
// - Fresh per expansion: a template that introduces a renamable binder takes the
//   runtime expand path (like `#`), so two nested expansions of one macro —
//   `(m (m x))` — get distinct binders. `template_introduces_binder` gates the
//   static-quasiquote optimisation accordingly.
// - Opt-out for intentional anaphora (a name the template deliberately exposes to
//   the caller, e.g. `it` in an `aif`, or `defseq`'s `item`/`acc`): write `~'it` —
//   an unquoted quoted symbol, which lands in a `~unquote` hole the rename never
//   descends, emitting a literal `it`.
//
// v1 scope: only `let`/`letrec`/`fn` PLAIN-SYMBOL binders are renamed. Destructuring
// binders, `match*` pattern binders, and computed (`~params`/`~bindings`) or `defn`
// binders inside a template stay literal — a sound under-approximation (leaving a
// binder un-renamed only preserves the pre-change capturable-but-explicit behaviour;
// it never miscompiles a real macro), opt into `#`/`(gensym)` there as before. The
// one documented non-soundness is the pathological case of an outer template binder
// shadowed by a *computed* (`~params`/`~bindings`) inner binder of the same name.

/// A hygiene scope frame: original binder symbol → its replacement in the output.
/// A replacement equal to the original means "bound here, but not renamed" (a
/// non-plain-symbol or `#`/`_`/`&`/qualified binder) — it still SHADOWS an outer
/// rename so an inner reference stays literal.
type HygScope = Vec<(value::Symbol, value::Symbol)>;

/// Look up `s` (innermost — last — binding wins).
fn hyg_lookup(scope: &[(value::Symbol, value::Symbol)], s: value::Symbol) -> Option<value::Symbol> {
    scope
        .iter()
        .rev()
        .find(|(orig, _)| *orig == s)
        .map(|(_, t)| *t)
}

/// A plain binder we rename: not `_`, not a `&`-marker, not `#`-suffixed
/// (auto-gensym owns those), not already qualified.
fn hyg_renamable(s: value::Symbol) -> bool {
    let n = value::symbol_name_ref(s);
    !(n == "_" || n.starts_with('&') || n.contains('/') || (n.len() > 1 && n.ends_with('#')))
}

/// A fresh replacement symbol for a binder named like `orig`.
fn hyg_fresh(orig: value::Symbol) -> value::Symbol {
    let name = value::symbol_name(orig); // owned, so gensym can re-enter the interner
    match value::gensym(&name).unpack() {
        ValueRef::Sym(g) => g,
        _ => orig, // gensym always yields a Sym
    }
}

/// True if `v` is `(unquote …)` / `(unquote-splicing …)` — a computed hole whose
/// binders (`(let ~bindings …)` / `(fn ~params …)`) aren't literal template
/// structure, so we can't rename them.
fn hyg_is_unquote(heap: &Heap, v: Value) -> bool {
    tagged(heap, v, kw::UNQUOTE).is_some() || tagged(heap, v, kw::UNQUOTE_SPLICING).is_some()
}

/// The hygiene pre-pass: alpha-rename a template's introduced binders. A no-op
/// (returning the template unchanged, no allocation) when it introduces none — the
/// common static case. GC-blocked otherwise because it builds a parallel template
/// tree, exactly like `resolve`.
pub(super) fn hygiene_rename(heap: &mut Heap, template: Value) -> Value {
    if !template_introduces_binder(heap, template) {
        return template;
    }
    let _gc = crate::process::GcBlockGuard::enter();
    let _macro = crate::process::MacroBlockGuard::enter();
    hyg_walk(heap, template, &[])
}

fn hyg_walk(heap: &mut Heap, v: Value, scope: &[(value::Symbol, value::Symbol)]) -> Value {
    crate::stack::maybe_grow(64 * 1024, 1024 * 1024, || hyg_walk_inner(heap, v, scope))
}

fn hyg_walk_inner(heap: &mut Heap, v: Value, scope: &[(value::Symbol, value::Symbol)]) -> Value {
    match v.unpack() {
        ValueRef::Sym(s) => match hyg_lookup(scope, s) {
            Some(target) => Value::symbol(target),
            None => v,
        },
        ValueRef::Pair(_) => hyg_list(heap, v, scope),
        ValueRef::Vector(id) => {
            let items = heap.vector(id).to_vec();
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(hyg_walk(heap, it, scope));
            }
            heap.alloc_vector(out)
        }
        ValueRef::Map(id) => {
            let entries = heap.map_entries(id);
            let mut pairs = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                let k = hyg_walk(heap, k, scope);
                let val = hyg_walk(heap, val, scope);
                pairs.push((k, val));
            }
            heap.map_from_pairs(pairs)
        }
        ValueRef::Set(id) => {
            let items = heap.set_elems(id);
            let mut out = Vec::with_capacity(items.len());
            for e in items {
                out.push(hyg_walk(heap, e, scope));
            }
            heap.set_from_elems(out)
        }
        _ => v,
    }
}

fn hyg_list(heap: &mut Heap, form: Value, scope: &[(value::Symbol, value::Symbol)]) -> Value {
    let items = match heap.list_to_vec(form) {
        Ok(i) => i,
        Err(_) => return form, // improper list — leave verbatim
    };
    if let Some(ValueRef::Sym(h)) = items.first().map(|v| v.unpack()) {
        // Data / holes — never descend for renaming: `quote` is data;
        // `unquote`/`unquote-splicing` are caller code resolved in the caller's
        // scope (also where `~'it` — the anaphora opt-out — lands, emitting a
        // literal `it`); a nested `quasiquote` is another template (rejected later).
        if value::symbol_is(h, kw::QUOTE)
            || value::symbol_is(h, kw::UNQUOTE)
            || value::symbol_is(h, kw::UNQUOTE_SPLICING)
            || value::symbol_is(h, kw::QUASIQUOTE)
        {
            return form;
        }
        if value::symbol_is(h, kw::LET) || value::symbol_is(h, kw::LETREC) {
            return hyg_let(heap, form, &items, scope);
        }
        if value::symbol_is(h, kw::FN) {
            return hyg_fn(heap, form, &items, scope);
        }
    }
    // Generic: rename references in every position (head included) under the same
    // scope. A `match*`/`def`/`defn` etc. rides here — a nested binder that matches
    // an enclosing rename stays consistent (shadowing preserved); one with no
    // enclosing rename stays literal (v1 under-approx).
    hyg_generic(heap, form, &items, scope)
}

fn hyg_generic(
    heap: &mut Heap,
    form: Value,
    items: &[Value],
    scope: &[(value::Symbol, value::Symbol)],
) -> Value {
    let mut out = Vec::with_capacity(items.len());
    for &it in items {
        out.push(hyg_walk(heap, it, scope));
    }
    rebuild_list(heap, form, out)
}

/// Add scope entries for a binder position (a `let`/`fn` binder).
fn hyg_bind(heap: &Heap, target: Value, scope: &mut HygScope) {
    match target.unpack() {
        ValueRef::Sym(s) if hyg_renamable(s) => scope.push((s, hyg_fresh(s))),
        ValueRef::Sym(s) => scope.push((s, s)), // `_`/`&rest`/`x#`/qualified — shadow, keep literal
        _ => {
            // A pattern binder (vector/map/list). Shadow every name it binds so an
            // inner reference stays literal, but don't rename it (v1 under-approx).
            let mut names = Vec::new();
            collect_all_syms(heap, target, &mut names);
            for s in names {
                scope.push((s, s));
            }
        }
    }
}

/// The binder position for the output: a renamed symbol for a plain binder, else
/// the pattern verbatim (v1 doesn't rewrite pattern internals).
fn hyg_binder_out(target: Value, scope: &[(value::Symbol, value::Symbol)]) -> Value {
    match target.unpack() {
        ValueRef::Sym(s) => Value::symbol(hyg_lookup(scope, s).unwrap_or(s)),
        _ => target,
    }
}

/// `(let/letrec (b0 v0 …) body…)` — rename plain-symbol binders. `letrec` binders
/// are all in scope for every RHS and the body; plain `let` is sequential (a binder
/// scopes only the *later* RHSs and the body — matching `resolve_let`).
fn hyg_let(
    heap: &mut Heap,
    form: Value,
    items: &[Value],
    scope: &[(value::Symbol, value::Symbol)],
) -> Value {
    let letrec = matches!(items.first().map(|v| v.unpack()),
        Some(ValueRef::Sym(h)) if value::symbol_is(h, kw::LETREC));
    let binds_form = items.get(1).copied().unwrap_or(Value::nil());
    // A computed binding list (`(let ~bindings …)`) — can't see its binders; recurse
    // generically (the `~bindings` hole is left verbatim, the body under `scope`).
    if hyg_is_unquote(heap, binds_form) {
        return hyg_generic(heap, form, items, scope);
    }
    let binds = match heap.list_or_vector_items(binds_form) {
        Some(b) if b.len() % 2 == 0 => b,
        _ => return hyg_generic(heap, form, items, scope),
    };
    let mut new_scope = scope.to_vec();
    if letrec {
        for &t in binds.iter().step_by(2) {
            hyg_bind(heap, t, &mut new_scope);
        }
    }
    let mut new_binds = Vec::with_capacity(binds.len());
    let mut i = 0;
    while i < binds.len() {
        let target = binds[i];
        let rhs = hyg_walk(heap, binds[i + 1], &new_scope);
        if !letrec {
            hyg_bind(heap, target, &mut new_scope); // sequential: bind AFTER the RHS
        }
        new_binds.push(hyg_binder_out(target, &new_scope));
        new_binds.push(rhs);
        i += 2;
    }
    let new_bind_form = rebuild_seq_like(heap, binds_form, new_binds);
    let mut out = Vec::with_capacity(items.len());
    out.push(items[0]);
    out.push(new_bind_form);
    for &b in items.get(2..).unwrap_or(&[]) {
        out.push(hyg_walk(heap, b, &new_scope));
    }
    rebuild_list(heap, form, out)
}

/// `(fn …)` — single-arity `(params body…)` or multi-arity `(doc? (params body…)…)`.
/// Params bind together in their body. Mirrors `resolve_fn`'s dispatch.
fn hyg_fn(
    heap: &mut Heap,
    form: Value,
    items: &[Value],
    scope: &[(value::Symbol, value::Symbol)],
) -> Value {
    let parts = &items[1..];
    let (has_doc, clause_start) = match parts.first().map(|v| v.unpack()) {
        Some(ValueRef::Str(_)) if parts.len() > 1 => (true, 1),
        _ => (false, 0),
    };
    let clauses = &parts[clause_start..];
    let multi = !clauses.is_empty() && clauses.iter().all(|&f| is_arity_clause(heap, f));
    let mut out = Vec::with_capacity(items.len());
    out.push(items[0]); // fn head
    if multi {
        if has_doc {
            out.push(parts[0]);
        }
        for &clause in clauses {
            out.push(hyg_arity_clause(heap, clause, scope));
        }
    } else {
        let params = parts.first().copied().unwrap_or(Value::nil());
        let mut inner = scope.to_vec();
        out.push(hyg_param_list(heap, params, &mut inner));
        for &b in parts.get(1..).unwrap_or(&[]) {
            out.push(hyg_walk(heap, b, &inner));
        }
    }
    rebuild_list(heap, form, out)
}

fn hyg_arity_clause(
    heap: &mut Heap,
    clause: Value,
    scope: &[(value::Symbol, value::Symbol)],
) -> Value {
    let cparts = match heap.list_to_vec(clause) {
        Ok(c) if !c.is_empty() => c,
        _ => return clause,
    };
    let mut inner = scope.to_vec();
    let new_params = hyg_param_list(heap, cparts[0], &mut inner);
    let mut out = Vec::with_capacity(cparts.len());
    out.push(new_params);
    for &b in &cparts[1..] {
        out.push(hyg_walk(heap, b, &inner));
    }
    rebuild_list(heap, clause, out)
}

/// Rename plain-symbol params, extending `inner` with them (all bound together for
/// the body). A computed param list (`~params`) is left verbatim.
fn hyg_param_list(heap: &mut Heap, params: Value, inner: &mut HygScope) -> Value {
    if hyg_is_unquote(heap, params) {
        return params; // `(fn ~params …)` — binders not visible
    }
    let elems = match heap.list_or_vector_items(params) {
        Some(e) => e,
        None => return params,
    };
    let mut out = Vec::with_capacity(elems.len());
    for p in elems {
        hyg_bind(heap, p, inner);
        out.push(hyg_binder_out(p, inner));
    }
    rebuild_seq_like(heap, params, out)
}

/// True if `v` contains a `let`/`letrec`/`fn` that introduces a plain-symbol binder
/// the hygiene pass would rename — so the static-quasiquote optimisation must defer
/// to the runtime expand path (fresh gensyms per expansion). Skips `quote`/`unquote`/
/// `quasiquote` subtrees (a `let` in caller code or quoted data isn't the template's).
pub(super) fn template_introduces_binder(heap: &Heap, v: Value) -> bool {
    crate::stack::maybe_grow(64 * 1024, 1024 * 1024, || {
        template_introduces_binder_inner(heap, v)
    })
}

fn template_introduces_binder_inner(heap: &Heap, v: Value) -> bool {
    match v.unpack() {
        ValueRef::Pair(_) => {
            let items = match heap.list_to_vec(v) {
                Ok(i) => i,
                Err(_) => return false,
            };
            if let Some(ValueRef::Sym(h)) = items.first().map(|x| x.unpack()) {
                if value::symbol_is(h, kw::QUOTE)
                    || value::symbol_is(h, kw::UNQUOTE)
                    || value::symbol_is(h, kw::UNQUOTE_SPLICING)
                    || value::symbol_is(h, kw::QUASIQUOTE)
                {
                    return false;
                }
                if value::symbol_is(h, kw::LET) || value::symbol_is(h, kw::LETREC) {
                    let binds_form = items.get(1).copied().unwrap_or(Value::nil());
                    if !hyg_is_unquote(heap, binds_form)
                        && let Some(binds) = heap.list_or_vector_items(binds_form)
                        && binds.iter().step_by(2).any(hyg_binder_renamable)
                    {
                        return true;
                    }
                }
                if value::symbol_is(h, kw::FN) && fn_has_renamable_param(heap, &items) {
                    return true;
                }
            }
            items.iter().any(|&it| template_introduces_binder(heap, it))
        }
        ValueRef::Vector(id) => heap
            .vector(id)
            .to_vec()
            .iter()
            .any(|&it| template_introduces_binder(heap, it)),
        ValueRef::Map(id) => heap.map_entries(id).iter().any(|(k, val)| {
            template_introduces_binder(heap, *k) || template_introduces_binder(heap, *val)
        }),
        ValueRef::Set(id) => heap
            .set_elems(id)
            .iter()
            .any(|&e| template_introduces_binder(heap, e)),
        _ => false,
    }
}

/// A binder position that hyg would rename: a plain renamable symbol.
fn hyg_binder_renamable(t: &Value) -> bool {
    matches!(t.unpack(), ValueRef::Sym(s) if hyg_renamable(s))
}

/// Does any clause of this `fn` form have a renamable plain-symbol param?
fn fn_has_renamable_param(heap: &Heap, items: &[Value]) -> bool {
    let parts = &items[1..];
    let clause_start = matches!(parts.first().map(|v| v.unpack()), Some(ValueRef::Str(_)) if parts.len() > 1)
        as usize;
    let clauses = &parts[clause_start..];
    if !clauses.is_empty() && clauses.iter().all(|&f| is_arity_clause(heap, f)) {
        clauses.iter().any(|&c| {
            heap.list_to_vec(c)
                .ok()
                .and_then(|cp| cp.first().copied())
                .is_some_and(|params| param_list_has_renamable(heap, params))
        })
    } else {
        parts
            .first()
            .copied()
            .is_some_and(|params| param_list_has_renamable(heap, params))
    }
}

fn param_list_has_renamable(heap: &Heap, params: Value) -> bool {
    if hyg_is_unquote(heap, params) {
        return false;
    }
    heap.list_or_vector_items(params)
        .is_some_and(|elems| elems.iter().any(hyg_binder_renamable))
}
