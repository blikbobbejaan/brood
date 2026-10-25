//! Public-key cryptography beyond ed25519 (ADR-448): ECDSA over P-256 / P-384 and RSA (PKCS#1
//! v1.5 and PSS) signing and verification, X25519 key agreement, and Argon2id password
//! hashing. Rust *mechanism* only — raw bytes in, raw `bytes` out; the key maps, string
//! messages and algorithm names a program uses are Brood policy in `std/crypto.blsp`.
//!
//! ECDSA and RSA are `aws-lc-rs`, the provider the TLS stack already links on every native
//! target, so they add no crate; it has no wasm target, so there they raise. X25519 is the
//! `x25519-dalek` the distribution handshake already uses, and Argon2id the pure-Rust
//! RustCrypto `argon2`: both work everywhere.

use crate::core::heap::Heap;
use crate::core::value::{EnvId, Value};
use crate::error::{LispError, LispResult};

use super::args::arg;
use super::bytes::{bytes_to_value, collect_bytes};

/// Every primitive this file contributes: name, arity, signature, arglist, docstring.
pub(super) fn register(primitives: &mut super::Primitives) {
    use super::signature_types::*;
    use crate::core::value::{Arity, Tag};
    use crate::types::{Sig, Ty};
    let boolean = Ty::of(Tag::Bool);
    let vector = Ty::of(Tag::Vector);
    primitives.def(
        "%ecdsa-keygen",
        Arity::exact(1),
        Sig::new(vec![kw], vector.clone()),
        &["curve"],
        "A fresh ECDSA keypair on `curve` (:p256 or :p384) as a [public private] vector: public the uncompressed SEC1 point (65 / 97 bytes), private a PKCS#8 document, both bytes values. Native targets only.",
        ecdsa_keygen,
    );
    primitives.def(
        "%ecdsa-sign",
        Arity::exact(3),
        Sig::new(vec![kw, any, any], bytes_ty),
        &["curve", "private-pkcs8", "message-bytes"],
        "The ECDSA signature of message-bytes (hashed with SHA-256 on :p256, SHA-384 on :p384) under the PKCS#8 private key, in the fixed r‖s form JOSE uses (64 / 96 bytes). Errors when the key is not a key on that curve.",
        ecdsa_sign,
    );
    primitives.def(
        "%ecdsa-verify",
        Arity::exact(4),
        Sig::new(vec![kw, any, any, any], boolean.clone()),
        &["curve", "public-point", "message-bytes", "signature-bytes"],
        "true when signature-bytes (fixed r‖s) is a valid ECDSA signature of message-bytes under the uncompressed SEC1 public point on `curve`, else false. Never errors on a bad key or signature.",
        ecdsa_verify,
    );
    primitives.def(
        "%rsa-keygen",
        Arity::exact(1),
        Sig::new(vec![int], vector.clone()),
        &["bits"],
        "A fresh RSA key of `bits` (2048, 3072, 4096 or 8192) as a [n e private] vector: the big-endian modulus and public exponent, and a PKCS#8 private key, all bytes values. Native targets only.",
        rsa_keygen,
    );
    primitives.def(
        "%rsa-sign",
        Arity::exact(3),
        Sig::new(vec![kw, any, any], bytes_ty),
        &["scheme", "private-pkcs8", "message-bytes"],
        "The RSA signature of message-bytes under the PKCS#8 private key, by `scheme`: :pkcs1-sha256 :pkcs1-sha384 :pkcs1-sha512 (JWT RS*) or :pss-sha256 :pss-sha384 :pss-sha512 (PS*). As long as the modulus.",
        rsa_sign,
    );
    primitives.def(
        "%rsa-verify",
        Arity::exact(5),
        Sig::new(vec![kw, any, any, any, any], boolean.clone()),
        &["scheme", "n", "e", "message-bytes", "signature-bytes"],
        "true when signature-bytes is a valid RSA signature of message-bytes by `scheme` under the public key given as its big-endian modulus `n` and exponent `e` (a JWK's \"n\" and \"e\"), else false. Keys of 2048 to 8192 bits. Never errors on a bad key or signature.",
        rsa_verify,
    );
    primitives.def(
        "%x25519-keygen",
        Arity::exact(0),
        Sig::new(vec![], vector),
        &[],
        "A fresh X25519 keypair as a [public private] vector of 32-byte bytes values.",
        x25519_keygen,
    );
    primitives.def(
        "%x25519-shared",
        Arity::exact(2),
        Sig::new(vec![any, any], bytes_ty),
        &["private-bytes", "their-public-bytes"],
        "The 32-byte X25519 shared secret of our 32-byte private key and their 32-byte public key. Feed it to a KDF (HKDF), never use it as a key directly. Errors when either is not 32 bytes.",
        x25519_shared,
    );
    primitives.def(
        "%argon2id-hash",
        Arity::exact(1),
        Sig::new(vec![any], string),
        &["password-bytes"],
        "An Argon2id hash of password-bytes with a fresh random salt and the crate's default (OWASP) cost, as a PHC string (\"$argon2id$v=19$m=…\") that carries its own salt and parameters — store it whole.",
        argon2id_hash,
    );
    primitives.def(
        "%argon2id-verify",
        Arity::exact(2),
        Sig::new(vec![string, any], boolean),
        &["phc-string", "password-bytes"],
        "true when password-bytes hashes to the PHC string (as `%argon2id-hash` wrote it, or any Argon2 PHC string), else false — including for a string that is no PHC hash.",
        argon2id_verify,
    );
}

