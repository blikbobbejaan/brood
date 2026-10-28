//! `%fuzzy-top` — the ranking pass under `std/fuzzy`'s `top` / `filter`, native.
//!
//! ADR-375 amends ADR-357, which decided this loop stays Brood so that the scoring rules
//! stay redefinable at runtime (`*fuzzy-scorer*`). That decision is kept: this primitive
//! implements the DEFAULT rules only, and `fuzzy/top` calls it only while `*fuzzy-scorer*`
//! is still the default. Bind your own and every ranking in the image goes back through
//! the Brood walk. What this buys is the price of NOT redefining them, which an editor
//! pays on every keystroke: ranking tens of thousands of paths in single-digit ms, with no
//! shard fan-out and so no copy of the candidate set across processes per keystroke.
//!
//! The rules are `std/fuzzy.blsp`'s, ported term for term (ADR-497 for what they became):
//! `fuzzy-char-bonus` (match, boundary, file name) and `fuzzy-step` (contiguous, gap, lead);
//! the OPTIMAL alignment of a fuzzy term (`fuzzy-optimal`, the same recurrence) with the
//! greedy walk and first run past `DP_MAX_CELLS`; the term operators `!` `=` `^` `$` and smart
//! case (`fuzzy-parse-term`); the `:boost` and the exact-match bonus (`fuzzy-score-of`); and the
//! `[-score segments length cand]` ordering. `tests/fuzzy_native_test.blsp` pins the two equal
//! over a corpus; that test is the contract, not this comment.
//!
//! Only scores are computed here — never positions. `fuzzy/match` traces those in Brood for
//! the rows a UI draws, so a tie between two alignments of equal score never has to be broken
//! the same way twice.
//!
//! What a call allocates: the parsed terms, the result, and a handful of scratch vectors
//! reused across every candidate (its characters, their lowered form, their bonuses, two DP
//! rows). Nothing is allocated per candidate. A candidate is rejected by a subsequence check
//! of every term before any bonus is computed, and a session that narrows (`:subset`, ADR-497)
//! visits only the candidates its previous query matched.
//!
//! One deliberate difference. Positions found in the LOWERED candidate index the ORIGINAL
//! (case is what a camelCase hump is made of), which is only sound while lowering preserves
//! length. Both sides lower per character and keep the first character of an expansion
//! (`İ` → `i`), so they cannot run out of step.

use super::args::{arg, expect_int, expect_string_ref};
use crate::core::heap::Heap;
use crate::core::value::{self, EnvId, MapId, Value, ValueRef};
use crate::error::{LispError, LispResult};

pub(super) fn register(primitives: &mut super::Primitives) {
    use super::signature_types::*;
    use crate::core::value::Arity;
    use crate::types::Sig;
    primitives.def(
        "%fuzzy-top",
        Arity::range(2, 4),
        Sig::with_optional(vec![string, seqable], vec![any, any], seqable),
        &["query", "cands", "limit", "options"],
        "The best `limit` of `cands` for `query` under std/fuzzy's built-in rules (a space separates terms that must each match, in any order; `!x` `=x` `^x` `x$` and smart case as fuzzy/top), best score first (ties: fewer path segments for a :file category, shorter candidate, then lexicographic) — a nil or non-positive limit means all of them. `options` is fuzzy/top's map (`:category`, `:boost`) plus two a ranking session uses: `:subset`, ascending indices into `cands` to rank instead of all of them, and `:indices` true, which answers `[hits indices]` — every matching index, ascending — so the next, narrower query can pass them back as its subset. The ranking pass behind fuzzy/top, fuzzy/filter and fuzzy/ranker, native because a completion UI runs it over a whole project on every keystroke; fuzzy/top calls it only while *fuzzy-scorer* is the default, so redefining the rules still moves every ranking (ADR-375, ADR-497). Every candidate must be a string.\n\n    (%fuzzy-top \"ab\" [\"axb\" \"ab\" \"zz\"] 1)   → (\"ab\")",
        fuzzy_top,
    );
}

