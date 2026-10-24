//! Tree-sitter parsing for *foreign* languages (feature `treesit`) — ROADMAP §C.
//!
//! Brood parses its own `.blsp` with the reader (`parse-source-positioned` in
//! `builtins.rs` projects a positioned CST). For Ruby, Elixir, … there is no
//! Brood reader, so this module wraps tree-sitter — the incremental,
//! error-tolerant parser — behind a single builtin, `tree-sitter-parse`.
//!
//! The point is *shape parity*: it converts a tree-sitter tree into the **same
//! positioned node maps** the Brood CST gives — `{:kind :start :end :named}` for
//! leaves (plus `:text`, the raw source), and additionally `:kids` (a vector of
//! child maps) for any node with children. `:start`/`:end` are half-open
//! CHARACTER offsets (tree-sitter counts bytes; we project them, exactly as
//! `parse-source-positioned`), so `std/tool/sexp`'s structural navigation and the
//! editor's `:fontify` service run over a foreign tree **unchanged**. `:named`
//! distinguishes grammar nodes from anonymous tokens (keywords/punctuation like
//! `def`/`end`/`(`), which a fontifier wants and a navigator filters out.
//!
//! Mechanism only: parse + project. All policy (which node kinds get which face,
//! how to navigate) lives in Brood (`std/editor/treesit.blsp` + the modes). Add a
//! language = add a grammar crate in `Cargo.toml` + one arm in `language_for`.
//!
//! Like the `gui` backend, the builtin is always registered; without the feature
//! it returns a runtime error telling you to rebuild with `--features treesit`.

#[cfg(feature = "treesit")]
use crate::core::value::Value;
use crate::error::{LispError, LispResult};

#[cfg(feature = "treesit")]
use crate::core::heap::Heap;
#[cfg(feature = "treesit")]
use crate::core::value;
#[cfg(feature = "treesit")]
use std::collections::HashMap;
#[cfg(feature = "treesit")]
use std::sync::{LazyLock, Mutex};

/// `(tree-sitter-parse source lang)` — parse `source` (a string) with the
/// grammar named by keyword `lang` (`:ruby`, `:elixir`) into a positioned CST: a
/// `{:kind :start :end :named :kids/:text}` node map (see the module docs). Errors
/// on an unknown language, or when the runtime wasn't built `--features treesit`.
#[cfg(feature = "treesit")]
pub fn parse(heap: &mut Heap, src: &str, lang: &str) -> LispResult {
    let tree = parse_cached(src, lang)?;
    let b2c = byte_to_char_offsets(src);
    Ok(node_to_positioned(heap, tree.root_node(), src, &b2c))
}

/// The last few parses, keyed by (language, source).
///
/// An editor asks the SAME text several questions in a row — fontify while redrawing,
/// then the enclosing chain when you press RET, then a sibling list when you press
/// C-M-f — and each one was re-parsing from scratch. Parsing 22 KB of Elixir is ~10 ms,
/// which is the whole cost of a structural motion once the projection is gone, so
/// repeated motion over an unchanged buffer went from paying it every time to paying it
/// once.
///
/// Small and exact: four entries, and a hit requires the source to be EQUAL, not merely
/// hashed the same. A miss is only ever a re-parse, so the cache can never be wrong —
/// but a false hit would hand back a tree for different text, which would be.
#[cfg(feature = "treesit")]
static TREE_CACHE: LazyLock<Mutex<Vec<(String, String, tree_sitter::Tree)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[cfg(feature = "treesit")]
const TREE_CACHE_CAP: usize = 8;

/// The edit that turns `old` into `new`, derived from their common prefix and suffix.
///
/// tree-sitter reparses INCREMENTALLY, but only if it is told what changed — and the
/// editor never tells us: it hands over a whole buffer, because that is what a pure
/// `text -> spans` service is. The change is recoverable anyway. Bytes before the first
/// difference and after the last are identical, so the edit is exactly "the span between
/// them was replaced". That is precise for any single contiguous change — a keystroke, a
/// paste, a kill — and still CORRECT for a scattered one, merely wider than necessary,
/// since a larger replaced range only makes tree-sitter reparse more.
///
/// `None` when the texts are equal, or when the derived edit spans most of a large
/// document, where reparsing from the old tree buys nothing over a fresh parse.
#[cfg(feature = "treesit")]
fn derive_edit(old: &str, new: &str) -> Option<tree_sitter::InputEdit> {
    let ob = old.as_bytes();
    let nb = new.as_bytes();
    let mut start = 0usize;
    let max_pre = ob.len().min(nb.len());
    while start < max_pre && ob[start] == nb[start] {
        start += 1;
    }
    let mut back = 0usize;
    while back < max_pre - start && ob[ob.len() - 1 - back] == nb[nb.len() - 1 - back] {
        back += 1;
    }
    let old_end = ob.len() - back;
    let new_end = nb.len() - back;
    if start == old_end && start == new_end {
        return None; // identical
    }
    if (old_end - start) * 4 > ob.len() && ob.len() > 4096 {
        return None; // a change spanning most of it is a new document, not an edit
    }
    // Byte offsets only. The row/column `Point`s matter to a client that renders from
    // the tree's positions; the parse itself is byte-addressed, and zeros are consistent.
    let zero = tree_sitter::Point::new(0, 0);
    Some(tree_sitter::InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: zero,
        old_end_position: zero,
        new_end_position: zero,
    })
}

/// Parse `src` with `lang`, reusing the cached tree when the text is unchanged and
/// reparsing incrementally from it when the text merely changed.
#[cfg(feature = "treesit")]
fn parse_cached(src: &str, lang: &str) -> Result<tree_sitter::Tree, LispError> {
    // An exact hit is free; a NEAR hit — the same buffer one keystroke later — is the
    // common case while typing, and is what decides how a large file feels. A full parse
    // of 250 KB of Elixir is ~500 ms, so paying it per edit is the whole of "the editor
    // is sluggish"; reparsing from the previous tree is ~1 ms.
    let reuse: Option<(usize, tree_sitter::Tree)> = {
        let mut cache = crate::core::sync::lock(&TREE_CACHE);
        if let Some(i) = cache
            .iter()
            .position(|(l, s, _)| l == lang && s.as_str() == src)
        {
            // move to the back: most-recently-used, so the eviction below drops the
            // buffer nobody is working in
            let hit = cache.remove(i);
            let tree = hit.2.clone();
            cache.push(hit);
            return Ok(tree);
        }
        // No exact match: the entry this text is an EDIT of — the same document one
        // keystroke earlier. Searched over every entry of the language, most recent first,
        // not just the latest one: an editor parses the same document in more than one
        // shape (the view colours the visible window, the indenter asks about the whole
        // file), and with only the latest entry consulted each shape evicted the other,
        // so every keystroke was a full reparse (18 ms a RET in a 1,300-line Elixir file).
        // The CLOSEST such entry, not the first: a small text (the view's window) is "an edit"
        // of anything by `derive_edit`'s measure, so taking the first match let a whole-document
        // parse claim — and then replace — the window's entry. The edit touching the fewest
        // bytes is the document's own previous version.
        cache
            .iter()
            .enumerate()
            .filter(|(_, (l, _, _))| l == lang)
            .filter_map(|(i, (_, old_src, old_tree))| {
                derive_edit(old_src, src).map(|edit| (i, old_tree, edit))
            })
            .min_by_key(|(_, _, edit)| {
                (edit.old_end_byte - edit.start_byte) + (edit.new_end_byte - edit.start_byte)
            })
            .map(|(i, old_tree, edit)| {
                let mut t = old_tree.clone();
                t.edit(&edit);
                (i, t)
            })
    };
    let language = language_for(lang)?;
    let mut parser = checkout_parser(lang, &language)?;
    let tree = parser.parse(src, reuse.as_ref().map(|(_, t)| t));
    return_parser(lang, parser);
    let tree =
        tree.ok_or_else(|| LispError::runtime(format!("tree-sitter: {lang}: parse failed")))?;
    {
        let mut cache = crate::core::sync::lock(&TREE_CACHE);
        // The entry this was an edit of is the same document's older version: it can never
        // be asked for again, so the new one takes its place — one entry per document (and
        // per window of one), not a generation per keystroke. Another thread may have reshuffled
        // the cache since, so the slot can hold another document now: dropping it costs at
        // most a reparse later, never a wrong tree — a hit always needs the EXACT text.
        let replaced = reuse.and_then(|(i, _)| {
            cache
                .get(i)
                .map(|(l, _, _)| l == lang)
                .filter(|same| *same)
                .map(|_| i)
        });
        if let Some(i) = replaced {
            cache.remove(i);
        } else if cache.len() >= TREE_CACHE_CAP {
            cache.remove(0);
        }
        cache.push((lang.to_string(), src.to_string(), tree.clone()));
    }
    Ok(tree)
}

