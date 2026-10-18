//! The slab substrate — child of heap.
//!
//! Where LOCAL and shared values physically live: [`VecStore`] (a vector's elements, inline in
//! the slab slot up to [`INLINE_VEC_CAP`] and spilled to a `Vec` past it), [`Slabs`] (one `Vec`
//! per kind — the LOCAL nursery/old generations and the frozen PRELUDE are all this shape) with
//! the live-count / capacity / park-trim helpers the collector's sizing reads, [`CodeSlabs`]
//! (the append-only `boxcar` slabs of one RUNTIME code generation). An accessor's borrow into
//! a RUNTIME generation is a plain `&T`: `Heap::code_gen_ref`'s per-process cache keeps every
//! generation it has handed out alive until `&mut self`. Fields and constructors are
//! `pub(super)` because `heap.rs`, `gc.rs` and `gc_runtime.rs` address the slabs directly;
//! `heap.rs` re-exports `VecStore` and `INLINE_VEC_CAP`.

use super::*;

/// How many elements a vector stores **inline in its slab slot** before it
/// spills to a heap `Vec` (see [`VecStore`]). Set to 2 — the hot small-vector
/// case (2-element tuples like `bintree` nodes, 2-element `SeqView` backings) —
/// kept small so an inline slot (~64 B) stays *below* the old `Vec<Value>`
/// handle-plus-`malloc` footprint (a 24 B slot + a ≥48 B heap block). A larger
/// cap would inline 3-element `Range` backings too, but at a bigger slot for
/// *every* vector, which added GC-copy traffic on the copy-bound `bintree`
/// (whose whole tree is live at once) — net CPU-negative in measurement.
pub(crate) const INLINE_VEC_CAP: usize = 2;

/// Element storage for one heap vector: **inline** in the slab slot for the
/// common small case, or **spilled** to a heap `Vec` for larger vectors. This
/// replaced a bare `Vec<Value>` per slot (`vectors: Vec<Vec<Value>>`), which
/// paid a `malloc` on *every* vector allocation and forced element reads through
/// a double indirection the JIT couldn't inline. A small vector is now a plain
/// bump-push (like a pair), and its elements sit at a fixed offset in the slot
/// so the JIT can inline the read the way it inlines a pair car/cdr.
///
/// An **enum** (not a struct with an always-present spill field), so the inline
/// and spill forms share storage and a small vector costs no more than its two
/// `Value`s plus a length. Both present as `&[Value]` through [`Deref`]/
/// [`DerefMut`], so every reader (accessors, GC, message copy, builtins) is
/// oblivious to which form backs a given vector. `#[repr(u8)]` pins the layout
/// (tag byte at 0; the `Inline` variant's `len` at 8, `items` at 16) for the
/// JIT's inline element read — see `jit_lower.rs`.
#[repr(u8)]
pub(crate) enum VecStore {
    Inline {
        len: u8,
        items: [Value; INLINE_VEC_CAP],
    },
    // Rust never reads `ptr`/`len` by name — JIT-lowered native code loads them
    // through the `#[repr(u8)]`-pinned byte offsets (see jit_lower.rs), which
    // dead_code analysis can't see.
    #[allow(dead_code)]
    Spill {
        /// Cached `vec.as_ptr()`, so the JIT reads spilled elements through one
        /// raw load instead of an FFI slab call (the ~20 ns/element that gated
        /// nbody's field reads and the json/regex code-vector scans). Sound
        /// because a spilled buffer never moves: vector contents are immutable
        /// (never pushed/resized after construction; `DerefMut` element writes
        /// don't reallocate), and moving the `VecStore` struct itself (slab
        /// growth, GC copy) moves three words — not the heap buffer they point
        /// to. A GC relocation builds a NEW store via [`VecStore::spill`], which
        /// re-derives the pointer.
        ptr: *const Value,
        /// Cached element count for the JIT's bounds check.
        len: u64,
        vec: Vec<Value>,
    },
}

// SAFETY: `Spill::ptr` always points into `Spill::vec`'s own buffer (established
// by the one constructor and re-derived on clone), so it is exactly as sendable/
// sharable as the `Vec` it caches.
unsafe impl Send for VecStore {}
unsafe impl Sync for VecStore {}