// The terms of the score — `std/fuzzy.blsp`'s constants.
const MATCH_SCORE: i64 = 1;
const BOUNDARY_BONUS: i64 = 8;
const CONTIGUOUS_BONUS: i64 = 6;
const GAP_PENALTY: i64 = 3;
const GAP_EXTEND_PENALTY: i64 = 1;
const LEAD_PENALTY: i64 = 3;
const BASENAME_BONUS: i64 = 2;
const EXACT_BONUS: i64 = 1_000_000_000;
const DP_MAX_CELLS: usize = 16_384;

/// A DP cell no alignment reaches — the Brood table's nil. Far enough from `i64::MIN` that
/// adding a bonus to it cannot overflow, and below any score a real alignment can have.
const UNREACHED: i64 = i64::MIN / 4;

/// A match just after one of these starts a word — `fuzzy-separators`.
const SEPARATORS: [char; 5] = ['-', '_', '/', '.', ' '];

/// `fuzzy-upper?`: true when lowering `c` changes it, which is how the Brood rule spells
/// "upper case" (`(not (= (string/lower ch) ch))`). A codepoint whose lowering expands to
/// several counts as upper, as it does there.
fn is_upper(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_uppercase();
    }
    let mut lowered = c.to_lowercase();
    match (lowered.next(), lowered.next()) {
        (Some(first), None) => first != c,
        _ => true,
    }
}

/// `fuzzy-fold-char`: `c` lowered to exactly one character.
///
/// The ASCII branch is not a micro-optimisation: this runs once per character of every
/// candidate, so on a 27k-path project it is ~1.1M calls per keystroke, and `to_lowercase`
/// is a Unicode table walk returning an iterator, while paths, module names and symbols
/// are ASCII almost always.
fn lower_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    c.to_lowercase().next().unwrap_or(c)
}

/// `fuzzy-boundary?`: char `pos` starts a word in `chars` — index 0, after a separator, or a
/// camelCase hump (an upper preceded by a non-upper).
fn boundary(chars: &[char], pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let prev = chars[pos - 1];
    SEPARATORS.contains(&prev) || (is_upper(chars[pos]) && !is_upper(prev))
}

/// `fuzzy-gap-cost`: what skipping `gap` characters between two matched ones costs.
fn gap_cost(gap: i64) -> i64 {
    if gap <= 0 {
        0
    } else {
        -(GAP_PENALTY + GAP_EXTEND_PENALTY * (gap - 1))
    }
}

/// `fuzzy-step`: what a query character matched at `pos` adds, given the previous match.
fn step(bonuses: &[i64], pos: usize, prev: Option<usize>) -> i64 {
    let contiguous_term = match prev {
        Some(p) if pos == p + 1 => CONTIGUOUS_BONUS,
        _ => 0,
    };
    let distance_term = match prev {
        None => -LEAD_PENALTY.min(pos as i64),
        Some(p) => gap_cost(pos as i64 - (p as i64 + 1)),
    };
    bonuses[pos] + contiguous_term + distance_term
}

// ---- terms ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fuzzy,
    Literal { start: bool, end: bool },
    Exclude,
}

/// `fuzzy-make-term`: one prepared term. `chars` is folded unless the term is case-sensitive.
struct Term {
    kind: Kind,
    chars: Vec<char>,
    case_sensitive: bool,
}