/// One node, WITHOUT its children — `{:kind :start :end :named :container}`.
///
/// `:container` replaces the presence of `:kids`, which is how the full projection says
/// "this node has children"; the point of these queries is not to build them.
#[cfg(feature = "treesit")]
fn node_shallow(heap: &mut Heap, node: tree_sitter::Node, b2c: &[u32]) -> Value {
    let kw = |k: &str| Value::keyword(value::intern(k));
    heap.map_from_pairs(vec![
        (kw("kind"), kw(node.kind())),
        (kw("start"), Value::int(b2c[node.start_byte()] as i64)),
        (kw("end"), Value::int(b2c[node.end_byte()] as i64)),
        (kw("named"), Value::boolean(node.is_named())),
        (kw("container"), Value::boolean(node.child_count() > 0)),
    ])
}

/// The byte offset for a char offset — the inverse of `byte_to_char_offsets`, by binary
/// search over it, so a caller's char position can address the byte-indexed tree.
#[cfg(feature = "treesit")]
fn char_to_byte(b2c: &[u32], ch: u32) -> usize {
    match b2c.binary_search(&ch) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

/// `(tree-sitter-chain source lang offset)` — the nodes containing char `offset`,
/// outermost first, each without its children.
///
/// **Why this exists rather than walking `tree-sitter-parse`'s result.** That call
/// projects the WHOLE tree into Brood maps: 9,561 of them for a 22 KB Elixir file. Every
/// consumer that wants "what encloses this point" — an indenter, `backward-up-list`,
/// a which-function display — then walks that to read about ten of them, and pays ~17 ms
/// to do it. The editor calls it on every RET, twice, and it was a quarter of a second of
/// every keypress. The tree already knows the answer in O(depth); this hands it over.
#[cfg(feature = "treesit")]
pub fn chain(heap: &mut Heap, src: &str, lang: &str, offset: i64) -> LispResult {
    let tree = parse_cached(src, lang)?;
    let b2c = byte_to_char_offsets(src);
    let byte = char_to_byte(&b2c, offset.max(0) as u32);
    let mut out: Vec<Value> = Vec::new();
    let mut node = tree.root_node();
    let broken = node.has_error();
    loop {
        // `:broken` on the outermost entry: does the tree contain an error anywhere?
        // O(1) from tree-sitter, and the one thing a caller cannot work out from a
        // chain — which only ever sees one path.
        let n = match (
            node.parent().is_none() && broken,
            node_shallow(heap, node, &b2c),
        ) {
            (true, Value::Map(id)) => heap.map_assoc(
                id,
                Value::keyword(value::intern("broken")),
                Value::boolean(true),
            ),
            (_, v) => v,
        };
        out.push(n);
        // The deepest child strictly containing the byte. `named_child` is not enough:
        // an anonymous token can be the one that contains it, and the chain of KINDS is
        // what a caller counts, so the walk follows every child.
        let mut cursor = node.walk();
        let next = node
            .children(&mut cursor)
            .find(|c| c.start_byte() <= byte && byte < c.end_byte());
        match next {
            Some(c) => node = c,
            None => break,
        }
    }
    Ok(heap.alloc_vector(out))
}

/// `(tree-sitter-kids source lang offset)` — the named children of the deepest node
/// STRICTLY containing char `offset`, each without its own children.
///
/// The sibling list every structural motion needs: `forward-sexp` is "the next one of
/// these starting at or after point", `down-list` is "the first child of the next one
/// that is a container". Same reason as [`chain`]: those motions were each projecting
/// the whole tree to read one list.
#[cfg(feature = "treesit")]
pub fn kids(heap: &mut Heap, src: &str, lang: &str, offset: i64) -> LispResult {
    let tree = parse_cached(src, lang)?;
    let b2c = byte_to_char_offsets(src);
    let byte = char_to_byte(&b2c, offset.max(0) as u32);
    // STRICTLY containing, so a point sitting exactly at a node's start belongs to that
    // node's parent — which is what makes `forward-sexp` step OVER the form at point
    // rather than into it.
    let mut node = tree.root_node();
    loop {
        let mut cursor = node.walk();
        let next = node
            .children(&mut cursor)
            .find(|c| c.child_count() > 0 && c.start_byte() < byte && byte < c.end_byte());
        match next {
            Some(c) => node = c,
            None => break,
        }
    }
    let mut cursor = node.walk();
    let out: Vec<tree_sitter::Node> = node.named_children(&mut cursor).collect();
    let vals: Vec<Value> = out
        .into_iter()
        .map(|c| node_shallow(heap, c, &b2c))
        .collect();
    Ok(heap.alloc_vector(vals))
}

/// The most error nodes one call reports. A buffer someone is halfway through
/// rewriting can hold hundreds; a caller only ever reads the first few, and the
/// point of this query is to be cheap enough to run on a keystroke.
#[cfg(feature = "treesit")]
const ERROR_CAP: usize = 32;

/// The most source characters an error node carries back as `:text` — enough to name
/// what is unclosed (`do`, `fn`, `"`) without putting a runaway region on the wire.
#[cfg(feature = "treesit")]
const ERROR_TEXT_CAP: usize = 40;

/// Collect the ERROR and MISSING nodes under `node`, outermost first.
#[cfg(feature = "treesit")]
fn errors_into(
    heap: &mut Heap,
    node: tree_sitter::Node,
    src: &str,
    b2c: &[u32],
    out: &mut Vec<Value>,
) {
    if out.len() >= ERROR_CAP {
        return;
    }
    if node.is_error() || node.is_missing() {
        let kw = |k: &str| Value::keyword(value::intern(k));
        // A MISSING node is zero-width and its kind IS the token the grammar wanted,
        // so its text is empty and the kind carries the meaning. An ERROR node spans
        // the text the parser could not place, and there the text is the meaning.
        let text: String = src[node.start_byte()..node.end_byte()]
            .chars()
            .take(ERROR_TEXT_CAP)
            .collect();
        let s = heap.alloc_string(&text);
        let v = heap.map_from_pairs(vec![
            (kw("kind"), kw(node.kind())),
            (kw("start"), Value::int(b2c[node.start_byte()] as i64)),
            (kw("end"), Value::int(b2c[node.end_byte()] as i64)),
            (kw("missing?"), Value::boolean(node.is_missing())),
            (kw("text"), s),
        ]);
        out.push(v);
        // Do not descend into an ERROR: its children are the tokens the parser
        // salvaged out of the unparsable region, and reporting them would bury the
        // one fact the caller wants — where the parse came apart — under its debris.
        return;
    }
    // Only subtrees that contain an error can contain an error node, and `has_error`
    // is O(1) from the tree. That is what keeps this a walk of the broken part rather
    // than of the buffer.
    let mut cursor = node.walk();
    let children: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
    for c in children {
        if c.has_error() || c.is_missing() {
            errors_into(heap, c, src, b2c, out);
        }
    }
}

/// `(tree-sitter-errors source lang)` — where the parse came apart: the ERROR and
/// MISSING nodes as `{:kind :start :end :missing? :text}`, outermost first.
///
/// **Why the kernel has to answer this.** `chain` already reports `:broken` on the root,
/// which says only *that* the tree has an error somewhere. That is not enough to tell a
/// person typing from a person who is wrong, and the difference is the whole contract of
/// a live evaluator: a form you are still writing must not be shown as a mistake. The
/// tree knows — a MISSING node is a token the grammar expected and inserted for you, an
/// ERROR node is text it could not place, and where each one SITS says the rest (an
/// error running to the end of the buffer is an unclosed construct; one with good code
/// after it is a real mistake). None of that is reachable from Brood today, and building
/// it out of `%tree-sitter-parse` means projecting every node in the file to find the
/// handful that are broken.
///
/// An empty vector means the parse is clean, which is the common answer and costs one
/// `has_error` on the root.
#[cfg(feature = "treesit")]
pub fn errors(heap: &mut Heap, src: &str, lang: &str) -> LispResult {
    let tree = parse_cached(src, lang)?;
    let root = tree.root_node();
    if !root.has_error() && !root.is_missing() {
        return Ok(heap.alloc_vector(Vec::new()));
    }
    let b2c = byte_to_char_offsets(src);
    let mut out: Vec<Value> = Vec::new();
    errors_into(heap, root, src, &b2c, &mut out);
    Ok(heap.alloc_vector(out))
}

/// The text a span walk reads positions and keyword letters from: a flat string with its
/// byte→char table (the slice-based `spans`), or a rope (a tree handle's text). The walk
/// itself is the same either way, so it is written once over this.
#[cfg(feature = "treesit")]
trait SpanText {
    /// The char offset of byte `byte`.
    fn char_of(&self, byte: usize) -> usize;
    /// Does the text in `[start, end)` begin with a letter? (the keyword rule)
    fn starts_alphabetic(&self, start: usize, end: usize) -> bool;
}

#[cfg(feature = "treesit")]
struct FlatText<'a> {
    src: &'a str,
    b2c: &'a [u32],
}

