//! The tree-sitter primitives: parse foreign-language text with a compiled grammar and
//! walk the resulting tree (`std/editor/treesit.blsp` is the policy). The mechanism is
//! `crate::host::treesit`; without the `treesit` feature every call reports so at runtime.

use crate::core::heap::Heap;
use crate::core::value::{self, EnvId, Value};
use crate::error::{LispError, LispResult};

use super::args::{arg, expect_int, expect_string, expect_string_ref};

/// Every primitive this file contributes: name, arity, signature, arglist, docstring.
pub(super) fn register(primitives: &mut super::Primitives) {
    use super::signature_types::*;
    use crate::core::value::Arity;
    use crate::types::Sig;
    // Foreign-language CST via tree-sitter (feature "treesit"), in the SAME node
    // shape as `parse-source-positioned` so std/sexp + the editor modes navigate
    // it unchanged. Always registered; errors if built without the feature. §C.
    primitives.def(
        "%tree-sitter-parse",
        Arity::exact(2),
        Sig::new(vec![string, kw], map_ty),
        &["source", "lang"],
        "Parse source (a string) with the tree-sitter grammar named by keyword lang into a positioned CST — the SAME node-map shape as parse-source-positioned (`{:kind :start :end :named}`; leaves add :text, nodes with children add :kids), :kind a keyword of the tree-sitter node type and :named false for anonymous tokens (keywords/punctuation). Char offsets, so std/sexp + the editor's fontify navigate it unchanged. The generic mechanism is in the default build, but the kernel ships NO language grammar — a grammar is opt-in (e.g. --features treesit-ruby, or treesit-grammars for all). Errors if the named language's grammar isn't built in, or if the runtime was built without --features treesit.",
        tree_sitter_parse);
    // The positional queries. `%tree-sitter-parse` projects the WHOLE tree into Brood
    // maps — 9,561 of them for a 22 KB Elixir file — and every consumer that only wants
    // to know what encloses a point then walks that to read about ten. These answer in
    // O(depth) without building the rest.
    primitives.def(
        "%tree-sitter-chain",
        Arity::exact(3),
        Sig::new(vec![string, kw, int], vec_ty),
        &["source", "lang", "offset"],
        "The nodes containing char `offset`, outermost first, each as {:kind :start :end :named :container} WITHOUT its children. The enclosing-context query an indenter or backward-up-list wants, answered in O(depth) instead of by projecting the whole tree and walking it. Same grammar rules and errors as %tree-sitter-parse.",
        tree_sitter_chain);
    primitives.def(
        "%tree-sitter-kids",
        Arity::exact(3),
        Sig::new(vec![string, kw, int], vec_ty),
        &["source", "lang", "offset"],
        "The named children of the deepest node STRICTLY containing char `offset`, each as {:kind :start :end :named :container} without its own children — the sibling list every structural motion needs (forward-sexp is the next one starting at or after point). Strict containment is what makes a point at a node's start belong to its parent, so forward-sexp steps over the form at point rather than into it. Same grammar rules and errors as %tree-sitter-parse.",
        tree_sitter_kids);
    primitives.def(
        "%tree-sitter-errors",
        Arity::exact(2),
        Sig::new(vec![string, kw], vec_ty),
        &["source", "lang"],
        "Where the parse came apart: the ERROR and MISSING nodes as {:kind :start :end :missing? :text}, outermost first, [] for a clean parse. A MISSING node is a token the grammar expected and inserted (its :kind IS that token, and it is zero-width); an ERROR node spans text the parser could not place. The distinction a live evaluator needs — text someone is still typing must not be reported as a mistake — which %tree-sitter-chain's :broken flag cannot make, since it says only that the tree has an error somewhere. Capped at 32 nodes, each :text at 40 chars. Same grammar rules and errors as %tree-sitter-parse.",
        tree_sitter_errors);
    primitives.def(
        "%tree-sitter-spans",
        Arity::exact(4),
        Sig::new(vec![string, kw, list_ty.union(vec_ty), any], vec_ty),
        &["source", "lang", "kinds", "keywords?"],
        "The fontify query: [start end kind] for the OUTERMOST nodes whose :kind is in `kinds` (a list/vector of keywords), in source order, not descending into one that matched. With keywords? true, an anonymous token beginning with a letter is also reported, as kind :__keyword__ — the cross-language keyword rule, which no grammar names. Which kinds get which face stays with the caller; what this avoids is projecting every node into a map to read its kind and discard the rest, which is what a windowed fontify did on every keystroke.",
        tree_sitter_spans);
    // Tree HANDLES (ADR-443): a whole document's parse tree, held across edits and
    // reparsed incrementally, queried over a range — what an editor's view keeps per
    // buffer instead of re-parsing a window of text on every keystroke.
    primitives.def(
        "%tree-sitter-tree",
        Arity::exact(2),
        Sig::new(vec![rope.union(string), kw], ref_ty),
        &["text", "lang"],
        "Parse a whole document — a rope (read chunk by chunk, never copied) or a string — with the tree-sitter grammar named by keyword lang, and return a tree HANDLE: an opaque ref naming the tree and the text it was parsed from. A handle is immutable: %tree-sitter-tree-reparse returns a new one and leaves this one answering as before. Handles live in a bounded runtime-wide registry (32, least recently used dropped first), so a dropped handle reads as nil from every query and reparsing from it is a full parse; free one you have replaced with %tree-sitter-tree-free. Sendable by value, so a task can parse off the loop and hand the handle back. Same grammar rules and errors as %tree-sitter-parse.",
        tree_sitter_tree);
    primitives.def(
        "%tree-sitter-tree-reparse",
        Arity::exact(4),
        Sig::new(vec![ref_ty, rope.union(string), kw, any], ref_ty),
        &["tree", "text", "lang", "edits"],
        "A NEW tree handle for text, reparsed incrementally from tree's tree (tree-sitter's edit + reparse, ~1 ms where a fresh parse of a large file is hundreds). edits says what changed since tree's text, in CHAR offsets: one [start old-end new-end] (chars [start, old-end) of the old text became [start, new-end) of the new — a splice [lo hi repl] is [lo hi (+ lo (count repl))]), a sequence of them applied in order, or nil to derive the edit by comparing the two texts (common prefix and suffix, chunk by chunk — cheap, and exact for any single contiguous change). An edit whose lengths do not fit the two texts is an error; one that fits them but names the wrong place gives a wrong tree, so pass nil when unsure. A gone handle, another lang, or a grammar reloaded since is a full parse. tree stays valid.",
        tree_sitter_tree_reparse);
    primitives.def(
        "%tree-sitter-tree-spans",
        Arity::exact(5),
        Sig::new(vec![ref_ty, list_ty.union(vec_ty), any, int, int], vec_or_nil),
        &["tree", "kinds", "keywords?", "from", "to"],
        "The fontify query over a held tree, for chars [from, to) of its text only: [start end kind] for the OUTERMOST nodes whose :kind is in kinds that reach into the range, in source order, each clipped to the range — so a string opened above it reports its visible part, coloured as a string, because the tree is the whole document's. Walks only the nodes that reach the range. keywords? as in %tree-sitter-spans (an anonymous token beginning with a letter is :__keyword__). Absolute char offsets. nil when the handle is gone (freed or evicted): open a fresh tree.",
        tree_sitter_tree_spans);
    primitives.def(
        "%tree-sitter-tree-changes",
        Arity::exact(1),
        Sig::new(vec![ref_ty], any),
        &["tree"],
        "Where tree's colouring may differ from the tree it was reparsed from: a sorted vector of disjoint char ranges [start end] of its text — the edited text itself plus tree-sitter's changed ranges (a quote typed above the screen recolours everything after it, and this says so). [] when the text did not change; true for a tree from a fresh parse (everything may differ); nil when the handle is gone. What a span cache invalidates by, instead of throwing every band away on any edit.",
        tree_sitter_tree_changes);
    primitives.def(
        "%tree-sitter-tree-free",
        Arity::exact(1),
        Sig::new(vec![ref_ty], bool_ty),
        &["tree"],
        "Drop a tree handle now, instead of waiting for the registry to evict it; true when it was still held. Every later query on it answers nil.",
        tree_sitter_tree_free);
    primitives.def(
        "%tree-sitter-load-grammar",
        Arity::exact(2),
        Sig::new(vec![string, kw], int),
        &["path", "lang"],
        "Load a tree-sitter grammar from the shared library at `path` and register it under the language keyword `lang`, so tree-sitter-parse can use it like a built-in grammar; returns its ABI version. This is how a language the runtime was not built with gets parsed at all — the kernel deliberately knows no path convention, so the caller names the library and an application keeps its own grammar directory. A grammar loaded this way overrides a bundled one of the same name. Errors (never crashes) on a missing file, a library exporting no tree_sitter_<lang>, or a grammar built for an incompatible tree-sitter ABI; it is native code, though, so load only from a directory the user controls. Feature `treesit`.",
        tree_sitter_load_grammar);
    primitives.def(
        "%tree-sitter-languages",
        Arity::exact(0),
        Sig::new(vec![], vec_ty),
        &[],
        "The languages this runtime can parse right now, as a sorted vector of keywords: every grammar loaded with %tree-sitter-load-grammar plus those built in. What an editor asks before offering to fetch a grammar, or telling you a file's mode has none — instead of parsing something and reading the error. [] in a build without feature `treesit`.",
        tree_sitter_languages);
}

