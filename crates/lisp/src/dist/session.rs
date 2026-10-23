//! The encrypted steady-state session (ADR-089) — confidentiality + per-frame
//! integrity for the node link, closing ADR-081's cleartext/injection gap.
//!
//! After the handshake authenticates the peer (cookie-HMAC) and agrees a shared
//! secret (ephemeral X25519, see `dist::handshake`), the link runs **encrypted**.
//! Every steady-state frame is sealed with **ChaCha20-Poly1305**: the 16-byte
//! Poly1305 tag *is* a per-frame MAC, so a forged frame injected after the
//! handshake — including a `Send` carrying a closure (→ RCE) — fails to open and
//! tears the link down.
//!
//! ## Why this fits the reader/writer thread split (and TLS wouldn't)
//! A live link runs two independent threads sharing an `Arc<Stream>` — a reader
//! (`&Stream: Read`) and a writer (`&Stream: Write`). A single TLS `Connection`
//! can't be driven from both (it holds shared mutable crypto state). Here each
//! **direction** has its own key + monotonic nonce counter: the writer owns a
//! [`SealKey`], the reader owns an [`OpenKey`], and they never share crypto state.
//!
//! ## Nonces
//! The nonce is a per-direction frame counter (`[0u8; 4] || counter_be_u64`).
//! Counters start at 0 and only increase; the two directions use *different*
//! keys, so every `(key, nonce)` pair is unique across the whole session — no
//! reuse, ever. A reordered or replayed frame decrypts under the wrong counter
//! and fails the tag check, so TCP's in-order delivery is enforced cryptographically
//! too. (Counter exhaustion at 2^64 frames is unreachable, but we error rather
//! than wrap — a wrap would reuse a nonce.)

use std::io::{self, Read};

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};

use super::MAX_FRAME;
use super::wire::{Frame, decode_frame};

/// AEAD key length (ChaCha20-Poly1305 / X25519 shared-secret derived).
pub(super) const KEY_LEN: usize = 32;

/// An authenticated link's encrypted session: the two directional ciphers from
/// the handshake's key agreement (ADR-089). `dist::establish` moves `send` into
/// the writer thread and `recv` into the reader thread, so neither shares crypto
/// state — the property that lets a per-direction AEAD fit the reader/writer split.
pub(super) struct Session {
    pub(super) send: SealKey,
    pub(super) recv: OpenKey,
}
/// Poly1305 authentication tag length appended to each ciphertext.
const TAG_LEN: usize = 16;