/// `fuzzy-parse-term`: one space-separated piece of the query as a term, or `None` for a
/// piece that asks nothing (a lone operator).
fn parse_term(piece: &[char]) -> Option<Term> {
    let (kind, body): (Kind, &[char]) = match piece.first() {
        Some('!') => (Kind::Exclude, &piece[1..]),
        Some('=') => (
            Kind::Literal {
                start: false,
                end: false,
            },
            &piece[1..],
        ),
        _ => {
            let start = piece.first() == Some(&'^');
            let from = usize::from(start);
            let end = piece.last() == Some(&'$') && piece.len() > from;
            let body = &piece[from..piece.len() - usize::from(end)];
            let kind = if start || end {
                Kind::Literal { start, end }
            } else {
                Kind::Fuzzy
            };
            (kind, body)
        }
    };
    if body.is_empty() {
        return None;
    }
    let case_sensitive = body.iter().any(|&c| is_upper(c));
    let chars = if case_sensitive {
        body.to_vec()
    } else {
        body.iter().map(|&c| lower_char(c)).collect()
    };
    Some(Term {
        kind,
        chars,
        case_sensitive,
    })
}

/// `fuzzy-exact-of`'s target: the query's pieces joined by one space, folded unless it has
/// an upper-case letter — and whether it is case-sensitive.
fn exact_target(pieces: &[&[char]]) -> (Vec<char>, bool) {
    let mut joined: Vec<char> = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        if index > 0 {
            joined.push(' ');
        }
        joined.extend_from_slice(piece);
    }
    let case_sensitive = joined.iter().any(|&c| is_upper(c));
    if !case_sensitive {
        for c in joined.iter_mut() {
            *c = lower_char(*c);
        }
    }
    (joined, case_sensitive)
}

// ---- aligning a term -----------------------------------------------------------------

/// Whether `needle` is a subsequence of `haystack` — the cheap reject every term runs before
/// any bonus is computed.
fn is_subsequence(needle: &[char], haystack: &[char]) -> bool {
    let mut remaining = needle.iter();
    let mut wanted = remaining.next();
    for c in haystack {
        match wanted {
            None => return true,
            Some(w) if w == c => wanted = remaining.next(),
            _ => {}
        }
    }
    wanted.is_none()
}

/// Whether `needle` occurs in `haystack` at `at` as an unbroken run.
fn run_at(haystack: &[char], needle: &[char], at: usize) -> bool {
    at + needle.len() <= haystack.len() && haystack[at..at + needle.len()] == *needle
}

/// The first index at or after `from` at which `needle` appears in `haystack` as a run.
fn find_run(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&at| run_at(haystack, needle, at))
}

/// `fuzzy-run-at`: the score of a literal run of `length` chars starting at `at`.
fn run_score(bonuses: &[i64], at: usize, length: usize) -> i64 {
    (at..at + length)
        .map(|pos| step(bonuses, pos, if pos == at { None } else { Some(pos - 1) }))
        .sum()
}

/// `fuzzy-walk`: the greedy alignment of `query` inside `hay` from `start`.
fn walk(query: &[char], hay: &[char], bonuses: &[i64], start: usize) -> Option<i64> {
    let mut at = start;
    let mut prev: Option<usize> = None;
    let mut score = 0i64;
    for &qc in query {
        let found = hay[at..].iter().position(|&c| c == qc)? + at;
        score += step(bonuses, found, prev);
        prev = Some(found);
        at = found + 1;
    }
    Some(score)
}

/// `fuzzy-greedy`: the better of the greedy walk and the walk from the first run.
fn greedy(query: &[char], hay: &[char], bonuses: &[i64]) -> Option<i64> {
    let from_start = walk(query, hay, bonuses, 0)?;
    match find_run(hay, query, 0).and_then(|at| walk(query, hay, bonuses, at)) {
        Some(run) if run > from_start => Some(run),
        _ => Some(from_start),
    }
}

/// `fuzzy-gap-arrival`: the best score reaching `j` across a gap, from the running maximum
/// of `row[p] + extend·p` over p ≤ j-2.
fn gap_arrival(gap_best: i64, j: usize) -> i64 {
    if gap_best == UNREACHED {
        return UNREACHED;
    }
    gap_best - GAP_EXTEND_PENALTY * (j as i64 - 1) - (GAP_PENALTY - GAP_EXTEND_PENALTY)
}