/// `(%tree-sitter-languages)` — the parseable languages; `crate::host::treesit::languages`.
pub(super) fn tree_sitter_languages(_: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    crate::host::treesit::languages(heap)
}

/// `(tree-sitter-parse source lang)` — parse a foreign language into the same
/// positioned-CST node shape as `parse-source-positioned`. Mechanism lives in
/// `crate::host::treesit` (feature-gated); this just unwraps the args. See §C.
pub(super) fn tree_sitter_parse(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let src = expect_string(heap, "%tree-sitter-parse", arg(args, 0))?;
    let lang = match arg(args, 1) {
        Value::Keyword(s) => value::symbol_name(s),
        v => {
            return Err(LispError::wrong_type(
                heap,
                "%tree-sitter-parse",
                "keyword",
                v,
            ));
        }
    };
    crate::host::treesit::parse(heap, &src, &lang)
}

/// `(tree-sitter-chain source lang offset)` / `(tree-sitter-kids source lang offset)` —
/// the two positional queries. Mechanism in `crate::host::treesit`; these unwrap the args.
fn ts_lang(heap: &Heap, who: &'static str, v: Value) -> Result<String, LispError> {
    match v {
        Value::Keyword(s) => Ok(value::symbol_name(s)),
        other => Err(LispError::wrong_type(heap, who, "keyword", other)),
    }
}