impl Clone for VecStore {
    fn clone(&self) -> Self {
        match self {
            VecStore::Inline { len, items } => VecStore::Inline {
                len: *len,
                items: *items,
            },
            // NOT derived: a derived clone would copy `ptr` — pointing the clone
            // at the ORIGINAL buffer. Re-derive from the cloned Vec.
            VecStore::Spill { vec, .. } => VecStore::spill(vec.clone()),
        }
    }
}

impl VecStore {
    /// The one `Spill` constructor: caches the buffer pointer + length.
    #[inline]
    pub(super) fn spill(vec: Vec<Value>) -> Self {
        VecStore::Spill {
            ptr: vec.as_ptr(),
            len: vec.len() as u64,
            vec,
        }
    }

    /// Wrap owned elements, inlining when they fit (no heap allocation) and
    /// spilling otherwise. Consumes `items` so the spill path is a move, not a copy.
    #[inline]
    pub(super) fn from_vec(items: Vec<Value>) -> Self {
        if items.len() <= INLINE_VEC_CAP {
            let mut inline = [Value::nil(); INLINE_VEC_CAP];
            inline[..items.len()].copy_from_slice(&items);
            VecStore::Inline {
                len: items.len() as u8,
                items: inline,
            }
        } else {
            VecStore::spill(items)
        }
    }

    /// Build from a known element count + a per-index producer, inlining without
    /// a temporary `Vec` when it fits. The GC copy path ([`flush_vector`]) uses
    /// this so relocating a small survivor allocates nothing.
    #[inline]
    pub(super) fn from_flushed(len: usize, mut producer: impl FnMut(usize) -> Value) -> Self {
        if len <= INLINE_VEC_CAP {
            let mut inline = [Value::nil(); INLINE_VEC_CAP];
            for (i, slot) in inline[..len].iter_mut().enumerate() {
                *slot = producer(i);
            }
            VecStore::Inline {
                len: len as u8,
                items: inline,
            }
        } else {
            VecStore::spill((0..len).map(producer).collect())
        }
    }

    #[inline]
    pub(super) fn as_slice(&self) -> &[Value] {
        match self {
            VecStore::Inline { len, items } => &items[..*len as usize],
            VecStore::Spill { vec, .. } => vec,
        }
    }

    #[inline]
    pub(super) fn as_mut_slice(&mut self) -> &mut [Value] {
        match self {
            VecStore::Inline { len, items } => &mut items[..*len as usize],
            VecStore::Spill { vec, .. } => vec,
        }
    }

    // ---- Byte layout for the JIT's inline element read (jit_lower.rs) ----
    // A `#[repr(u8)]` enum is laid out per RFC 2195 as a union of repr(C)
    // variant-structs, each prefixed by the u8 discriminant. So for the `Inline`
    // variant: discriminant @0, `len` @1, `items` @8 (8-aligned). The
    // discriminant of the first variant (`Inline`) is 0. `JIT_STRIDE` is the
    // slab stride. These are asserted against reality by `vecstore_jit_layout`.

    /// Slab stride: bytes per `VecStore` slot.
    #[cfg(feature = "jit")]
    pub(crate) const JIT_STRIDE: i64 = std::mem::size_of::<VecStore>() as i64;
    /// Discriminant byte offset within a slot.
    #[cfg(feature = "jit")]
    pub(crate) const JIT_TAG_OFF: i32 = 0;
    /// Discriminant value that means `Inline` (inline-readable).
    #[cfg(feature = "jit")]
    pub(crate) const JIT_INLINE_TAG: i64 = 0;
    /// `Inline.len` (u8) byte offset within a slot.
    #[cfg(feature = "jit")]
    pub(crate) const JIT_LEN_OFF: i32 = 1;
    /// `Inline.items[0]` byte offset within a slot.
    #[cfg(feature = "jit")]
    pub(crate) const JIT_ITEMS_OFF: i32 = 8;
    /// Discriminant value that means `Spill` (pointer-readable).
    #[cfg(feature = "jit")]
    pub(crate) const JIT_SPILL_TAG: i64 = 1;
    /// `Spill.ptr` byte offset within a slot (u8 tag, padded to the pointer's align).
    #[cfg(feature = "jit")]
    pub(crate) const JIT_SPILL_PTR_OFF: i32 = 8;
    /// `Spill.len` byte offset within a slot.
    #[cfg(feature = "jit")]
    pub(crate) const JIT_SPILL_LEN_OFF: i32 = 16;
}