/// `fuzzy-optimal`'s score: the best alignment of `query` in `hay`, by the table
/// `fuzzy-dp-first-row` / `fuzzy-dp-row` build. Two rows, reused across candidates.
fn optimal(
    query: &[char],
    hay: &[char],
    bonuses: &[i64],
    row: &mut Vec<i64>,
    next: &mut Vec<i64>,
) -> Option<i64> {
    let n = hay.len();
    row.clear();
    row.extend((0..n).map(|j| {
        if hay[j] == query[0] {
            bonuses[j] - LEAD_PENALTY.min(j as i64)
        } else {
            UNREACHED
        }
    }));
    for &qc in &query[1..] {
        next.clear();
        let mut gap_best = UNREACHED;
        for j in 0..n {
            if j >= 2 && row[j - 2] != UNREACHED {
                gap_best = gap_best.max(row[j - 2] + GAP_EXTEND_PENALTY * (j as i64 - 2));
            }
            let mut cell = UNREACHED;
            if hay[j] == qc {
                let contiguous = if j >= 1 && row[j - 1] != UNREACHED {
                    row[j - 1] + CONTIGUOUS_BONUS
                } else {
                    UNREACHED
                };
                let best = contiguous.max(gap_arrival(gap_best, j));
                if best != UNREACHED {
                    cell = bonuses[j] + best;
                }
            }
            next.push(cell);
        }
        std::mem::swap(row, next);
    }
    row.iter().copied().filter(|&s| s != UNREACHED).max()
}

/// `fuzzy-literal`: a literal term at its anchor, or at the best place it occurs.
fn literal(
    term: &[char],
    start: bool,
    end: bool,
    hay: &[char],
    bonuses: &[i64],
    base: usize,
) -> Option<i64> {
    let length = term.len();
    let n = hay.len();
    if start {
        let at = base;
        return (run_at(hay, term, at) && (!end || at + length == n))
            .then(|| run_score(bonuses, at, length));
    }
    if end {
        let at = n.checked_sub(length)?;
        return run_at(hay, term, at).then(|| run_score(bonuses, at, length));
    }
    let mut best: Option<i64> = None;
    let mut from = 0;
    while let Some(at) = find_run(hay, term, from) {
        let score = run_score(bonuses, at, length);
        best = Some(best.map_or(score, |b| b.max(score)));
        from = at + 1;
    }
    best
}

// ---- one candidate -------------------------------------------------------------------

/// The per-candidate buffers, reused across a whole ranking so nothing is allocated per
/// candidate: its characters, their lowered form, their bonuses, and the DP's two rows.
#[derive(Default)]
struct Scratch {
    original: Vec<char>,
    lowered: Vec<char>,
    bonuses: Vec<i64>,
    row: Vec<i64>,
    next: Vec<i64>,
}

impl Scratch {
    /// Load `text` and answer where its file name starts (just past the last `/`).
    fn load(&mut self, text: &str) -> usize {
        self.original.clear();
        self.original.extend(text.chars());
        self.lowered.clear();
        self.lowered
            .extend(self.original.iter().copied().map(lower_char));
        self.original
            .iter()
            .rposition(|&c| c == '/')
            .map_or(0, |at| at + 1)
    }