#[cfg(feature = "treesit")]
impl SpanText for FlatText<'_> {
    fn char_of(&self, byte: usize) -> usize {
        self.b2c[byte] as usize
    }
    fn starts_alphabetic(&self, start: usize, end: usize) -> bool {
        self.src[start..end]
            .chars()
            .next()
            .is_some_and(char::is_alphabetic)
    }
}

#[cfg(feature = "treesit")]
impl SpanText for ropey::Rope {
    fn char_of(&self, byte: usize) -> usize {
        self.byte_to_char(byte.min(self.len_bytes()))
    }
    fn starts_alphabetic(&self, start: usize, end: usize) -> bool {
        start < end
            && start < self.len_bytes()
            && self.char(self.byte_to_char(start)).is_alphabetic()
    }
}

/// What one span walk is asked for: the kinds to colour, the keyword flag, and the byte
/// window `[from, to)` to report — `None` for the whole tree, unclipped.
#[cfg(feature = "treesit")]
struct SpanQuery<'a> {
    wanted: &'a std::collections::HashSet<String>,
    keywords: bool,
    window: Option<(usize, usize)>,
}

#[cfg(feature = "treesit")]
impl SpanQuery<'_> {
    /// Does `node` reach into the window? Zero-width nodes count when they sit inside it.
    fn touches(&self, node: &tree_sitter::Node) -> bool {
        match self.window {
            None => true,
            Some((from, to)) => {
                let (start, end) = (node.start_byte(), node.end_byte());
                (start < to && end > from) || (start == end && from <= start && start < to)
            }
        }
    }
}

/// Collect `[start end kind]` for the OUTERMOST nodes whose kind is wanted, in source
/// order, descending only into children that reach the query's window. A span is clipped
/// to the window, so a string opened above it reports only its visible part.
#[cfg(feature = "treesit")]
fn spans_into<T: SpanText>(
    heap: &mut Heap,
    node: tree_sitter::Node,
    text: &T,
    query: &SpanQuery,
    out: &mut Vec<Value>,
) {
    let kind = node.kind();
    // An anonymous token whose text starts with a letter is a language keyword — the
    // cross-language rule, so a table need only name the handful of NAMED nodes it
    // colours. Reported under its own kind so the caller maps it like any other.
    let wanted = query.wanted.contains(kind);
    let is_keyword = !wanted
        && query.keywords
        && !node.is_named()
        && text.starts_alphabetic(node.start_byte(), node.end_byte());
    if wanted || is_keyword {
        let reported = if wanted { kind } else { "__keyword__" };
        let (start, end) = match query.window {
            None => (node.start_byte(), node.end_byte()),
            Some((from, to)) => (
                node.start_byte().max(from),
                node.end_byte().min(to).max(node.start_byte().max(from)),
            ),
        };
        let span = vec![
            Value::int(text.char_of(start) as i64),
            Value::int(text.char_of(end) as i64),
            Value::keyword(value::intern(reported)),
        ];
        let v = heap.alloc_vector(span);
        out.push(v);
        return; // coloured whole: do not descend
    }
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let child = cursor.node();
        if let Some((_, to)) = query.window
            && child.start_byte() >= to
        {
            break; // children are in source order: nothing after this reaches the window
        }
        if query.touches(&child) {
            spans_into(heap, child, text, query, out);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// `(tree-sitter-spans source lang kinds keywords?)` — the fontify query: `[start end
/// kind]` for the outermost nodes whose kind is in `kinds`, in source order.
///
/// **Why the kernel takes the kind set.** Fontify is the one consumer that genuinely
/// walks the WHOLE tree, so it was the one paying the full projection — thousands of
/// Brood maps built per keystroke to read `:kind` off each and throw the rest away. The
/// set is data the caller supplies and the answer is spans, so which kinds get which
/// face — the actual policy — stays entirely in the editor; what moves here is only the
/// walk, which is where the allocation was.
///
/// `keywords?` adds the cross-language rule that an anonymous token beginning with a
/// letter is a keyword, reported as kind `:__keyword__`. It is a flag rather than a kind
/// because no grammar names it: keywords surface as anonymous tokens of every kind.
#[cfg(feature = "treesit")]
pub fn spans(
    heap: &mut Heap,
    src: &str,
    lang: &str,
    kinds: Vec<String>,
    keywords: bool,
) -> LispResult {
    let tree = parse_cached(src, lang)?;
    let b2c = byte_to_char_offsets(src);
    let wanted: std::collections::HashSet<String> = kinds.into_iter().collect();
    let query = SpanQuery {
        wanted: &wanted,
        keywords,
        window: None,
    };
    let mut out: Vec<Value> = Vec::new();
    let text = FlatText { src, b2c: &b2c };
    spans_into(heap, tree.root_node(), &text, &query, &mut out);
    Ok(heap.alloc_vector(out))
}

// ---- tree handles: a parse tree a caller keeps, edits and queries (ADR-433) ----------
//
// The slice-based queries above take TEXT and find the tree themselves, which is the
// right shape for a one-off question and the wrong one for an editor's view: the text has
// to be copied out of the buffer on every keystroke, the edit has to be rediscovered by
// diffing it against whatever was parsed last, and a view that colours a window parses
// the window — a slice with no context above it, so a string opened off screen is code.
//
// A handle is the tree of a WHOLE document, held across edits. It is a `Ref`: opaque,
// compared by identity, sendable by value (the registry is runtime-wide, so a task can
// parse off the loop and hand the editor the result). Each handle is IMMUTABLE — a reparse
// returns a NEW handle and leaves the old one answering exactly as before, so a model that
// keeps one beside a rope can never see it change underneath it. What a handle costs is a
// registry slot, and the registry is bounded: past `TREE_HANDLE_CAP` the least recently
// used is dropped. A dropped handle is not an error to reparse from (that is a full parse)
// and reads as `nil` from every query — the caller opens a fresh tree. `%tree-sitter-tree-
// free` drops one eagerly; a caller that replaces its tree per edit should free the old one,
// and successive versions share almost all of their subtrees, so the ones it forgets cost
// little until the cap reclaims them.

/// The most tree handles kept at once. Each is one document version; consecutive versions
/// of one document share their unchanged subtrees, so the cost is mostly distinct documents.
#[cfg(feature = "treesit")]
const TREE_HANDLE_CAP: usize = 32;

/// Bumped by every `%tree-sitter-load-grammar`. A tree parsed under an older grammar is
/// never handed to the new grammar's parser as the tree to reparse from: that is not stale
/// but ill-formed, and fails the parse outright (see `load_grammar`).
#[cfg(feature = "treesit")]
static GRAMMAR_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "treesit")]
fn grammar_epoch() -> u64 {
    GRAMMAR_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

/// One handle's document: the tree, the text it was parsed from (a rope clone — ropey
/// shares structure, so this is a reference count, not a copy), and what changed since the
/// tree it was reparsed from.
#[cfg(feature = "treesit")]
#[derive(Clone)]
struct TreeEntry {
    tree: tree_sitter::Tree,
    text: ropey::Rope,
    lang: String,
    epoch: u64,
    /// Byte ranges of THIS text whose highlighting may differ from the previous version's:
    /// the edited text plus tree-sitter's `changed_ranges`. `None` for a fresh parse.
    changed: Option<Vec<(usize, usize)>>,
    last_used: u64,
}

#[cfg(feature = "treesit")]
#[derive(Default)]
struct TreeHandles {
    entries: HashMap<u64, TreeEntry>,
    clock: u64,
}

#[cfg(feature = "treesit")]
static TREE_HANDLES: LazyLock<Mutex<TreeHandles>> =
    LazyLock::new(|| Mutex::new(TreeHandles::default()));

/// A clone of handle `id`'s entry (cheap: a tree refcount and a rope refcount), marking it
/// used; `None` when the handle was freed or evicted.
#[cfg(feature = "treesit")]
fn tree_entry(id: u64) -> Option<TreeEntry> {
    let mut handles = crate::core::sync::lock(&TREE_HANDLES);
    handles.clock += 1;
    let now = handles.clock;
    handles.entries.get_mut(&id).map(|entry| {
        entry.last_used = now;
        entry.clone()
    })
}

/// Register `entry` under a fresh ref id, evicting the least recently used past the cap.
#[cfg(feature = "treesit")]
fn tree_register(mut entry: TreeEntry) -> u64 {
    let id = crate::process::monitor::next_ref();
    let mut handles = crate::core::sync::lock(&TREE_HANDLES);
    handles.clock += 1;
    entry.last_used = handles.clock;
    while handles.entries.len() >= TREE_HANDLE_CAP {
        let oldest = handles
            .entries
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| *k);
        match oldest {
            Some(k) => handles.entries.remove(&k),
            None => break,
        };
    }
    handles.entries.insert(id, entry);
    id
}