impl std::ops::Deref for VecStore {
    type Target = [Value];
    #[inline]
    fn deref(&self) -> &[Value] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for VecStore {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        self.as_mut_slice()
    }
}

/// A slab for a kind most processes never allocate — bigints, decimals, ratios, ropes,
/// local natives: one pointer until the first push, where a `Vec` is three words. Every
/// process carries two sets of slabs (nursery and old generation), so five of these are
/// 80 bytes a set off the floor of a process that holds none of them (`process_floor.rs`).
/// Reads go through the slice it dereferences to, empty while unallocated.
#[allow(clippy::box_collection)] // the box is the point: one word until the first push
pub(super) struct LazyVec<T>(Option<Box<Vec<T>>>);

impl<T> Default for LazyVec<T> {
    fn default() -> Self {
        LazyVec(None)
    }
}

impl<T> LazyVec<T> {
    pub(super) fn with_capacity(n: usize) -> Self {
        LazyVec((n > 0).then(|| Box::new(Vec::with_capacity(n))))
    }

    #[inline]
    pub(super) fn push(&mut self, value: T) {
        self.0.get_or_insert_with(Default::default).push(value);
    }

    pub(super) fn capacity(&self) -> usize {
        self.0.as_ref().map_or(0, |v| v.capacity())
    }

    pub(super) fn truncate(&mut self, len: usize) {
        if let Some(v) = self.0.as_mut() {
            v.truncate(len);
        }
    }

    /// Releases the box too once empty, so a process that allocated one bigint long ago
    /// goes back to costing one pointer.
    pub(super) fn shrink_to_fit(&mut self) {
        match self.0.as_mut() {
            Some(v) if v.is_empty() => self.0 = None,
            Some(v) => v.shrink_to_fit(),
            None => {}
        }
    }
}

impl<T> std::ops::Deref for LazyVec<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.0.as_deref().map_or(&[], |v| v.as_slice())
    }
}

impl<T> std::ops::DerefMut for LazyVec<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        match self.0.as_deref_mut() {
            Some(v) => v.as_mut_slice(),
            None => &mut [],
        }
    }
}

/// The slabs holding heap objects in the LOCAL data heap and the PRELUDE region.
#[derive(Default)]
pub(super) struct Slabs {
    pub(super) pairs: Vec<(Value, Value)>,
    pub(super) vectors: Vec<VecStore>,
    /// Maps as a flat slab of CHAMP nodes (ADR-040). Each [`MapNode`] is
    /// either a branch (two bitmaps + packed data/children arrays) or a
    /// max-depth collision leaf. The handle in `Value::Map(MapId)` points
    /// at the trie's *root* node; child sub-nodes live in the same slab,
    /// referenced by `MapId`. The root is the only entry-point — internal
    /// nodes are reachable only through the trie itself.
    pub(super) maps: Vec<MapNode>,
    pub(super) strings: Vec<LocalString>,
    /// Arbitrary-precision integers (the bignum leaf, mirrors `strings`). One
    /// `num_bigint::BigInt` per live value that overflowed i64; immutable, holds
    /// no `Value` children. Every entry satisfies the normalize invariant
    /// (strictly outside the i64 range) — `Heap::int_from_bigint` enforces it.
    pub(super) bigints: LazyVec<num_bigint::BigInt>,
    /// Arbitrary-precision base-10 decimals (mirrors `bigints` exactly). One
    /// `bigdecimal::BigDecimal` per live `Value::Decimal`; immutable, holds no
    /// `Value` children. Unlike `bigints` there is no normalize-into-`Int`
    /// invariant — a decimal is its own type and any value is stored as-is.
    pub(super) decimals: LazyVec<bigdecimal::BigDecimal>,
    /// Exact rationals (mirrors `decimals`). One `num_rational::BigRational` per live
    /// `Value::Ratio`; immutable, holds no `Value` children. Always reduced with a
    /// positive denominator; a denominator of 1 is demoted to `Int` at construction
    /// (`Heap::alloc_ratio`), so no entry here is ever integer-valued.
    pub(super) ratios: LazyVec<num_rational::BigRational>,
    /// **Raw bytes** — byte-clean immutable leaves, one `Arc<SharedBlob>` per live
    /// value (arbitrary bytes, never UTF-8, own slab + handle). The `Arc` is the unit
    /// of cross-process sharing (a refcount bump, not a byte copy).
    pub(super) bytes: Vec<Arc<SharedBlob>>,
    /// Text ropes (ADR-045). A `ropey::Rope` is itself `Arc`-shared internally,
    /// so this slab owns one cheap handle per live rope; cloning for an edit
    /// bumps refcounts, not bytes. Always inline (no SharedBlob split — ropes
    /// don't cross processes, so there's no cross-heap aliasing to optimise).
    pub(super) ropes: LazyVec<ropey::Rope>,
    pub(super) closures: Vec<Closure>,
    pub(super) natives: LazyVec<NativeFn>,
    pub(super) envs: Vec<EnvFrame>,
}