/// The 12-byte nonce for frame number `counter` in one direction: four zero bytes
/// then the big-endian counter. Distinct per direction because the keys differ.
fn nonce_bytes(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// The send half of a session: seals outbound plaintext payloads. Owned by the
/// writer thread, so its counter needs no synchronisation.
pub(super) struct SealKey {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl SealKey {
    pub(super) fn new(key: [u8; KEY_LEN]) -> Self {
        SealKey {
            cipher: ChaCha20Poly1305::new_from_slice(&key).expect("32-byte AEAD key"),
            counter: 0,
        }
    }

    /// Seal one frame `payload` (the bare bytes from `wire::encode_payload`) into a
    /// ready-to-write `[u32 len][ciphertext+tag]` blob, advancing the nonce counter.
    #[cfg(test)]
    pub(super) fn seal(&mut self, payload: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.seal_into(payload, &mut out)?;
        Ok(out)
    }

    /// [`seal`](Self::seal), APPENDED to `out` — the writer thread seals a run of queued
    /// frames into one reused buffer and writes them together.
    ///
    /// Encrypted in place: the payload is copied once, into `out` after a reserved length
    /// prefix, encrypted there, and the tag appended. The bytes are exactly what
    /// `Aead::encrypt` + `wire::len_prefixed` produced (that `encrypt` is this same
    /// in-place encryption with empty associated data and the tag after the ciphertext),
    /// minus its two allocations and the second copy — so the wire format is unchanged.
    /// On failure `out` is left as it was, so frames already sealed into it are intact.
    pub(super) fn seal_into(&mut self, payload: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let start = out.len();
        out.reserve(4 + payload.len() + TAG_LEN);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(payload);
        let nonce = nonce_bytes(self.counter);
        let sealed = self
            .cipher
            .encrypt_inout_detached(&Nonce::from(nonce), b"", (&mut out[start + 4..]).into())
            .map_err(|_| io::Error::other("frame encryption failed"))
            .and_then(|tag| {
                let counter = self
                    .counter
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("session nonce space exhausted"))?;
                Ok((tag, counter))
            });
        let (tag, counter) = match sealed {
            Ok(sealed) => sealed,
            Err(error) => {
                out.truncate(start);
                return Err(error);
            }
        };
        self.counter = counter;
        out.extend_from_slice(&tag);
        // `payload` was already capped at MAX_FRAME by `encode_payload`, so the body
        // (payload + 16) fits a u32 comfortably. The `[u32 len][…]` framing is shared with
        // the plaintext path (`wire::len_prefixed`); only the body differs (ciphertext+tag
        // here, plaintext there).
        let body_len = (payload.len() + TAG_LEN) as u32;
        out[start..start + 4].copy_from_slice(&body_len.to_be_bytes());
        Ok(())
    }
}

/// The receive half of a session: reads + opens inbound sealed frames. Owned by
/// the reader thread, so its counter needs no synchronisation.
pub(super) struct OpenKey {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl OpenKey {
    pub(super) fn new(key: [u8; KEY_LEN]) -> Self {
        OpenKey {
            cipher: ChaCha20Poly1305::new_from_slice(&key).expect("32-byte AEAD key"),
            counter: 0,
        }
    }

    /// Read one sealed frame from `r`, authenticate + decrypt it, and decode the
    /// `Frame`. A tag failure — a tampered, forged, replayed, or reordered frame —
    /// surfaces as an `io::Error`, so the steady-state reader's
    /// `while let Ok(frame) = open.open(..)` loop tears the link down on any
    /// integrity violation. This is the gate that closes ADR-081's
    /// post-handshake-injection (RCE-by-forged-closure) hole.
    pub(super) fn open(&mut self, r: &mut impl Read) -> io::Result<Frame> {
        let mut len = [0u8; 4];
        r.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        // A sealed frame is `plaintext + TAG_LEN`; reject an over-large prefix
        // before allocating (mirrors the plaintext `read_frame_capped` ceiling).
        if !(TAG_LEN..=MAX_FRAME + TAG_LEN).contains(&len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sealed frame length {len} out of range"),
            ));
        }
        let mut sealed = read_claimed(r, len)?;
        let nonce = nonce_bytes(self.counter);
        // Decrypted IN PLACE (what `Aead::decrypt` does on a copy it allocates): the
        // ciphertext is the buffer's head, the tag its last TAG_LEN bytes, and the
        // plaintext then decodes straight out of the same buffer.
        let body_len = len - TAG_LEN;
        let (body, tag) = sealed.split_at_mut(body_len);
        let tag = Tag::try_from(&*tag).expect("the range check leaves TAG_LEN bytes");
        self.cipher
            .decrypt_inout_detached(&Nonce::from(nonce), b"", body.into(), &tag)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame authentication failed (tampered, forged, replayed, or reordered)",
                )
            })?;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session nonce space exhausted"))?;
        sealed.truncate(body_len);
        decode_frame(&mut std::io::Cursor::new(sealed))
    }
}

/// Largest slice this reads in one go while collecting a frame body.
///
/// The buffer grows a chunk at a time *as bytes actually arrive*, so a peer's claimed
/// length never becomes an allocation on its own.
const READ_CHUNK: usize = 64 * 1024;