pub(super) fn tree_sitter_chain(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let src = expect_string(heap, "%tree-sitter-chain", arg(args, 0))?;
    let lang = ts_lang(heap, "%tree-sitter-chain", arg(args, 1))?;
    let offset = expect_int(heap, "%tree-sitter-chain", arg(args, 2))?;
    crate::host::treesit::chain(heap, &src, &lang, offset)
}

pub(super) fn tree_sitter_kids(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let src = expect_string(heap, "%tree-sitter-kids", arg(args, 0))?;
    let lang = ts_lang(heap, "%tree-sitter-kids", arg(args, 1))?;
    let offset = expect_int(heap, "%tree-sitter-kids", arg(args, 2))?;
    crate::host::treesit::kids(heap, &src, &lang, offset)
}

pub(super) fn tree_sitter_errors(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let src = expect_string(heap, "%tree-sitter-errors", arg(args, 0))?;
    let lang = ts_lang(heap, "%tree-sitter-errors", arg(args, 1))?;
    crate::host::treesit::errors(heap, &src, &lang)
}

pub(super) fn tree_sitter_spans(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let src = expect_string(heap, "%tree-sitter-spans", arg(args, 0))?;
    let lang = ts_lang(heap, "%tree-sitter-spans", arg(args, 1))?;
    let kinds = ts_kinds(heap, "%tree-sitter-spans", arg(args, 2))?;
    let keywords = !matches!(arg(args, 3), Value::Nil | Value::Bool(false));
    crate::host::treesit::spans(heap, &src, &lang, kinds, keywords)
}

/// `(tree-sitter-load-grammar path lang)` — load a grammar shared library at
/// runtime and register it under `lang`. Mechanism in `crate::host::treesit`
/// (feature-gated); this just unwraps the args.
pub(super) fn tree_sitter_load_grammar(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let path = expect_string(heap, "%tree-sitter-load-grammar", arg(args, 0))?;
    let lang = match arg(args, 1) {
        Value::Keyword(s) => value::symbol_name(s),
        v => {
            return Err(LispError::wrong_type(
                heap,
                "%tree-sitter-load-grammar",
                "keyword",
                v,
            ));
        }
    };
    crate::host::treesit::load_grammar(&path, &lang)
}

// ---- tree handles (ADR-443) ----------------------------------------------------------