/// Parse `text` (a rope, read chunk by chunk — never flattened) with `lang`, from `old`.
#[cfg(feature = "treesit")]
fn parse_rope(
    text: &ropey::Rope,
    lang: &str,
    old: Option<&tree_sitter::Tree>,
) -> Result<tree_sitter::Tree, LispError> {
    let language = language_for(lang)?;
    let mut parser = checkout_parser(lang, &language)?;
    let length = text.len_bytes();
    let tree = parser.parse_with_options(
        &mut |byte: usize, _point: tree_sitter::Point| -> &[u8] {
            if byte >= length {
                return &[];
            }
            let (chunk, chunk_start, _, _) = text.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - chunk_start..]
        },
        old,
        None,
    );
    return_parser(lang, parser);
    tree.ok_or_else(|| LispError::runtime(format!("tree-sitter: {lang}: parse failed")))
}

/// The tree-sitter `Point` of `byte` in `text`: its line, and its byte column in that line.
/// Lines are ropey's (which also breaks on `\r` alone and the Unicode separators, where
/// tree-sitter breaks on `\n`); the parse itself is byte-addressed, so a row that differs on
/// such text skews only a point-based query, which nothing here makes.
#[cfg(feature = "treesit")]
fn point_of(text: &ropey::Rope, byte: usize) -> tree_sitter::Point {
    let byte = byte.min(text.len_bytes());
    let row = text.byte_to_line(byte);
    tree_sitter::Point::new(row, byte - text.line_to_byte(row))
}

/// The common prefix of two ropes in bytes, compared chunk run by chunk run (a `memcmp`
/// per run, not a loop per byte).
#[cfg(feature = "treesit")]
fn common_prefix(old: &ropey::Rope, new: &ropey::Rope) -> usize {
    let mut old_chunks = old.chunks();
    let mut new_chunks = new.chunks();
    let mut old_run: &[u8] = &[];
    let mut new_run: &[u8] = &[];
    let mut total = 0usize;
    loop {
        if old_run.is_empty() {
            match old_chunks.next() {
                Some(chunk) => old_run = chunk.as_bytes(),
                None => return total,
            }
            continue;
        }
        if new_run.is_empty() {
            match new_chunks.next() {
                Some(chunk) => new_run = chunk.as_bytes(),
                None => return total,
            }
            continue;
        }
        let n = old_run.len().min(new_run.len());
        if old_run[..n] == new_run[..n] {
            total += n;
            old_run = &old_run[n..];
            new_run = &new_run[n..];
        } else {
            let first = old_run[..n]
                .iter()
                .zip(&new_run[..n])
                .position(|(a, b)| a != b)
                .unwrap_or(n);
            return total + first;
        }
    }
}

/// The common suffix of two ropes in bytes, at most `limit` — the same run comparison as
/// [`common_prefix`], walking the chunks from the end.
#[cfg(feature = "treesit")]
fn common_suffix(old: &ropey::Rope, new: &ropey::Rope, limit: usize) -> usize {
    let mut old_chunks = old.chunks_at_byte(old.len_bytes()).0.reversed();
    let mut new_chunks = new.chunks_at_byte(new.len_bytes()).0.reversed();
    let mut old_run: &[u8] = &[];
    let mut new_run: &[u8] = &[];
    let mut total = 0usize;
    while total < limit {
        if old_run.is_empty() {
            match old_chunks.next() {
                Some(chunk) => old_run = chunk.as_bytes(),
                None => break,
            }
            continue;
        }
        if new_run.is_empty() {
            match new_chunks.next() {
                Some(chunk) => new_run = chunk.as_bytes(),
                None => break,
            }
            continue;
        }
        let n = old_run.len().min(new_run.len()).min(limit - total);
        let old_tail = &old_run[old_run.len() - n..];
        let new_tail = &new_run[new_run.len() - n..];
        if old_tail == new_tail {
            total += n;
            old_run = &old_run[..old_run.len() - n];
            new_run = &new_run[..new_run.len() - n];
        } else {
            let same = old_tail
                .iter()
                .rev()
                .zip(new_tail.iter().rev())
                .position(|(a, b)| a != b)
                .unwrap_or(n);
            return total + same;
        }
    }
    total.min(limit)
}

