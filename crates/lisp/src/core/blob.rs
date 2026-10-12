//! Shared, refcounted storage for large immutable byte blobs.
//!
//! Strings allocated in a process LOCAL heap above [`SHARED_BLOB_THRESHOLD`]
//! bytes, and every `Value::Bytes`, live in a [`SharedBlob`] behind an `Arc`
//! instead of inline. Send between processes then bumps an atomic refcount
//! instead of deep-copying the bytes; handle death (process exit, a collection
//! dropping a non-surviving slot) drops the `Arc` and frees the blob when the
//! last reference goes. An `Arc<SharedBlob>` never leaves its runtime: the dist
//! wire deep-copies the bytes.
//!
//! ADR-026 makes data immutable, so there are no cycles — a plain `Arc` is
//! sound (no `Weak`, no cycle collector).

use std::sync::Arc;

/// Strings of this size or larger are stored as a [`SharedBlob`] instead of
/// inline in the per-process slab. Below the threshold, inline storage is
/// cheaper than the atomic refcount traffic + extra indirection.
pub const SHARED_BLOB_THRESHOLD: usize = 256;

/// An immutable byte blob shared across processes within one runtime.
///
/// A blob is either **text** — built only by [`SharedBlob::from_text`], so its bytes
/// are valid UTF-8 by construction and [`as_str`](Self::as_str) can hand them out as
/// a `&str` with no check — or **raw bytes** from [`SharedBlob::new`], which holds
/// arbitrary data and has no `&str` view at all. The two are one type because both
/// cross processes the same way (an `Arc` bump), but a string slot only ever holds a
/// text blob.
pub struct SharedBlob {
    contents: Contents,
}

enum Contents {
    Text(Box<str>),
    Bytes(Box<[u8]>),
}

impl SharedBlob {
    /// A raw-bytes blob: any data, no UTF-8 guarantee (`Value::Bytes`).
    pub fn new(bytes: &[u8]) -> Arc<Self> {
        Arc::new(SharedBlob {
            contents: Contents::Bytes(bytes.into()),
        })
    }

    /// A text blob — the only constructor a string's storage may use.
    pub fn from_text(text: &str) -> Arc<Self> {
        Arc::new(SharedBlob {
            contents: Contents::Text(text.into()),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        match &self.contents {
            Contents::Text(text) => text.as_bytes(),
            Contents::Bytes(bytes) => bytes,
        }
    }

    /// The blob as text, or `None` for a raw-bytes blob.
    pub fn as_str(&self) -> Option<&str> {
        match &self.contents {
            Contents::Text(text) => Some(text),
            Contents::Bytes(_) => None,
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self.contents, Contents::Text(_))
    }

    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }
}