/// The keyword list a span query names, as strings.
fn ts_kinds(heap: &Heap, who: &'static str, v: Value) -> Result<Vec<String>, LispError> {
    let mut kinds: Vec<String> = Vec::new();
    for k in heap.seq_items(v)? {
        match k {
            Value::Keyword(s) => kinds.push(value::symbol_name(s)),
            other => return Err(LispError::wrong_type(heap, who, "keyword", other)),
        }
    }
    Ok(kinds)
}

/// A document's text as a rope: a rope as itself (a reference count, no copy), a string
/// copied into one.
fn ts_text(heap: &Heap, who: &'static str, v: Value) -> Result<ropey::Rope, LispError> {
    match v {
        Value::Rope(id) => Ok(heap.rope(id).clone()),
        Value::Str(_) => Ok(ropey::Rope::from_str(expect_string_ref(heap, who, v)?)),
        other => Err(LispError::wrong_type(heap, who, "rope or string", other)),
    }
}

/// A tree handle: the `Ref` `%tree-sitter-tree` returned.
fn ts_handle(heap: &Heap, who: &'static str, v: Value) -> Result<u64, LispError> {
    match v {
        Value::Ref(id) => Ok(id),
        other => Err(LispError::wrong_type(heap, who, "tree handle (ref)", other)),
    }
}

/// A non-negative int, as a char offset.
fn ts_offset(heap: &Heap, who: &'static str, v: Value) -> Result<usize, LispError> {
    let n = expect_int(heap, who, v)?;
    usize::try_from(n).map_err(|_| {
        LispError::runtime(format!("{who}: an edit offset is never negative, got {n}"))
    })
}

/// The edits a reparse is told: nil (derive them), one `[start old-end new-end]`, or a
/// sequence of them applied in order.
fn ts_edits(
    heap: &Heap,
    who: &'static str,
    v: Value,
) -> Result<Option<Vec<(usize, usize, usize)>>, LispError> {
    if matches!(v, Value::Nil) {
        return Ok(None);
    }
    let items = heap.seq_items(v)?;
    let triple = |heap: &Heap, item: Value| -> Result<(usize, usize, usize), LispError> {
        let parts = heap.seq_items(item)?;
        if parts.len() != 3 {
            return Err(LispError::runtime(format!(
                "{who}: an edit is [start old-end new-end], got {} items",
                parts.len()
            )));
        }
        Ok((
            ts_offset(heap, who, parts[0])?,
            ts_offset(heap, who, parts[1])?,
            ts_offset(heap, who, parts[2])?,
        ))
    };
    if items
        .first()
        .is_some_and(|first| matches!(first, Value::Int(_)))
    {
        return Ok(Some(vec![triple(heap, v)?]));
    }
    let edits = items
        .into_iter()
        .map(|item| triple(heap, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(edits))
}

pub(super) fn tree_sitter_tree(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let text = ts_text(heap, "%tree-sitter-tree", arg(args, 0))?;
    let lang = ts_lang(heap, "%tree-sitter-tree", arg(args, 1))?;
    Ok(Value::Ref(crate::host::treesit::tree_open(text, &lang)?))
}

pub(super) fn tree_sitter_tree_reparse(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let who = "%tree-sitter-tree-reparse";
    let old = ts_handle(heap, who, arg(args, 0))?;
    let text = ts_text(heap, who, arg(args, 1))?;
    let lang = ts_lang(heap, who, arg(args, 2))?;
    let edits = ts_edits(heap, who, arg(args, 3))?;
    Ok(Value::Ref(crate::host::treesit::tree_reparse(
        old, text, &lang, edits,
    )?))
}

pub(super) fn tree_sitter_tree_spans(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let who = "%tree-sitter-tree-spans";
    let handle = ts_handle(heap, who, arg(args, 0))?;
    let kinds = ts_kinds(heap, who, arg(args, 1))?;
    let keywords = !matches!(arg(args, 2), Value::Nil | Value::Bool(false));
    let from = expect_int(heap, who, arg(args, 3))?;
    let to = expect_int(heap, who, arg(args, 4))?;
    crate::host::treesit::tree_spans(heap, handle, kinds, keywords, from, to)
}

pub(super) fn tree_sitter_tree_changes(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let handle = ts_handle(heap, "%tree-sitter-tree-changes", arg(args, 0))?;
    crate::host::treesit::tree_changes(heap, handle)
}

pub(super) fn tree_sitter_tree_free(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let handle = ts_handle(heap, "%tree-sitter-tree-free", arg(args, 0))?;
    Ok(Value::boolean(crate::host::treesit::tree_free(handle)))
}