/// The `[start old-end new-end)` byte edit turning `old` into `new`, from their common
/// prefix and suffix — [`derive_edit`] over ropes. Widened to whole characters, so the
/// edit never splits a UTF-8 sequence. `None` when the texts are equal.
#[cfg(feature = "treesit")]
fn derive_rope_edit(old: &ropey::Rope, new: &ropey::Rope) -> Option<(usize, usize, usize)> {
    let prefix = common_prefix(old, new);
    let shorter = old.len_bytes().min(new.len_bytes());
    let suffix = common_suffix(old, new, shorter - prefix.min(shorter));
    let start = old.char_to_byte(old.byte_to_char(prefix));
    let old_end_raw = old.len_bytes() - suffix;
    // round the end UP to a char boundary: the suffix bytes are identical in both texts, so a
    // boundary in one is a boundary in the other at the same distance from the end
    let old_end_char = old.byte_to_char(old_end_raw);
    let old_end = if old.char_to_byte(old_end_char) == old_end_raw {
        old_end_raw
    } else {
        old.char_to_byte(old_end_char + 1)
    };
    let new_end = new.len_bytes() - (old.len_bytes() - old_end);
    if start == old_end && start == new_end {
        return None;
    }
    Some((start, old_end, new_end.max(start)))
}

/// Compose a sequence of CHAR edits `[start old-end new-end]`, each in the coordinates of the
/// text the previous ones produced, into the one edit that covers them all: `start` and
/// `old-end` in the original text, `new-end` in the final one. tree-sitter is told one edit
/// per reparse, and a wider one is only a wider reparse — never a wrong tree.
#[cfg(feature = "treesit")]
fn compose_char_edits(edits: &[(usize, usize, usize)]) -> Option<(usize, usize, usize)> {
    let mut iter = edits.iter();
    let (mut start, mut old_end, mut new_end) = *iter.next()?;
    for &(next_start, next_old_end, next_new_end) in iter {
        // the covered range in CURRENT coordinates is [start, new_end); the next edit replaces
        // [next_start, next_old_end) of the current text
        let covered_end = new_end.max(next_old_end);
        // map the covered end back to the ORIGINAL text: past the cover the two differ by
        // the cover's own growth
        let original_end = if covered_end == new_end {
            old_end
        } else {
            covered_end + old_end - new_end
        };
        let shifted_end = covered_end + next_new_end - next_old_end;
        start = start.min(next_start);
        old_end = original_end.max(old_end);
        new_end = shifted_end;
    }
    Some((start, old_end, new_end))
}

/// A byte `InputEdit` from a `[start old-end new-end)` byte triple and both texts.
#[cfg(feature = "treesit")]
fn input_edit(
    old: &ropey::Rope,
    new: &ropey::Rope,
    (start, old_end, new_end): (usize, usize, usize),
) -> tree_sitter::InputEdit {
    tree_sitter::InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_of(old, start),
        old_end_position: point_of(old, old_end),
        new_end_position: point_of(new, new_end),
    }
}

/// `(%tree-sitter-tree text lang)` — parse a whole document into a fresh handle.
#[cfg(feature = "treesit")]
pub fn tree_open(text: ropey::Rope, lang: &str) -> Result<u64, LispError> {
    let tree = parse_rope(&text, lang, None)?;
    Ok(tree_register(TreeEntry {
        tree,
        text,
        lang: lang.to_string(),
        epoch: grammar_epoch(),
        changed: None,
        last_used: 0,
    }))
}