/// Live object count of a [`Slabs`] (`Σ slab.len()`). The collector is a moving
/// copy collector that never reuses a slot in place — survivors are relocated
/// into fresh slabs and the dead dropped wholesale — so there is no free list to
/// subtract and the slab lengths *are* the live count. Shared by both
/// [`Heap::local_live_count`] (the nursery) and [`Heap::old_live_count`] (the old
/// gen), which were identical sums. `natives` is excluded (it's never GC'd — see
/// the byte-weighted [`slab_bytes`], which does count it for footprint).
impl Slabs {
    /// How many objects the slab holding `kind` has — the bound a valid handle's index
    /// stays under.
    pub(super) fn len_of(&self, kind: crate::core::value::HandleKind) -> usize {
        use crate::core::value::HandleKind;
        match kind.slab() {
            HandleKind::Pair => self.pairs.len(),
            HandleKind::Vector => self.vectors.len(),
            HandleKind::Map => self.maps.len(),
            HandleKind::Str => self.strings.len(),
            HandleKind::BigInt => self.bigints.len(),
            HandleKind::Decimal => self.decimals.len(),
            HandleKind::Ratio => self.ratios.len(),
            HandleKind::Bytes => self.bytes.len(),
            HandleKind::Rope => self.ropes.len(),
            HandleKind::Fn => self.closures.len(),
            other => unreachable!("{} is not a slab of its own", other.name()),
        }
    }

    /// A fresh, empty `Slabs` whose per-slab `Vec`s carry the **capacity of
    /// `like`'s lengths** — the flip-side nursery allocator. A zero-capacity
    /// nursery would re-pay the full Vec-doubling ladder every cycle (each
    /// doubling memmoves everything allocated so far). The previous
    /// nursery's *lengths* are the steady-state high-water mark (each cycle
    /// allocates about as much as the last), so reserving them up front makes
    /// the next cycle's pushes copy-free while releasing the memory of any
    /// one-off spike (capacity follows the last cycle's actual use, not max).
    pub(super) fn with_capacity_like(like: &Slabs) -> Slabs {
        Slabs {
            pairs: Vec::with_capacity(like.pairs.len()),
            vectors: Vec::with_capacity(like.vectors.len()),
            maps: Vec::with_capacity(like.maps.len()),
            strings: Vec::with_capacity(like.strings.len()),
            bigints: LazyVec::with_capacity(like.bigints.len()),
            decimals: LazyVec::with_capacity(like.decimals.len()),
            ratios: LazyVec::with_capacity(like.ratios.len()),
            bytes: Vec::with_capacity(like.bytes.len()),
            ropes: LazyVec::with_capacity(like.ropes.len()),
            closures: Vec::with_capacity(like.closures.len()),
            natives: LazyVec::default(), // never GC'd, never grows here
            envs: Vec::with_capacity(like.envs.len()),
        }
    }
}

pub(super) fn slab_live_count(s: &Slabs) -> usize {
    s.pairs.len()
        + s.vectors.len()
        + s.maps.len()
        + s.strings.len()
        + s.bigints.len()
        + s.decimals.len()
        + s.ratios.len()
        + s.bytes.len()
        + s.ropes.len()
        + s.closures.len()
        + s.envs.len()
}