    /// `fuzzy-run-terms`' score for the loaded candidate, or `None` when a term fails.
    fn score(&mut self, terms: &[Term], file: bool, base: usize) -> Option<i64> {
        // The cheap reject first: every matching term's characters, in order, before a
        // single bonus is computed — most candidates of a project fail here.
        for term in terms {
            let hay = if term.case_sensitive {
                &self.original
            } else {
                &self.lowered
            };
            if term.kind != Kind::Exclude && !is_subsequence(&term.chars, hay) {
                return None;
            }
        }
        let n = self.original.len();
        self.bonuses.clear();
        for j in 0..n {
            let mut bonus = MATCH_SCORE;
            if boundary(&self.original, j) {
                bonus += BOUNDARY_BONUS;
            }
            if file && j >= base {
                bonus += BASENAME_BONUS;
            }
            self.bonuses.push(bonus);
        }
        let mut total = 0i64;
        for term in terms {
            let hay = if term.case_sensitive {
                &self.original
            } else {
                &self.lowered
            };
            total += match term.kind {
                Kind::Exclude => {
                    if find_run(hay, &term.chars, 0).is_some() {
                        return None;
                    }
                    0
                }
                Kind::Literal { start, end } => literal(
                    &term.chars,
                    start,
                    end,
                    hay,
                    &self.bonuses,
                    if file { base } else { 0 },
                )?,
                Kind::Fuzzy if term.chars.len() * n > DP_MAX_CELLS => {
                    greedy(&term.chars, hay, &self.bonuses)?
                }
                Kind::Fuzzy => optimal(
                    &term.chars,
                    hay,
                    &self.bonuses,
                    &mut self.row,
                    &mut self.next,
                )?,
            };
        }
        Some(total)
    }

    /// `fuzzy-exact-of`: whether the loaded candidate (or its file name) IS the query.
    fn exact(&self, target: &[char], case_sensitive: bool, file: bool, base: usize) -> bool {
        let text = if case_sensitive {
            &self.original
        } else {
            &self.lowered
        };
        *text == *target || (file && text[base..] == *target)
    }
}

// ---- the primitive -------------------------------------------------------------------

/// The options map's entries this pass reads.
struct Options {
    file: bool,
    boost: Option<MapId>,
    subset: Option<Vec<Value>>,
    indices: bool,
}

fn read_options(heap: &Heap, who: &str, options: Value) -> Result<Options, LispError> {
    let mut read = Options {
        file: false,
        boost: None,
        subset: None,
        indices: false,
    };
    let Value::Map(id) = options else {
        if matches!(options, Value::Nil) {
            return Ok(read);
        }
        return Err(LispError::wrong_type(
            heap,
            who,
            "options map or nil",
            options,
        ));
    };
    read.file = matches!(
        heap.map_get(id, value::kw("category")),
        Some(Value::Keyword(name)) if name == value::intern("file")
    );
    read.boost = match heap.map_get(id, value::kw("boost")) {
        None | Some(Value::Nil) => None,
        Some(Value::Map(boost)) => Some(boost),
        Some(other) => return Err(LispError::wrong_type(heap, who, ":boost map", other)),
    };
    read.subset = match heap.map_get(id, value::kw("subset")) {
        None | Some(Value::Nil) => None,
        Some(subset) => Some(
            heap.list_or_vector_items(subset)
                .ok_or_else(|| LispError::wrong_type(heap, who, ":subset indices", subset))?,
        ),
    };
    read.indices = heap
        .map_get(id, value::kw("indices"))
        .is_some_and(crate::eval::truthy);
    Ok(read)
}