/// The keyword argument `name` of `function` as a string, or an error naming what was wanted.
#[cfg(not(target_arch = "wasm32"))]
fn keyword_arg(
    function: &str,
    args: &[Value],
    index: usize,
    wanted: &str,
) -> Result<String, LispError> {
    match arg(args, index) {
        Value::Keyword(sym) => Ok(crate::core::value::symbol_name(sym)),
        _ => Err(LispError::runtime(format!(
            "{function}: want a keyword, one of {wanted}"
        ))),
    }
}

// ---- ECDSA and RSA: aws-lc-rs, native only -------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::super::args::expect_int;
    use super::*;
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{self, EcdsaKeyPair, KeyPair, RsaKeyPair, UnparsedPublicKey};

    fn curve(
        function: &str,
        args: &[Value],
    ) -> Result<
        (
            &'static signature::EcdsaSigningAlgorithm,
            &'static signature::EcdsaVerificationAlgorithm,
        ),
        LispError,
    > {
        match keyword_arg(function, args, 0, ":p256 :p384")?.as_str() {
            "p256" => Ok((
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &signature::ECDSA_P256_SHA256_FIXED,
            )),
            "p384" => Ok((
                &signature::ECDSA_P384_SHA384_FIXED_SIGNING,
                &signature::ECDSA_P384_SHA384_FIXED,
            )),
            other => Err(LispError::runtime(format!(
                "{function}: unknown curve :{other} (want :p256 :p384)"
            ))),
        }
    }

    pub(super) fn ecdsa_keygen(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
        let (signing, _) = curve("%ecdsa-keygen", args)?;
        let failed = |_| LispError::runtime("%ecdsa-keygen: key generation failed");
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(signing, &SystemRandom::new()).map_err(failed)?;
        let pair = EcdsaKeyPair::from_pkcs8(signing, pkcs8.as_ref())
            .map_err(|_| LispError::runtime("%ecdsa-keygen: key generation failed"))?;
        let public = bytes_to_value(pair.public_key().as_ref(), heap);
        let private = bytes_to_value(pkcs8.as_ref(), heap);
        Ok(heap.alloc_vector2(public, private))
    }

    pub(super) fn ecdsa_sign(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
        let (signing, _) = curve("%ecdsa-sign", args)?;
        let pkcs8 = collect_bytes("%ecdsa-sign", arg(args, 1), heap)?;
        let message = collect_bytes("%ecdsa-sign", arg(args, 2), heap)?;
        let pair = EcdsaKeyPair::from_pkcs8(signing, &pkcs8).map_err(|e| {
            LispError::runtime(format!("%ecdsa-sign: not a PKCS#8 key on that curve ({e})"))
        })?;
        let signed = pair
            .sign(&SystemRandom::new(), &message)
            .map_err(|_| LispError::runtime("%ecdsa-sign: signing failed"))?;
        Ok(bytes_to_value(signed.as_ref(), heap))
    }

    pub(super) fn ecdsa_verify(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
        let (_, verifying) = curve("%ecdsa-verify", args)?;
        let public = collect_bytes("%ecdsa-verify", arg(args, 1), heap)?;
        let message = collect_bytes("%ecdsa-verify", arg(args, 2), heap)?;
        let signed = collect_bytes("%ecdsa-verify", arg(args, 3), heap)?;
        let ok = UnparsedPublicKey::new(verifying, &public)
            .verify(&message, &signed)
            .is_ok();
        Ok(Value::Bool(ok))
    }

    pub(super) fn rsa_keygen(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
        let size = match expect_int(heap, "%rsa-keygen", arg(args, 0))? {
            2048 => KeySize::Rsa2048,
            3072 => KeySize::Rsa3072,
            4096 => KeySize::Rsa4096,
            8192 => KeySize::Rsa8192,
            other => {
                return Err(LispError::runtime(format!(
                    "%rsa-keygen: {other} bits (want 2048, 3072, 4096 or 8192)"
                )));
            }
        };
        let failed = || LispError::runtime("%rsa-keygen: key generation failed");
        let pair = RsaKeyPair::generate(size).map_err(|_| failed())?;
        let pkcs8 = pair.as_der().map_err(|_| failed())?;
        let (n, e) = rsa_public_integers(pair.public_key().as_ref()).ok_or_else(failed)?;
        let n = bytes_to_value(n, heap);
        let e = bytes_to_value(e, heap);
        let private = bytes_to_value(pkcs8.as_ref(), heap);
        Ok(heap.alloc_vector(vec![n, e, private]))
    }

    /// `n` and `e` of a PKCS#1 `RSAPublicKey` (`SEQUENCE { INTEGER, INTEGER }`, DER), each
    /// big-endian without the sign byte DER adds before a high bit. The key's own accessors
    /// sit behind aws-lc-rs's `ring-io` feature, which would pull a second `untrusted` crate
    /// in for two integers.
    fn rsa_public_integers(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        // one TLV at `at`: (tag, value range)
        let element = |at: usize| -> Option<(u8, std::ops::Range<usize>)> {
            let tag = *der.get(at)?;
            let first = *der.get(at + 1)? as usize;
            let (length, header) = if first < 0x80 {
                (first, 2)
            } else {
                let width = first & 0x7f;
                let bytes = der.get(at + 2..at + 2 + width)?;
                (
                    bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize),
                    2 + width,
                )
            };
            let start = at + header;
            Some((tag, start..start.checked_add(length)?))
        };
        let (0x30, body) = element(0)? else {
            return None;
        };
        let (0x02, n) = element(body.start)? else {
            return None;
        };
        let (0x02, e) = element(n.end)? else {
            return None;
        };
        let unsigned = |range: std::ops::Range<usize>| -> Option<Vec<u8>> {
            let bytes = der.get(range)?;
            let skip = bytes
                .iter()
                .take_while(|b| **b == 0)
                .count()
                .min(bytes.len() - 1);
            Some(bytes[skip..].to_vec())
        };
        Some((unsigned(n)?, unsigned(e)?))
    }

    fn rsa_scheme(
        function: &str,
        args: &[Value],
    ) -> Result<
        (
            &'static dyn signature::RsaEncoding,
            &'static signature::RsaParameters,
        ),
        LispError,
    > {
        let wanted =
            ":pkcs1-sha256 :pkcs1-sha384 :pkcs1-sha512 :pss-sha256 :pss-sha384 :pss-sha512";
        match keyword_arg(function, args, 0, wanted)?.as_str() {
            "pkcs1-sha256" => Ok((
                &signature::RSA_PKCS1_SHA256,
                &signature::RSA_PKCS1_2048_8192_SHA256,
            )),
            "pkcs1-sha384" => Ok((
                &signature::RSA_PKCS1_SHA384,
                &signature::RSA_PKCS1_2048_8192_SHA384,
            )),
            "pkcs1-sha512" => Ok((
                &signature::RSA_PKCS1_SHA512,
                &signature::RSA_PKCS1_2048_8192_SHA512,
            )),
            "pss-sha256" => Ok((
                &signature::RSA_PSS_SHA256,
                &signature::RSA_PSS_2048_8192_SHA256,
            )),
            "pss-sha384" => Ok((
                &signature::RSA_PSS_SHA384,
                &signature::RSA_PSS_2048_8192_SHA384,
            )),
            "pss-sha512" => Ok((
                &signature::RSA_PSS_SHA512,
                &signature::RSA_PSS_2048_8192_SHA512,
            )),
            other => Err(LispError::runtime(format!(
                "{function}: unknown scheme :{other} (want {wanted})"
            ))),
        }
    }

    pub(super) fn rsa_sign(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
        let (encoding, _) = rsa_scheme("%rsa-sign", args)?;
        let pkcs8 = collect_bytes("%rsa-sign", arg(args, 1), heap)?;
        let message = collect_bytes("%rsa-sign", arg(args, 2), heap)?;
        let pair = RsaKeyPair::from_pkcs8(&pkcs8)
            .map_err(|e| LispError::runtime(format!("%rsa-sign: not a PKCS#8 RSA key ({e})")))?;
        let mut signed = vec![0u8; pair.public_modulus_len()];
        pair.sign(encoding, &SystemRandom::new(), &message, &mut signed)
            .map_err(|_| LispError::runtime("%rsa-sign: signing failed"))?;
        Ok(bytes_to_value(signed, heap))
    }

    pub(super) fn rsa_verify(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
        let (_, parameters) = rsa_scheme("%rsa-verify", args)?;
        let n = collect_bytes("%rsa-verify", arg(args, 1), heap)?;
        let e = collect_bytes("%rsa-verify", arg(args, 2), heap)?;
        let message = collect_bytes("%rsa-verify", arg(args, 3), heap)?;
        let signed = collect_bytes("%rsa-verify", arg(args, 4), heap)?;
        let ok = aws_lc_rs::rsa::PublicKeyComponents {
            n: &n[..],
            e: &e[..],
        }
        .verify(parameters, &message, &signed)
        .is_ok();
        Ok(Value::Bool(ok))
    }
}