/// How much retained capacity must have accumulated **since the last trim** before
/// [`Heap::trim_parked`] does anything.
///
/// An absolute size threshold is wrong in both directions: a high one makes memory
/// non-monotonic in allocation (a process that allocated less can keep more), and a low
/// one makes a process that parks constantly pay a collection per park.
///
/// Growth-since-last-trim gets both. A responder that parks constantly reaches a steady
/// working set, so after one trim its capacity stops growing and it never trims again — it
/// pays one subtraction per park. A process that actually accumulated something trims once
/// per accumulation, regardless of how large its heap is in absolute terms.
///
/// Counted in [`park_trim_probe`] slots rather than bytes so the gate stays a few loads:
/// 64 slots is roughly a few KiB of pairs.
///
/// **And proportional above that** ([`park_trim_due`]): the growth must also reach half
/// the last trim's high-water. A long-lived process that keeps ACCUMULATING — an editor's
/// UI loop, whose caches, buffers and history grow a little with every edit, and which
/// parks after every event — crossed a fresh 64-slot mark every few turns, and each time
/// paid a whole-heap collection at the park and then the slab regrowth after the shrink:
/// 28 trims in a 20-second session, which made its turns cost twice the CPU they cost
/// with collection off. Proportional, a growing heap trims a logarithmic number of times,
/// and an idle one still gives back everything beyond 1.5× what it last settled at.
pub(super) const PARK_TRIM_GROWTH_SLOTS: usize = 64;

/// Whether a parked process whose slab capacity is now `probe` should trim, having last
/// trimmed at high-water `mark` — grown by at least [`PARK_TRIM_GROWTH_SLOTS`] AND by half
/// of `mark` since.
pub(super) fn park_trim_due(probe: usize, mark: usize) -> bool {
    probe.saturating_sub(mark) >= PARK_TRIM_GROWTH_SLOTS.max(mark / 2)
}

/// Retained *capacity* of a slab set, in bytes — what the process is holding from the
/// allocator, as opposed to [`slab_bytes`]'s live contents. The two diverge sharply for a
/// process that allocated and then dropped: the `Vec`s keep their high-water capacity, and a
/// nursery flip deliberately preserves it (`Slabs::with_capacity_like`) so the next cycle
/// does not re-pay the doubling ladder. That is right for a *running* process and wrong for
/// a parked one, which may hold it for the rest of the program.
pub(super) fn slab_capacity_bytes(s: &Slabs) -> usize {
    use std::mem::size_of;
    s.pairs.capacity() * size_of::<(Value, Value)>()
        + s.vectors.capacity() * size_of::<VecStore>()
        + s.maps.capacity() * size_of::<MapNode>()
        + s.strings.capacity() * size_of::<LocalString>()
        + s.bigints.capacity() * size_of::<num_bigint::BigInt>()
        + s.decimals.capacity() * size_of::<bigdecimal::BigDecimal>()
        + s.ratios.capacity() * size_of::<num_rational::BigRational>()
        + s.bytes.capacity() * size_of::<Arc<SharedBlob>>()
        + s.ropes.capacity() * size_of::<ropey::Rope>()
        + s.closures.capacity() * size_of::<Closure>()
        + s.natives.capacity() * size_of::<NativeFn>()
        + s.envs.capacity() * size_of::<EnvFrame>()
}

