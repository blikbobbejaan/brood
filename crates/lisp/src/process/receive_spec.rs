//! `receive`'s native matcher (ADR-409): a pattern SPEC the `receive` macro emits in place of
//! a matcher closure when every clause is a simple shape, matched here against each scanned
//! message without calling back into Brood.
//!
//! The macro decides what qualifies and builds the spec — this file only interprets it, so the
//! pattern language stays in Brood (`std/prelude/process.blsp`, `%receive-spec`).
//!
//! **A spec is a keyword**, `:%rspec <patterns>`, whose name is the printed pattern data
//! ([`spec_key`], called by the macro at expansion time). A keyword is an immediate value, so
//! a `receive`'s arm embeds no heap constant: an arm that did could not be shared across a
//! runtime's processes (each recompiled it, and none reused its native code — `spawn` read
//! +47% when the spec was a quoted vector). The kernel parses the name once per thread into
//! [`Pat`]s and caches them by symbol. A `receive` that pins values passes
//! `[key pin0 pin1 …]` instead, the pins read at the call site.
//!
//! `patterns` has one pattern per clause, in clause order; a pattern is one of
//!
//! | spec        | matches                                     | binds        |
//! |-------------|---------------------------------------------|--------------|
//! | `[:any]`    | anything (`_`)                              | nothing      |
//! | `[:bind]`   | anything (a fresh symbol)                   | the value    |
//! | `[:same k]` | a value `%eq` to binding `k` (a repeat)     | nothing      |
//! | `[:eq v]`   | a value `%eq` to literal `v` (or `'sym`)    | nothing      |
//! | `[:pin i]`  | a value `%eq` to pinned value `i`           | nothing      |
//! | `[:vec p…]` | a vector of exactly that many elements      | what its elements bind |
//!
//! and the answer for the first clause that matches is the `[idx var…]` vector the matcher
//! closure would have returned — the clause's index, then its bindings in first-occurrence
//! order — so the call-site dispatch the macro emits is the same either way.
//!
//! Equality is `Heap::equal`, which is `%eq`'s, with one difference `%eq` handles and this
//! cannot: a lazy seq-view is realised before comparing. A message never carries one (`send`
//! refuses them), so only a pinned value can, and [`realise_spec_views`] realises those once,
//! at the `receive`'s entry.

use crate::core::heap::Heap;
use crate::core::heap::SymbolMap;
use crate::core::value::{self, Symbol, Value, ValueRef, VecId};
use crate::error::{LispError, LispResult};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::rc::Rc;

/// The keyword-name prefix that marks a spec.
const PREFIX: &str = "%rspec ";

/// One clause's pattern, parsed out of a spec key.
enum Pat {
    Any,
    Bind,
    Same(usize),
    Eq(Lit),
    Pin(usize),
    Vec(Box<[Pat]>),
}

/// A literal a pattern compares against, held without a heap so one parse serves every
/// process on the thread.
enum Lit {
    /// An immediate — int, float, keyword, symbol, nil, bool: compared as itself.
    Imm(Value),
    Str(Box<str>),
}

thread_local! {
    static CACHE: RefCell<SymbolMap<Option<Rc<[Pat]>>>> = RefCell::new(SymbolMap::default());
}

/// A `receive`'s spec, resolved once at its entry ([`resolve`]): the clause patterns, and
/// whether the matcher value carries pins (`[key pin0 …]`). The pins themselves are read off
/// the rooted matcher value per candidate — a collection can move that vector.
pub(crate) struct Spec {
    pats: Rc<[Pat]>,
    pinned: bool,
}

/// `%receive-spec-key`: the keyword naming `patterns` — for the `receive` macro, at expansion.
pub(crate) fn spec_key(heap: &Heap, patterns: Value) -> Value {
    let name = format!("{PREFIX}{}", crate::syntax::printer::print(heap, patterns));
    Value::Keyword(value::intern(&name))
}

/// The spec `matcher` stands for, or `None` when it is a matcher closure. One cache lookup —
/// a `receive` makes it once, at its entry.
pub(crate) fn resolve(heap: &mut Heap, matcher: Value) -> Option<Spec> {
    let (key, pinned) = match matcher {
        Value::Keyword(key) => (key, false),
        Value::Vector(id) => match heap.vector(id).first().copied() {
            Some(Value::Keyword(key)) => (key, true),
            _ => return None,
        },
        _ => return None,
    };
    let cached = CACHE.with(|c| c.borrow().get(&key).cloned());
    let pats = match cached {
        Some(p) => p,
        None => {
            let parsed = parse_key(heap, key);
            CACHE.with(|c| c.borrow_mut().insert(key, parsed.clone()));
            parsed
        }
    }?;
    Some(Spec { pats, pinned })
}

fn parse_key(heap: &mut Heap, key: Symbol) -> Option<Rc<[Pat]>> {
    let name = value::symbol_name(key);
    let text = name.strip_prefix(PREFIX)?;
    // The reader allocates the pattern data in this heap; it is converted at once and not
    // kept, so nothing here outlives a collection.
    let data = crate::syntax::reader::read_one_complete(heap, text).ok()?;
    let Value::Vector(clauses) = data else {
        return None;
    };
    let items = heap.vector(clauses).to_vec();
    let pats: Option<Vec<Pat>> = items.into_iter().map(|p| parse_pat(heap, p)).collect();
    pats.map(Rc::from)
}