#[cfg(target_arch = "wasm32")]
mod native {
    use super::*;

    fn unavailable(name: &str) -> LispResult {
        Err(LispError::runtime(format!(
            "{name}: ECDSA and RSA are not available on wasm32 (no aws-lc-rs there)"
        )))
    }

    pub(super) fn ecdsa_keygen(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
        unavailable("%ecdsa-keygen")
    }
    pub(super) fn ecdsa_sign(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
        unavailable("%ecdsa-sign")
    }
    pub(super) fn ecdsa_verify(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
        unavailable("%ecdsa-verify")
    }
    pub(super) fn rsa_keygen(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
        unavailable("%rsa-keygen")
    }
    pub(super) fn rsa_sign(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
        unavailable("%rsa-sign")
    }
    pub(super) fn rsa_verify(_: &[Value], _: EnvId, _: &mut Heap) -> LispResult {
        unavailable("%rsa-verify")
    }
}

use native::{ecdsa_keygen, ecdsa_sign, ecdsa_verify, rsa_keygen, rsa_sign, rsa_verify};

// ---- X25519 and Argon2id: pure Rust, every target --------------------------------------

fn x25519_keygen(_: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret)
        .map_err(|e| LispError::runtime(format!("%x25519-keygen: OS RNG unavailable: {e}")))?;
    let private = x25519_dalek::StaticSecret::from(secret);
    let public = x25519_dalek::PublicKey::from(&private);
    let public_value = bytes_to_value(public.to_bytes(), heap);
    let private_value = bytes_to_value(private.to_bytes(), heap);
    Ok(heap.alloc_vector2(public_value, private_value))
}