/// Read exactly `len` bytes **without trusting `len` for the allocation**.
///
/// `vec![0u8; len]` was the obvious spelling and the wrong one: `len` comes off the wire,
/// and the Poly1305 tag that proves the frame genuine is inside the bytes we have not read
/// yet — so the allocation happens strictly *before* anything about this frame is
/// authenticated. A peer needed only to send a 4-byte prefix claiming `MAX_FRAME` to make
/// us commit **64 MiB**, then stall or drop; repeated across links that is memory
/// amplification of ~16 million to one, from traffic too small to look like an attack.
/// (The peer is cookie-authenticated by then, so this is hardening rather than a hole —
/// but "authenticated" is not "trusted with the allocator". KI-97 item 4.)
///
/// Growing per chunk makes the cost proportional to bytes *delivered*: a claim of 64 MiB
/// backed by silence costs one chunk. Honest frames are unaffected beyond a few `realloc`s
/// on the way up, and the ceiling is still enforced by the caller's range check.
fn read_claimed(r: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(len.min(READ_CHUNK));
    while buf.len() < len {
        let want = (len - buf.len()).min(READ_CHUNK);
        let start = buf.len();
        buf.resize(start + want, 0);
        // A short read here (peer stalled or went away) leaves the buffer at its current
        // size and propagates — nothing larger was ever reserved.
        r.read_exact(&mut buf[start..])?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    /// KI-97 item 4: a claimed length must not become an allocation.
    ///
    /// `session::open` read its body with `vec![0u8; len]`, where `len` is four bytes off
    /// the wire and the Poly1305 tag proving the frame genuine sits in the bytes not yet
    /// read. A peer could therefore spend 4 bytes to make us commit **64 MiB** and then
    /// stall — memory amplification of ~16 million to one, repeatable per link.
    ///
    /// The assertion is on the mechanism, not on a memory measurement (which would be
    /// unreliable in-process): this reader records the largest slice it is ever asked to
    /// fill. Pre-fix that is the full claimed length in one go; post-fix it is capped at
    /// `READ_CHUNK` however large the claim.
    ///
    /// **Driven through `OpenKey::open`, not the helper.** An earlier version called
    /// `read_claimed` directly and passed happily with `open` reverted to `vec![0u8; len]`
    /// — it guarded the helper while the bug lived at the call site. Exercise the entry
    /// point, or the guard is decorative.
    #[test]
    fn a_claimed_length_is_not_an_allocation() {
        struct Recorder {
            biggest: usize,
            budget: usize,
            prefix: Vec<u8>,
        }
        impl Read for Recorder {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                // The 4-byte length prefix first, verbatim; only the BODY reads are the
                // ones whose size this is measuring.
                if !self.prefix.is_empty() {
                    let n = buf.len().min(self.prefix.len());
                    buf[..n].copy_from_slice(&self.prefix[..n]);
                    self.prefix.drain(..n);
                    return Ok(n);
                }
                self.biggest = self.biggest.max(buf.len());
                if self.budget == 0 {
                    return Ok(0); // peer went away mid-frame — what the attack relies on
                }
                let n = buf.len().min(self.budget);
                self.budget -= n;
                Ok(n)
            }
        }

        // Claim the largest body the range check admits, then deliver almost nothing.
        let claim = MAX_FRAME + TAG_LEN;
        let mut peer = Recorder {
            biggest: 0,
            budget: 128,
            prefix: (claim as u32).to_be_bytes().to_vec(),
        };
        let mut key = OpenKey::new([7u8; KEY_LEN]);
        // `Frame` has no `Debug`, so match rather than `expect_err`.
        let err = match key.open(&mut peer) {
            Err(e) => e,
            Ok(_) => panic!("a peer that stops mid-frame must not yield a frame"),
        };
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            peer.biggest <= READ_CHUNK,
            "open() asked to fill {} bytes for a claim of {claim} — the claim became an \
             allocation, which is the bug",
            peer.biggest
        );
    }

    /// The complement: an honest frame of any size still round-trips byte-for-byte, so the
    /// chunking cannot have introduced a truncation or a reordering.
    #[test]
    fn chunked_reads_still_deliver_the_whole_body() {
        let body: Vec<u8> = (0..(READ_CHUNK * 2 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut cursor = io::Cursor::new(body.clone());
        let got = read_claimed(&mut cursor, body.len()).expect("honest frame");
        assert_eq!(got, body, "a multi-chunk body must come back exactly");
    }

    use super::*;
    use std::io::Cursor;

    fn key(b: u8) -> [u8; KEY_LEN] {
        [b; KEY_LEN]
    }

    /// A frame the tests can seal and recognise on the way back out.
    fn monitor(mref: u64) -> Frame {
        Frame::Monitor {
            watcher_pid: 7,
            target: 42,
            mref,
        }
    }

    fn payload(f: &Frame) -> Vec<u8> {
        super::super::wire::encode_payload(f).unwrap()
    }

    /// A sealed pair built from the same key round-trips a stream of frames in
    /// order — the happy path the link runs on.
    #[test]
    fn seal_open_roundtrips_in_order() {
        let mut seal = SealKey::new(key(1));
        let mut open = OpenKey::new(key(1));
        let mut wire = Vec::new();
        for i in 0..5u64 {
            wire.extend_from_slice(&seal.seal(&payload(&monitor(i))).unwrap());
        }
        let mut r = Cursor::new(wire);
        for i in 0..5u64 {
            match open.open(&mut r).unwrap() {
                Frame::Monitor { mref, target, .. } => {
                    assert_eq!(mref, i);
                    assert_eq!(target, 42);
                }
                _ => panic!("wrong frame"),
            }
        }
    }

    /// Flipping a single ciphertext byte makes the frame fail to open — the
    /// per-frame integrity guarantee that closes the injection hole (ADR-081 #1).
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut seal = SealKey::new(key(2));
        let mut framed = seal.seal(&payload(&monitor(1))).unwrap();
        let last = framed.len() - 1; // inside the ciphertext+tag, past the 4-byte len
        framed[last] ^= 0xff;
        let mut open = OpenKey::new(key(2));
        assert!(open.open(&mut Cursor::new(framed)).is_err());
    }

    /// A reordered (or replayed) frame decrypts under the wrong counter and is
    /// rejected, so an attacker can't reorder or replay captured frames.
    #[test]
    fn reordered_and_replayed_frames_are_rejected() {
        let mut seal = SealKey::new(key(3));
        let a = seal.seal(&payload(&monitor(10))).unwrap(); // counter 0
        let b = seal.seal(&payload(&monitor(11))).unwrap(); // counter 1

        // Feeding B first (sealed at counter 1) to a fresh OpenKey at counter 0 fails.
        let mut open = OpenKey::new(key(3));
        assert!(open.open(&mut Cursor::new(b)).is_err(), "reorder must fail");

        // In order, A opens; replaying A again (now counter 1) fails.
        let mut open = OpenKey::new(key(3));
        assert!(open.open(&mut Cursor::new(a.clone())).is_ok());
        assert!(open.open(&mut Cursor::new(a)).is_err(), "replay must fail");
    }

    /// The opposite direction's key can't open a frame — the two directions are
    /// cryptographically separated.
    #[test]
    fn wrong_direction_key_cannot_open() {
        let mut seal = SealKey::new(key(4));
        let framed = seal.seal(&payload(&monitor(1))).unwrap();
        let mut open = OpenKey::new(key(5)); // different key
        assert!(open.open(&mut Cursor::new(framed)).is_err());
    }

    /// Sealing the same plaintext twice yields different ciphertext (the counter
    /// advanced), proving nonces aren't reused.
    #[test]
    fn counter_advances_so_nonces_never_repeat() {
        let mut seal = SealKey::new(key(6));
        let p = payload(&monitor(1));
        let first = seal.seal(&p).unwrap();
        let second = seal.seal(&p).unwrap();
        assert_ne!(first, second);
    }
}