/// Everything a slab set holds, in bytes: [`slab_capacity_bytes`] plus what the entries
/// own outside the slabs — spilled vector buffers, map and frame arrays, string text and
/// shared blobs, ropes. A shared blob counts in full in every heap that holds it, since
/// each holder keeps it alive. **O(entries)**, so it is for a rare question — which
/// process to stop under memory pressure ([`crate::process::pressure`]) — never a hot path.
pub(super) fn owned_bytes(s: &Slabs) -> usize {
    use std::mem::size_of;
    let vectors: usize = s
        .vectors
        .iter()
        .map(|store| match store {
            VecStore::Spill { vec, .. } => vec.capacity() * size_of::<Value>(),
            VecStore::Inline { .. } => 0,
        })
        .sum();
    let maps: usize = s
        .maps
        .iter()
        .map(|node| {
            let data = if node.data.spilled() {
                node.data.capacity() * size_of::<(Value, Value)>()
            } else {
                0
            };
            let children = if node.children.spilled() {
                node.children.capacity() * size_of::<MapId>()
            } else {
                0
            };
            data + children
        })
        .sum();
    let strings: usize = s
        .strings
        .iter()
        .map(|string| match &string.data {
            StrData::Inline(text) => text.capacity(),
            StrData::Shared(blob) => blob.len(),
        })
        .sum();
    let bytes: usize = s.bytes.iter().map(|blob| blob.len()).sum();
    let ropes: usize = s.ropes.iter().map(|rope| rope.len_bytes()).sum();
    let envs: usize = s
        .envs
        .iter()
        .map(|frame| {
            if frame.vars.spilled() {
                frame.vars.capacity() * size_of::<(Symbol, Value)>()
            } else {
                0
            }
        })
        .sum();
    slab_capacity_bytes(s) + vectors + maps + strings + bytes + ropes + envs
}

/// A **cheap** stand-in for retained capacity, in slab elements, for the park-time gate.
///
/// [`slab_capacity_bytes`] sums eleven `capacity()` fields per generation and multiplies each
/// by a size — ~25 ns, which is nothing once, and everything on a path that runs on every
/// park. `ring` parks a million times: the full sum cost it **+4.6%** while the trims it
/// gated cost nothing measurable. Three element counts from the nursery track growth just as
/// well for a heuristic (pairs, vectors and env frames are what a working set is made of),
/// and the trim itself still measures real bytes.
#[inline]
pub(super) fn park_trim_probe(s: &Slabs) -> usize {
    s.pairs.capacity() + s.vectors.capacity() + s.envs.capacity()
}

/// Hand every slab's unused capacity back to the allocator.
pub(super) fn shrink_slabs(s: &mut Slabs) {
    s.pairs.shrink_to_fit();
    s.vectors.shrink_to_fit();
    s.maps.shrink_to_fit();
    s.strings.shrink_to_fit();
    s.bigints.shrink_to_fit();
    s.decimals.shrink_to_fit();
    s.ratios.shrink_to_fit();
    s.bytes.shrink_to_fit();
    s.ropes.shrink_to_fit();
    s.closures.shrink_to_fit();
    s.natives.shrink_to_fit();
    s.envs.shrink_to_fit();
}

/// Byte-weighted footprint of a [`Slabs`] (`Σ slab.len() * size_of::<elem>`) —
/// the slab arrays themselves, not nested/shared content (inner spilled vectors,
/// string bytes, `Arc`-shared ropes/blobs). A comparative figure, not exact RSS.
/// Counts `natives` too (unlike [`slab_live_count`]). Backs [`Heap::local_bytes`].
///
/// **O(1)** — no per-element walk, because [`Heap::local_bytes`] is republished on
/// every `receive` park. A process with large spilled vectors therefore under-reports
/// `:memory` by their buffer bytes, acceptable for a comparative figure (the hard
/// memory cap uses the global allocator counter, not this).
pub(super) fn slab_bytes(s: &Slabs) -> usize {
    use std::mem::size_of;
    s.pairs.len() * size_of::<(Value, Value)>()
        + s.vectors.len() * size_of::<VecStore>()
        + s.maps.len() * size_of::<MapNode>()
        + s.strings.len() * size_of::<LocalString>()
        + s.bigints.len() * size_of::<num_bigint::BigInt>()
        + s.decimals.len() * size_of::<bigdecimal::BigDecimal>()
        + s.ratios.len() * size_of::<num_rational::BigRational>()
        + s.bytes.len() * size_of::<Arc<SharedBlob>>()
        + s.ropes.len() * size_of::<ropey::Rope>()
        + s.closures.len() * size_of::<Closure>()
        + s.natives.len() * size_of::<NativeFn>()
        + s.envs.len() * size_of::<EnvFrame>()
}