/// `(%tree-sitter-tree-reparse tree text lang edits)` — the handle for `text`, reparsed
/// incrementally from `old`'s tree. `edits` is the caller's account of what changed, as CHAR
/// triples `[start old-end new-end]` applied in order; `None` derives the edit by comparing
/// the ropes (prefix and suffix, chunk by chunk). An edit that does not fit the two texts —
/// whose unchanged tails differ in length — is an error, never a silently wrong tree.
#[cfg(feature = "treesit")]
pub fn tree_reparse(
    old: u64,
    text: ropey::Rope,
    lang: &str,
    edits: Option<Vec<(usize, usize, usize)>>,
) -> Result<u64, LispError> {
    let previous = tree_entry(old).filter(|e| e.lang == lang && e.epoch == grammar_epoch());
    let Some(previous) = previous else {
        return tree_open(text, lang); // gone, another language, or another grammar
    };
    let byte_edit = match edits {
        None => derive_rope_edit(&previous.text, &text),
        Some(edits) => match compose_char_edits(&edits) {
            None => derive_rope_edit(&previous.text, &text),
            Some((start, old_end, new_end)) => {
                let old_chars = previous.text.len_chars();
                let new_chars = text.len_chars();
                if start > old_end
                    || old_end > old_chars
                    || start > new_end
                    || new_end > new_chars
                    || old_chars - old_end != new_chars - new_end
                {
                    return Err(LispError::runtime(format!(
                        "%tree-sitter-tree-reparse: edit [{start} {old_end} {new_end}] does \
                         not turn the tree's text ({old_chars} chars) into this one \
                         ({new_chars} chars)"
                    )));
                }
                Some((
                    previous.text.char_to_byte(start),
                    previous.text.char_to_byte(old_end),
                    text.char_to_byte(new_end),
                ))
            }
        },
    };
    let Some(byte_edit) = byte_edit else {
        // the same text: a new handle on the same tree, nothing changed
        return Ok(tree_register(TreeEntry {
            changed: Some(Vec::new()),
            text,
            ..previous
        }));
    };
    let (start, old_end, new_end) = byte_edit;
    let old_length = previous.text.len_bytes();
    // the same rule as `derive_edit`: an edit spanning most of a large document is a new
    // document, and reparsing from the old tree buys nothing
    if (old_end - start) * 4 > old_length && old_length > 4096 {
        return tree_open(text, lang);
    }
    let mut edited = previous.tree.clone();
    edited.edit(&input_edit(&previous.text, &text, byte_edit));
    let tree = parse_rope(&text, lang, Some(&edited))?;
    let mut changed: Vec<(usize, usize)> = edited
        .changed_ranges(&tree)
        .map(|range| (range.start_byte, range.end_byte))
        .collect();
    changed.push((start, new_end));
    changed.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(changed.len());
    for (from, to) in changed {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    Ok(tree_register(TreeEntry {
        tree,
        text,
        lang: lang.to_string(),
        epoch: previous.epoch,
        changed: Some(merged),
        last_used: 0,
    }))
}

/// `(%tree-sitter-tree-spans tree kinds keywords? from to)` — the fontify walk over a held
/// tree, restricted to chars `[from, to)` and clipped to it; `Nil` for a gone handle.
#[cfg(feature = "treesit")]
pub fn tree_spans(
    heap: &mut Heap,
    handle: u64,
    kinds: Vec<String>,
    keywords: bool,
    from: i64,
    to: i64,
) -> LispResult {
    let Some(entry) = tree_entry(handle) else {
        return Ok(Value::Nil);
    };
    let chars = entry.text.len_chars();
    let from = (from.max(0) as usize).min(chars);
    let to = (to.max(0) as usize).min(chars).max(from);
    let window = (entry.text.char_to_byte(from), entry.text.char_to_byte(to));
    let wanted: std::collections::HashSet<String> = kinds.into_iter().collect();
    let query = SpanQuery {
        wanted: &wanted,
        keywords,
        window: Some(window),
    };
    let mut out: Vec<Value> = Vec::new();
    spans_into(heap, entry.tree.root_node(), &entry.text, &query, &mut out);
    Ok(heap.alloc_vector(out))
}

/// `(%tree-sitter-tree-changes tree)` — the char ranges `[start end]` of `tree`'s text whose
/// colouring may differ from the tree it was reparsed from; `Bool(true)` when it was a fresh
/// parse (everything may differ), `Nil` for a gone handle.
#[cfg(feature = "treesit")]
pub fn tree_changes(heap: &mut Heap, handle: u64) -> LispResult {
    let Some(entry) = tree_entry(handle) else {
        return Ok(Value::Nil);
    };
    let Some(changed) = entry.changed else {
        return Ok(Value::Bool(true));
    };
    let ranges: Vec<Value> = changed
        .into_iter()
        .map(|(from, to)| {
            let pair = vec![
                Value::int(entry.text.char_of(from) as i64),
                Value::int(entry.text.char_of(to) as i64),
            ];
            heap.alloc_vector(pair)
        })
        .collect();
    Ok(heap.alloc_vector(ranges))
}

/// `(%tree-sitter-tree-free tree)` — drop a handle now; true when it was still held.
#[cfg(feature = "treesit")]
pub fn tree_free(handle: u64) -> bool {
    crate::core::sync::lock(&TREE_HANDLES)
        .entries
        .remove(&handle)
        .is_some()
}

#[cfg(not(feature = "treesit"))]
fn without_treesit(who: &str) -> LispError {
    LispError::runtime(format!(
        "{who}: this runtime was built without tree-sitter (rebuild with --features treesit)"
    ))
}

#[cfg(not(feature = "treesit"))]
pub fn tree_open(_text: ropey::Rope, _lang: &str) -> Result<u64, LispError> {
    Err(without_treesit("%tree-sitter-tree"))
}

#[cfg(not(feature = "treesit"))]
pub fn tree_reparse(
    _old: u64,
    _text: ropey::Rope,
    _lang: &str,
    _edits: Option<Vec<(usize, usize, usize)>>,
) -> Result<u64, LispError> {
    Err(without_treesit("%tree-sitter-tree-reparse"))
}

#[cfg(not(feature = "treesit"))]
pub fn tree_spans(
    _heap: &mut crate::core::heap::Heap,
    _handle: u64,
    _kinds: Vec<String>,
    _keywords: bool,
    _from: i64,
    _to: i64,
) -> LispResult {
    Err(without_treesit("%tree-sitter-tree-spans"))
}

#[cfg(not(feature = "treesit"))]
pub fn tree_changes(_heap: &mut crate::core::heap::Heap, _handle: u64) -> LispResult {
    Err(without_treesit("%tree-sitter-tree-changes"))
}

#[cfg(not(feature = "treesit"))]
pub fn tree_free(_handle: u64) -> bool {
    false
}

#[cfg(not(feature = "treesit"))]
pub fn spans(
    heap: &mut crate::core::heap::Heap,
    src: &str,
    lang: &str,
    _kinds: Vec<String>,
    _keywords: bool,
) -> LispResult {
    parse(heap, src, lang)
}

/// `(tree-sitter-load-grammar path lang)` — load a tree-sitter grammar from a
/// shared library at runtime and register it under the language keyword `lang`,
/// so `tree-sitter-parse` can use it like a built-in one. Returns the grammar's
/// ABI version.
///
/// **Why this rather than a search path.** The kernel does not know where an
/// application keeps its grammars, and should not: a path convention is policy.
/// This takes the library the caller chose, which leaves "look in
/// `~/.config/bedit/grammars`, named for the file's mode" entirely in the editor,
/// and leaves the kernel with the mechanism it can actually own — dlopen the
/// library, find `tree_sitter_<lang>`, check the ABI, remember it.
///
/// **The library is leaked, deliberately.** A `Language` is a pointer into the
/// loaded object's static tables, so unmapping the library would dangle every
/// tree ever parsed with it. Grammars are loaded once and used for the life of
/// the process; `std::mem::forget` says that in one line, where an `Arc` holding
/// the `Library` alongside every `Language` would say it in many and still never
/// drop.
///
/// Failure is an ordinary error at every step — a missing file, a library with no
/// such grammar, a grammar built for an incompatible tree-sitter. None of them is
/// a crash. What this cannot make safe is a *hostile* library: it is native code
/// and runs with the process's privileges, exactly like an Emacs dynamic module,
/// so an application should load only from a directory its user controls.
#[cfg(feature = "treesit")]
pub fn load_grammar(path: &str, lang: &str) -> LispResult {
    // Brood keywords are spelled with hyphens and C identifiers cannot be, so `:c-sharp`
    // asks for `tree_sitter_c_sharp` — which is exactly what the tree-sitter-c-sharp grammar
    // exports. The library's entry point is named for the GRAMMAR, so `lang` names the
    // grammar rather than being a nickname for it; there is no aliasing here and shouldn't
    // be, since the mode that asks to parse `:c-sharp` is the same name on both sides.
    let symbol = format!("tree_sitter_{}", lang.replace('-', "_"));
    // SAFETY: dlopen'ing a caller-chosen library and calling the grammar constructor it
    // exports. The symbol type is tree-sitter's documented grammar entry point, and the
    // Language it yields is validated against the host's ABI range below before any use.
    let language = unsafe {
        let lib = libloading::Library::new(path)
            .map_err(|e| LispError::runtime(format!("%tree-sitter-load-grammar:{path}: {e}")))?;
        let entry: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
            lib.get(symbol.as_bytes()).map_err(|e| {
                LispError::runtime(format!(
                    "%tree-sitter-load-grammar:{path} exports no {symbol}: {e}"
                ))
            })?;
        let f = *entry;
        std::mem::forget(lib); // the grammar's tables must stay mapped — see above
        tree_sitter::Language::new(tree_sitter_language::LanguageFn::from_raw(f))
    };
    // `set_language` is the ABI gate: it range-checks the grammar's version against what
    // this tree-sitter can drive, so a grammar from another era is a message, not a crash.
    tree_sitter::Parser::new()
        .set_language(&language)
        .map_err(|e| {
            LispError::runtime(format!(
                "%tree-sitter-load-grammar:{path}: {lang} is not usable by this runtime \
                 (grammar ABI {}, this build drives {}..={}): {e}",
                language.abi_version(),
                tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
                tree_sitter::LANGUAGE_VERSION
            ))
        })?;
    let abi = language.abi_version() as i64;
    crate::core::sync::lock(&DYNAMIC).insert(lang.to_string(), language);
    // A re-load replaces the entry, so a grammar can be swapped without restarting; the
    // parser pool is keyed by language name and holds parsers bound to the OLD language, so
    // it is cleared for this one rather than left to hand out stale parsers.
    crate::core::sync::lock(&PARSER_POOL).remove(lang);
    // The tree cache is keyed by language name too, and holds trees SHAPED BY the old
    // grammar. An exact hit would hand one back for the new one, and an incremental
    // reparse would hand it to the new grammar's parser as the tree to edit from — which
    // is not merely stale but ill-formed, and fails the parse outright. Same reasoning as
    // the pool above; the cache simply arrived later than the comment did.
    crate::core::sync::lock(&TREE_CACHE).retain(|(l, _, _)| l != lang);
    // Held tree handles keep answering queries from the trees they have — those are valid
    // trees, merely from the old grammar — but none is reparsed FROM again, for the reason
    // above: a reparse after this sees a newer epoch and parses afresh.
    GRAMMAR_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    Ok(Value::int(abi))
}

/// Grammars loaded at runtime by [`load_grammar`], keyed by language name. Consulted
/// BEFORE the compile-time arms, so loading one deliberately overrides a bundled grammar of
/// the same name — the point of being able to load one at all is to move faster than the
/// runtime's release cycle.
#[cfg(feature = "treesit")]
static DYNAMIC: LazyLock<Mutex<HashMap<String, tree_sitter::Language>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "treesit")]
fn dynamic_language(lang: &str) -> Option<tree_sitter::Language> {
    crate::core::sync::lock(&DYNAMIC).get(lang).cloned()
}