fn parse_pat(heap: &Heap, p: Value) -> Option<Pat> {
    let Value::Vector(id) = p else {
        return None;
    };
    let parts = heap.vector(id);
    let Some(Value::Keyword(op)) = parts.first().copied() else {
        return None;
    };
    let index = |v: Option<&Value>| match v.map(|v| v.unpack()) {
        Some(ValueRef::Int(i)) if i >= 0 => Some(i as usize),
        _ => None,
    };
    Some(match value::symbol_name(op).as_str() {
        "any" => Pat::Any,
        "bind" => Pat::Bind,
        "same" => Pat::Same(index(parts.get(1))?),
        "pin" => Pat::Pin(index(parts.get(1))?),
        "eq" => Pat::Eq(match parts.get(1).copied()? {
            v @ (Value::Int(_)
            | Value::Float(_)
            | Value::Keyword(_)
            | Value::Sym(_)
            | Value::Nil
            | Value::Bool(_)) => Lit::Imm(v),
            Value::Str(s) => Lit::Str(heap.string(s).into()),
            _ => return None,
        }),
        "vec" => Pat::Vec(
            parts[1..]
                .iter()
                .map(|&q| parse_pat(heap, q))
                .collect::<Option<Vec<_>>>()?
                .into_boxed_slice(),
        ),
        _ => return None,
    })
}

/// The first clause of `spec` that `msg` matches, as its `[idx var…]` answer, or nil.
/// `matcher` is the receive's matcher value as it is NOW (re-read from its root), for the pins.
pub(crate) fn spec_answer(heap: &mut Heap, spec: &Spec, matcher: Value, msg: Value) -> Value {
    let pins = match matcher {
        Value::Vector(id) if spec.pinned => Some(id),
        _ => None,
    };
    let mut binds: SmallVec<[Value; 6]> = SmallVec::new();
    for (idx, pat) in spec.pats.iter().enumerate() {
        binds.clear();
        if matches(heap, pins, pat, msg, &mut binds) {
            let mut answer = Vec::with_capacity(binds.len() + 1);
            answer.push(Value::int(idx as i64));
            answer.extend_from_slice(&binds);
            // Nothing between the match and this allocation collects; the bound values are
            // reachable from `msg`, which the caller holds rooted.
            return heap.alloc_vector(answer);
        }
    }
    Value::nil()
}

fn matches(
    heap: &Heap,
    pins: Option<VecId>,
    pat: &Pat,
    v: Value,
    binds: &mut SmallVec<[Value; 6]>,
) -> bool {
    match pat {
        Pat::Any => true,
        Pat::Bind => {
            binds.push(v);
            true
        }
        Pat::Same(k) => binds.get(*k).is_some_and(|&earlier| heap.equal(earlier, v)),
        Pat::Eq(Lit::Imm(want)) => heap.equal(*want, v),
        Pat::Eq(Lit::Str(want)) => matches!(v, Value::Str(s) if heap.string(s) == &**want),
        Pat::Pin(i) => pins
            .and_then(|p| heap.vector(p).get(1 + i).copied())
            .is_some_and(|want| heap.equal(want, v)),
        Pat::Vec(elems) => {
            let Value::Vector(target) = v else {
                return false;
            };
            if heap.vector(target).len() != elems.len() {
                return false;
            }
            elems.iter().enumerate().all(|(i, sub)| {
                let el = heap.vector(target)[i];
                matches(heap, pins, sub, el, binds)
            })
        }
    }
}

/// The spec with every pinned seq-view realised, so the scan's comparisons need no evaluator —
/// `%eq` realises a view before comparing, and so must this. Only a pinned value can be one,
/// and a spec with none — every spec in practice — comes back unchanged and unallocated.
pub(crate) fn realise_spec_views(heap: &mut Heap, spec: Value) -> LispResult {
    let Value::Vector(sid) = spec else {
        return Ok(spec);
    };
    if !heap
        .vector(sid)
        .iter()
        .any(|e| matches!(e, Value::SeqView(_)))
    {
        return Ok(spec);
    }
    // Each view realised in turn; a realise can run code and collect, so the spec and the
    // elements built so far are rooted across it.
    heap.root_scope(|heap| {
        let src = heap.root(spec);
        let n = heap.vector(sid).len();
        let mut done: Vec<crate::core::heap::Root> = Vec::with_capacity(n);
        for i in 0..n {
            let Value::Vector(cur) = heap.read_root(src) else {
                return Err(LispError::type_err("receive: a pattern spec is a vector"));
            };
            let e = heap.vector(cur)[i];
            let r = if matches!(e, Value::SeqView(_)) {
                crate::builtins::realize_seqview(heap, crate::core::value::EnvId::GLOBAL, e)?
            } else {
                e
            };
            done.push(heap.root(r));
        }
        let elems = done.iter().map(|&r| heap.read_root(r)).collect();
        Ok(heap.alloc_vector(elems))
    })
}