/// Append-only code slabs for the shared RUNTIME region. `boxcar::Vec` gives
/// lock-free reads that return stable references (existing elements never move
/// or free as the vector grows), so process threads read closure bodies without
/// locking while another process `def`s new code.
#[derive(Default)]
pub(super) struct CodeSlabs {
    pub(super) pairs: boxcar::Vec<(Value, Value)>,
    pub(super) vectors: boxcar::Vec<VecStore>,
    pub(super) maps: boxcar::Vec<MapNode>,
    pub(super) strings: boxcar::Vec<LocalString>,
    /// Bignums `def`'d into a global / baked as a literal into shared RUNTIME
    /// code (mirrors `strings`). Immutable, holds no handles; append-only.
    pub(super) bigints: boxcar::Vec<num_bigint::BigInt>,
    /// Decimals `def`'d into a global / baked as a literal into shared RUNTIME
    /// code (mirrors `bigints`). Immutable, holds no handles; append-only.
    pub(super) decimals: boxcar::Vec<bigdecimal::BigDecimal>,
    /// Rationals `def`'d into a global / baked as a literal into shared RUNTIME
    /// code (mirrors `decimals`). Immutable, holds no handles; append-only.
    pub(super) ratios: boxcar::Vec<num_rational::BigRational>,
    /// Raw bytes `def`'d into a global / captured by a promoted closure (mirrors
    /// `bigints`). Byte-clean `Arc<SharedBlob>`, never read as UTF-8.
    /// Append-only; the Arc is shared, not copied.
    pub(super) bytes: boxcar::Vec<Arc<SharedBlob>>,
    /// Ropes `def`'d into a global (shared read-only across this runtime's
    /// processes). A `ropey::Rope` is `Send + Sync` and immutable-by-construction
    /// here (every edit makes a fresh LOCAL rope), so sharing one by handle is
    /// sound. Append-only like the rest of this region.
    pub(super) ropes: boxcar::Vec<ropey::Rope>,
    /// `OnceLock`-wrapped so `promote` can **reserve a slot, then fill it** — the
    /// append-only `boxcar` can't write-back the way the GC's mutable slabs do, so
    /// a *cyclic* promote (a closure whose captured scope binds the closure itself,
    /// e.g. `(let (g (fn () g)) g)` or mutually-recursive `letrec` closures) would
    /// otherwise recurse forever → SIGSEGV. Reserve-then-fill lets the recursion
    /// resolve the back-edge to the reserved handle. Each cell is set exactly once
    /// before the handle is ever published, so reads (`get().unwrap()`) never race.
    pub(super) closures: boxcar::Vec<OnceLock<Closure>>,
    /// Captured environments of promoted closures. A closure defined *inside a
    /// function call* (not at top level) closes over a local scope; promoting it
    /// for sharing copies that scope here so it resolves in any process. Frozen
    /// once promoted (read-only), so append-only is sound. `OnceLock`-wrapped for
    /// the same reserve-then-fill cycle break as `closures` above.
    pub(super) envs: boxcar::Vec<OnceLock<EnvFrame>>,
}

impl CodeSlabs {
    /// True if this generation holds no code — every slab empty. Aging may only
    /// start a new generation in a slot that is empty (its previous generation
    /// fully reclaimed), so a fresh gen's handle indices can't collide with a
    /// stale generation's still-live handles (the 2-versions-max rule, ADR-091).
    pub(super) fn is_empty(&self) -> bool {
        self.pairs.count() == 0
            && self.vectors.count() == 0
            && self.maps.count() == 0
            && self.strings.count() == 0
            && self.bigints.count() == 0
            && self.decimals.count() == 0
            && self.ratios.count() == 0
            && self.bytes.count() == 0
            && self.ropes.count() == 0
            && self.closures.count() == 0
            && self.envs.count() == 0
    }
}

#[cfg(all(test, feature = "jit"))]
mod vecstore_layout_tests {
    use super::*;
    use crate::core::value::Value;