/// `(%tree-sitter-languages)` — the languages this runtime can parse right now: every
/// grammar loaded at runtime plus the ones built in, sorted. The question an editor asks
/// before it offers to fetch a grammar, or tells you a file's mode has none: without it the
/// only way to find out was to parse something and read the error.
#[cfg(feature = "treesit")]
pub fn languages(heap: &mut Heap) -> LispResult {
    #[allow(unused_mut)] // a build with no grammar feature never pushes a built-in
    let mut names: Vec<String> = crate::core::sync::lock(&DYNAMIC).keys().cloned().collect();
    #[cfg(feature = "treesit-ruby")]
    names.push("ruby".to_string());
    #[cfg(feature = "treesit-elixir")]
    names.push("elixir".to_string());
    names.sort();
    names.dedup();
    let keywords = names
        .iter()
        .map(|name| Value::keyword(crate::core::value::intern(name)))
        .collect();
    Ok(heap.alloc_vector(keywords))
}

#[cfg(not(feature = "treesit"))]
pub fn languages(heap: &mut crate::core::heap::Heap) -> LispResult {
    Ok(heap.alloc_vector(Vec::new()))
}

#[cfg(not(feature = "treesit"))]
pub fn load_grammar(_path: &str, lang: &str) -> LispResult {
    Err(LispError::runtime(format!(
        "%tree-sitter-load-grammar::{lang}: this runtime was built without tree-sitter \
         (rebuild with --features treesit)"
    )))
}

/// The grammar for a language keyword's name. Each arm is gated on its own
/// `treesit-<lang>` feature — the kernel ships no grammar by default (a `treesit`
/// build with no grammar feature has zero arms and reports every language as not
/// built in). One cfg'd arm per language is the single place a grammar plugs in
/// (plus its `Cargo.toml` dep).
#[cfg(feature = "treesit")]
fn language_for(lang: &str) -> Result<tree_sitter::Language, LispError> {
    // A grammar loaded at runtime wins over a bundled one of the same name: loading one is a
    // deliberate act, and the reason to do it is usually that the bundled grammar is behind.
    if let Some(l) = dynamic_language(lang) {
        return Ok(l);
    }
    match lang {
        #[cfg(feature = "treesit-ruby")]
        "ruby" => Ok(tree_sitter_ruby::LANGUAGE.into()),
        #[cfg(feature = "treesit-elixir")]
        "elixir" => Ok(tree_sitter_elixir::LANGUAGE.into()),
        other => Err(LispError::runtime(format!(
            "%tree-sitter-parse:no grammar for :{other} — load one at runtime with \
             (%tree-sitter-load-grammar \"/path/to/libtree-sitter-{other}.so\" :{other}), \
             or build it in with --features treesit-{other}"
        ))),
    }
}

/// A pool of ready `Parser`s per language, so a parse doesn't pay
/// `Parser::new()` + `set_language` (grammar load) every call. `Parser` is
/// `Send`, and Brood green processes migrate across worker threads, so this is a
/// global pool rather than thread-local. Capped so a burst of concurrent parses
/// can't retain an unbounded number of parsers.
#[cfg(feature = "treesit")]
static PARSER_POOL: LazyLock<Mutex<HashMap<String, Vec<tree_sitter::Parser>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "treesit")]
const PARSER_POOL_PER_LANG: usize = 8;

/// Borrow a parser configured for `lang` from the pool (or build one). Held by
/// the caller during the parse — *not* across the pool lock — so parses run
/// concurrently. Return it with [`return_parser`].
#[cfg(feature = "treesit")]
fn checkout_parser(
    lang: &str,
    language: &tree_sitter::Language,
) -> Result<tree_sitter::Parser, LispError> {
    if let Some(p) = crate::core::sync::lock(&PARSER_POOL)
        .get_mut(lang)
        .and_then(Vec::pop)
    {
        return Ok(p);
    }
    let mut p = tree_sitter::Parser::new();
    p.set_language(language)
        .map_err(|e| LispError::runtime(format!("%tree-sitter-parse:{lang}: {e}")))?;
    Ok(p)
}

/// Return a parser to the pool for re-use (dropping it if the pool is full).
#[cfg(feature = "treesit")]
fn return_parser(lang: &str, parser: tree_sitter::Parser) {
    // Poison-tolerant like every lock here: a parse that panicked must not make every
    // later parse in the runtime panic too.
    let mut pool = crate::core::sync::lock(&PARSER_POOL);
    let slot = pool.entry(lang.to_string()).or_default();
    if slot.len() < PARSER_POOL_PER_LANG {
        slot.push(parser);
    }
}

/// Per-byte → character-offset table for `s`: `t[b]` is the count of characters
/// before byte offset `b`. Length `s.len() + 1` so a node's end byte (which can
/// equal `s.len()`) is indexable. tree-sitter spans land on char boundaries (it
/// parses UTF-8); a byte interior to a multi-byte char maps to that char's own
/// index. (Mirror of `builtins.rs::byte_to_char_offsets`, kept local so the
/// feature-off build links neither.)
#[cfg(feature = "treesit")]
fn byte_to_char_offsets(s: &str) -> Vec<u32> {
    let mut t = vec![0u32; s.len() + 1];
    let mut byte = 0usize;
    let mut ci = 0u32;
    for ch in s.chars() {
        let w = ch.len_utf8();
        for k in 0..w {
            t[byte + k] = ci;
        }
        byte += w;
        ci += 1;
    }
    t[s.len()] = ci;
    t
}

/// Convert a tree-sitter node (and its subtree) into a positioned node map,
/// mirroring `builtins.rs::cst_to_positioned`: a node with children carries
/// `:kids` (ALL children — named and anonymous, so keywords/operators are
/// present for fontify); a leaf carries `:text`.
#[cfg(feature = "treesit")]
fn node_to_positioned(heap: &mut Heap, node: tree_sitter::Node, src: &str, b2c: &[u32]) -> Value {
    let kw = |k: &str| Value::keyword(value::intern(k));
    let start = Value::int(b2c[node.start_byte()] as i64);
    let end = Value::int(b2c[node.end_byte()] as i64);
    let mut pairs: Vec<(Value, Value)> = vec![
        (kw("kind"), kw(node.kind())),
        (kw("start"), start),
        (kw("end"), end),
        (kw("named"), Value::boolean(node.is_named())),
    ];
    // Surface tree-sitter's error-recovery state so editor mode services can draw
    // diagnostics over a foreign tree: `:error` is an `ERROR` node, `:missing` a
    // zero-width inserted node (which `:kind` can't signal — it has no error-string
    // and zero width). Pushed only when set, to keep the common (valid) node small.
    if node.is_error() {
        pairs.push((kw("error"), Value::boolean(true)));
    }
    if node.is_missing() {
        pairs.push((kw("missing"), Value::boolean(true)));
    }
    // `:broken` on the ROOT: does this tree contain an error ANYWHERE? tree-sitter
    // keeps that as a bit on every node, so asking is O(1) — where deciding it in
    // Brood means walking the whole projected tree, which is what the indenter was
    // doing on every keypress (two trees, thousands of nodes, ~250 ms on a 22 KB
    // Elixir file). Only the root carries it: a caller wanting per-node state has
    // `:error`/`:missing` already, and putting a redundant flag on every node would
    // grow every tree to save one lookup.
    if node.parent().is_none() && node.has_error() {
        pairs.push((kw("broken"), Value::boolean(true)));
    }
    if node.child_count() == 0 {
        let text = heap.alloc_string(&src[node.start_byte()..node.end_byte()]);
        pairs.push((kw("text"), text));
    } else {
        // Collect child maps first (recursion needs `&mut heap`), then the vector.
        let mut cursor = node.walk();
        let children: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
        let kids: Vec<Value> = children
            .into_iter()
            .map(|c| node_to_positioned(heap, c, src, b2c))
            .collect();
        let kids_vec = heap.alloc_vector(kids);
        pairs.push((kw("kids"), kids_vec));
    }
    heap.map_from_pairs(pairs)
}