fn x25519_shared(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    let thirty_two = |what: &str, bytes: Vec<u8>| -> Result<[u8; 32], LispError> {
        bytes.as_slice().try_into().map_err(|_| {
            LispError::runtime(format!(
                "%x25519-shared: {what} must be 32 bytes, got {}",
                bytes.len()
            ))
        })
    };
    let private = thirty_two(
        "the private key",
        collect_bytes("%x25519-shared", arg(args, 0), heap)?,
    )?;
    let public = thirty_two(
        "their public key",
        collect_bytes("%x25519-shared", arg(args, 1), heap)?,
    )?;
    let shared = x25519_dalek::StaticSecret::from(private)
        .diffie_hellman(&x25519_dalek::PublicKey::from(public));
    Ok(bytes_to_value(shared.to_bytes(), heap))
}

fn argon2id_hash(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    use argon2::{PasswordHash, PasswordHasher};
    let password = collect_bytes("%argon2id-hash", arg(args, 0), heap)?;
    // a 16-byte salt from the OS RNG, chosen by the crate
    let hashed: PasswordHash = argon2::Argon2::default()
        .hash_password(&password)
        .map_err(|e| LispError::runtime(format!("%argon2id-hash: {e}")))?;
    Ok(heap.alloc_string_owned(hashed.to_string()))
}

fn argon2id_verify(args: &[Value], _: EnvId, heap: &mut Heap) -> LispResult {
    use argon2::{PasswordHash, PasswordVerifier};
    let phc = super::args::expect_string(heap, "%argon2id-verify", arg(args, 0))?;
    let password = collect_bytes("%argon2id-verify", arg(args, 1), heap)?;
    let ok = PasswordHash::new(&phc)
        .map(|parsed| {
            argon2::Argon2::default()
                .verify_password(&password, &parsed)
                .is_ok()
        })
        .unwrap_or(false);
    Ok(Value::Bool(ok))
}