    /// Pin the byte layout the JIT hardcodes for its inline small-vector read
    /// (`jit_lower.rs`): the `Inline` discriminant is 0, and `len`/`items` sit at
    /// the advertised offsets within a slot. A `#[repr(u8)]` layout drift (e.g.
    /// bumping `INLINE_VEC_CAP` or reordering fields) fails here rather than
    /// silently miscompiling every `nth`.
    #[test]
    fn vecstore_jit_layout() {
        let v = VecStore::Inline {
            len: 2,
            items: [Value::int(7), Value::int(9)],
        };
        let base = &v as *const VecStore as usize;
        // Discriminant byte at offset 0 (repr(u8), RFC 2195).
        let tag = unsafe { *(base as *const u8) };
        assert_eq!(tag as i64, VecStore::JIT_INLINE_TAG, "Inline discriminant");
        if let VecStore::Inline { len, items } = &v {
            assert_eq!(
                len as *const u8 as usize - base,
                VecStore::JIT_LEN_OFF as usize,
                "Inline.len offset"
            );
            assert_eq!(
                items.as_ptr() as usize - base,
                VecStore::JIT_ITEMS_OFF as usize,
                "Inline.items offset"
            );
        }
        assert_eq!(
            std::mem::size_of::<VecStore>() as i64,
            VecStore::JIT_STRIDE,
            "slab stride"
        );
        // The JIT reads a Value element as 3 i64 words; the slab stride between
        // elements is `size_of::<Value>()`.
        assert_eq!(std::mem::size_of::<Value>(), 24, "Value stride");
        // Spill layout: discriminant 1 @0, cached ptr @8, cached len @16 — the
        // JIT's pointer-read path loads exactly these.
        let sp = VecStore::spill(vec![Value::int(1), Value::int(2), Value::int(3)]);
        let sbase = &sp as *const VecStore as usize;
        let stag = unsafe { *(sbase as *const u8) };
        assert_eq!(stag as i64, VecStore::JIT_SPILL_TAG, "Spill discriminant");
        if let VecStore::Spill { ptr, len, vec } = &sp {
            assert_eq!(
                ptr as *const *const Value as usize - sbase,
                VecStore::JIT_SPILL_PTR_OFF as usize,
                "Spill.ptr offset"
            );
            assert_eq!(
                len as *const u64 as usize - sbase,
                VecStore::JIT_SPILL_LEN_OFF as usize,
                "Spill.len offset"
            );
            assert_eq!(*ptr, vec.as_ptr(), "cached ptr matches the buffer");
            assert_eq!(*len as usize, vec.len(), "cached len matches");
        }
    }
}

#[cfg(test)]
mod lazy_vec_tests {
    use super::LazyVec;

    /// One pointer until used, a `Vec` while used, and one pointer again once emptied and
    /// shrunk — the property the per-process floor counts on.
    #[test]
    fn a_lazy_slab_costs_a_pointer_until_used_and_after_it_empties() {
        assert_eq!(std::mem::size_of::<LazyVec<u64>>(), 8);
        let mut v: LazyVec<u64> = LazyVec::default();
        assert!(v.is_empty() && v.0.is_none() && v.capacity() == 0);
        assert!(
            LazyVec::<u64>::with_capacity(0).0.is_none(),
            "no box for a zero reserve"
        );
        v.push(7);
        v.push(8);
        assert_eq!((v.len(), v[1]), (2, 8));
        v[0] = 9;
        assert_eq!(&v[..], &[9, 8]);
        v.truncate(0);
        v.shrink_to_fit();
        assert!(v.0.is_none(), "an emptied slab gives its box back");
    }
}

#[cfg(test)]
mod park_trim_tests {
    use super::*;

    #[test]
    fn a_small_heap_trims_after_the_fixed_minimum() {
        assert!(!park_trim_due(63, 0));
        assert!(park_trim_due(64, 0));
        assert!(park_trim_due(100 + 64, 100));
    }

    #[test]
    fn a_large_growing_heap_trims_only_after_growing_by_half() {
        // the editor's case: 1M slots of capacity, growing a little per turn
        let mark = 1_000_000;
        assert!(!park_trim_due(mark + 64, mark));
        assert!(!park_trim_due(mark + 499_999, mark));
        assert!(park_trim_due(mark + 500_000, mark));
    }

    #[test]
    fn a_steadily_growing_heap_trims_logarithmically_often() {
        // grow by 1000 slots per park from 10K to 10M: count the trims
        let (mut probe, mut mark, mut trims) = (10_000usize, 10_000usize, 0);
        while probe < 10_000_000 {
            probe += 1000;
            if park_trim_due(probe, mark) {
                trims += 1;
                mark = probe;
            }
        }
        assert!(trims < 20, "{trims} trims growing 10K → 10M");
    }
}