/// Feature-off stub: the builtin is registered unconditionally (like `gui-*`), so
/// calling it without the parser built in gives a clear rebuild hint.
#[cfg(not(feature = "treesit"))]
pub fn parse(_heap: &mut crate::core::heap::Heap, _src: &str, lang: &str) -> LispResult {
    Err(LispError::runtime(format!(
        "%tree-sitter-parse::{lang}: this runtime was built without tree-sitter \
         (rebuild with --features treesit)"
    )))
}

#[cfg(not(feature = "treesit"))]
pub fn chain(heap: &mut crate::core::heap::Heap, src: &str, lang: &str, _o: i64) -> LispResult {
    parse(heap, src, lang)
}

#[cfg(not(feature = "treesit"))]
pub fn kids(heap: &mut crate::core::heap::Heap, src: &str, lang: &str, _o: i64) -> LispResult {
    parse(heap, src, lang)
}

#[cfg(not(feature = "treesit"))]
pub fn errors(heap: &mut crate::core::heap::Heap, src: &str, lang: &str) -> LispResult {
    parse(heap, src, lang)
}

#[cfg(all(test, feature = "treesit-grammars"))]
mod parse_cache_tests {
    use super::*;

    fn cached_texts() -> Vec<String> {
        crate::core::sync::lock(&TREE_CACHE)
            .iter()
            .map(|(_, src, _)| src.clone())
            .collect()
    }

    /// An editor parses one document in two shapes: the indenter the whole text, the view
    /// the window on screen. A keystroke's whole-text parse must replace the whole text's
    /// OWN older entry and leave the window's, so the window's next parse is incremental
    /// too. With one entry per language, each shape evicted the other and every keystroke
    /// paid a full reparse.
    #[test]
    fn an_edit_replaces_its_own_document_and_keeps_the_other_shape() {
        crate::core::sync::lock(&TREE_CACHE).clear();
        let document: String = (0..200)
            .map(|i| format!("def method_{i}\n  {i}\nend\n"))
            .collect();
        let window = document[1000..2000].to_string();
        let edited = format!("{}x{}", &document[..3000], &document[3000..]);

        parse_cached(&document, "ruby").expect("document parses");
        parse_cached(&window, "ruby").expect("window parses");
        parse_cached(&edited, "ruby").expect("edited document parses");

        let texts = cached_texts();
        assert!(
            texts.contains(&window),
            "the window's tree was evicted by the document's edit"
        );
        assert!(texts.contains(&edited), "the edited document is cached");
        assert!(
            !texts.contains(&document),
            "the document's older version is replaced, not kept"
        );
    }
}

#[cfg(all(test, feature = "treesit"))]
mod tree_handle_edit_tests {
    use super::*;

    /// Two edits in sequence compose into the one edit that covers both, in the original
    /// text's coordinates for its start and old end and the final text's for its new end.
    #[test]
    fn edits_in_sequence_compose_into_one_covering_edit() {
        // "abcdef" -> "aXYZdef" (bc -> XYZ) -> "aXYZdeQf" (insert Q before f)
        assert_eq!(compose_char_edits(&[(1, 3, 4), (6, 6, 7)]), Some((1, 5, 7)));
        // an insert BEFORE the first edit widens the start, not the end
        assert_eq!(compose_char_edits(&[(1, 3, 4), (0, 0, 1)]), Some((0, 3, 5)));
        // a delete overlapping the first edit's front
        assert_eq!(compose_char_edits(&[(1, 3, 4), (0, 2, 0)]), Some((0, 3, 2)));
        assert_eq!(compose_char_edits(&[]), None);
    }

    /// The rope diff finds one contiguous change exactly, across chunk boundaries, and
    /// widens it to whole characters when the change is inside a multi-byte one.
    #[test]
    fn a_rope_diff_finds_the_change_and_never_splits_a_character() {
        let base: String = "fn main() { let x = 1; }\n".repeat(400);
        let old = ropey::Rope::from_str(&base);
        assert!(
            old.chunks().count() > 1,
            "the test must cross a chunk boundary"
        );
        let mut edited = base.clone();
        edited.insert_str(5000, "yy");
        let new = ropey::Rope::from_str(&edited);
        assert_eq!(derive_rope_edit(&old, &new), Some((5000, 5000, 5002)));
        assert_eq!(derive_rope_edit(&old, &old.clone()), None);
        // é (C3 A9) -> è (C3 A8): the bytes differ only in the second, the edit is the char
        let accented_old = ropey::Rope::from_str("caf\u{e9}!");
        let accented_new = ropey::Rope::from_str("caf\u{e8}!");
        assert_eq!(
            derive_rope_edit(&accented_old, &accented_new),
            Some((3, 5, 5))
        );
    }
}

#[cfg(all(test, feature = "treesit-grammars"))]
mod tree_handle_tests {
    use super::*;

    fn spans_of(handle: u64, from: i64, to: i64) -> Vec<(i64, i64)> {
        let mut heap = Heap::new();
        let kinds = vec!["string".to_string(), "comment".to_string()];
        let v = tree_spans(&mut heap, handle, kinds, false, from, to).expect("spans");
        heap.seq_items(v)
            .expect("a vector")
            .into_iter()
            .map(|span| {
                let parts = heap.seq_items(span).expect("a span");
                match (parts[0], parts[1]) {
                    (Value::Int(a), Value::Int(b)) => (a, b),
                    _ => panic!("span offsets are ints"),
                }
            })
            .collect()
    }

    /// A reparse from an edit — explicit or derived — colours exactly what a fresh parse of
    /// the new text does, and the old handle still answers for the old text.
    #[test]
    fn an_incremental_reparse_matches_a_fresh_parse() {
        let source: String = (0..300)
            .map(|i| format!("def m{i}\n  x = \"s{i}\" # c{i}\nend\n"))
            .collect();
        let old_text = ropey::Rope::from_str(&source);
        let old = tree_open(old_text.clone(), "ruby").expect("parses");
        // open a string above the window: everything after it is a string now
        let mut new_text = old_text.clone();
        new_text.insert(4, "\"");
        let length = new_text.len_chars() as i64;
        let fresh = tree_open(new_text.clone(), "ruby").expect("parses");
        let derived = tree_reparse(old, new_text.clone(), "ruby", None).expect("reparses");
        let explicit =
            tree_reparse(old, new_text.clone(), "ruby", Some(vec![(4, 4, 5)])).expect("reparses");
        let window = (length / 2, length / 2 + 400);
        assert_eq!(
            spans_of(derived, window.0, window.1),
            spans_of(fresh, window.0, window.1)
        );
        assert_eq!(
            spans_of(explicit, window.0, window.1),
            spans_of(fresh, window.0, window.1)
        );
        // the old handle is untouched by the reparse
        assert_eq!(spans_of(old, 0, 40), {
            let again = tree_open(old_text, "ruby").expect("parses");
            spans_of(again, 0, 40)
        });
        // an edit that does not fit the texts is refused
        assert!(tree_reparse(old, new_text, "ruby", Some(vec![(4, 4, 9)])).is_err());
        assert!(tree_free(fresh));
        let mut heap = Heap::new();
        let gone = tree_spans(&mut heap, fresh, Vec::new(), false, 0, 10).expect("answers");
        assert!(matches!(gone, Value::Nil), "a freed handle reads as nil");
    }
}