/// `(%fuzzy-top query cands &optional limit options)` — see the registration.
fn fuzzy_top(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let who = "%fuzzy-top";
    let query_chars: Vec<char> = expect_string_ref(heap, who, arg(args, 0))?
        .chars()
        .collect();
    // `fuzzy-pieces` then `fuzzy-parse-term`: a space separates terms (ADR-429), empty
    // pieces are dropped, and a piece that asks nothing is not a term.
    let pieces: Vec<&[char]> = query_chars
        .split(|&c| c == ' ')
        .filter(|piece| !piece.is_empty())
        .collect();
    let terms: Vec<Term> = pieces
        .iter()
        .filter_map(|piece| parse_term(piece))
        .collect();
    let cands_value = arg(args, 1);
    let Some(cands) = heap.list_or_vector_items(cands_value) else {
        return Err(LispError::wrong_type(
            heap,
            who,
            "vector or list of strings",
            cands_value,
        ));
    };
    let limit = match args.get(2).copied().unwrap_or(Value::nil()) {
        Value::Nil => 0,
        v => expect_int(heap, who, v)?.max(0) as usize,
    };
    let options = read_options(heap, who, args.get(3).copied().unwrap_or(Value::nil()))?;
    // A query with no terms matches everything at score 0 and keeps the given order, which
    // is what the Brood `top` answers without ranking at all. Mirrored here so the
    // primitive is usable on its own; there is nothing to narrow from, so no indices.
    if terms.is_empty() {
        let kept = if limit > 0 && limit < cands.len() {
            cands[..limit].to_vec()
        } else {
            cands
        };
        let hits = heap.list(kept);
        return Ok(if options.indices {
            heap.alloc_vector2(hits, Value::nil())
        } else {
            hits
        });
    }
    let (exact, exact_case) = exact_target(&pieces);

    // The candidates to visit: a session's subset (what its previous query matched), else
    // all of them.
    let visit: Vec<usize> = match &options.subset {
        Some(subset) => subset
            .iter()
            .map(|&index| match expect_int(heap, who, index)? {
                i if i >= 0 && (i as usize) < cands.len() => Ok(i as usize),
                _ => Err(LispError::wrong_type(
                    heap,
                    who,
                    "an index into cands",
                    index,
                )),
            })
            .collect::<Result<_, _>>()?,
        None => (0..cands.len()).collect(),
    };

    // `[score, segments, length, index]` per match. The candidate rides as its index, so the
    // ordering never copies a string and the result hands back the caller's own values.
    let mut ranked: Vec<(i64, usize, usize, usize)> = Vec::new();
    let mut scratch = Scratch::default();
    for index in visit {
        let cand = cands[index];
        let base = scratch.load(expect_string_ref(heap, who, cand)?);
        let Some(mut score) = scratch.score(&terms, options.file, base) else {
            continue;
        };
        if let Some(boost) = options.boost
            && let Some(extra) = heap.map_get(boost, cand)
        {
            score += expect_int(heap, who, extra)?;
        }
        if scratch.exact(&exact, exact_case, options.file, base) {
            score += EXACT_BONUS;
        }
        let segments = if options.file {
            scratch.original.iter().filter(|&&c| c == '/').count()
        } else {
            0
        };
        ranked.push((score, segments, scratch.original.len(), index));
    }
    let matched: Option<Vec<Value>> = options.indices.then(|| {
        ranked
            .iter()
            .map(|&(_, _, _, index)| Value::int(index as i64))
            .collect()
    });

    // Best score first, then fewer segments, then the shorter candidate, then
    // lexicographic — `fuzzy-key`.
    let order = |heap: &Heap, a: &(i64, usize, usize, usize), b: &(i64, usize, usize, usize)| {
        b.0.cmp(&a.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then_with(|| {
                let (left, right) = (cands[a.3], cands[b.3]);
                match (left.unpack(), right.unpack()) {
                    (ValueRef::Str(l), ValueRef::Str(r)) => heap.string(l).cmp(heap.string(r)),
                    _ => std::cmp::Ordering::Equal,
                }
            })
    };
    let keep = if limit > 0 {
        limit.min(ranked.len())
    } else {
        ranked.len()
    };
    if keep < ranked.len() {
        // Only the kept prefix needs ordering: partition once, then sort what is shown.
        ranked.select_nth_unstable_by(keep, |a, b| order(heap, a, b));
        ranked.truncate(keep);
    }
    ranked.sort_unstable_by(|a, b| order(heap, a, b));

    let kept: Vec<Value> = ranked
        .iter()
        .map(|&(_, _, _, index)| cands[index])
        .collect();
    let hits = heap.list(kept);
    Ok(match matched {
        Some(indices) => {
            let indices = heap.alloc_vector(indices);
            heap.alloc_vector2(hits, indices)
        }
        None => hits,
    })
}
