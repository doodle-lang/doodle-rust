//! Deterministic value hashing for dict keys (L§4.8, §15 hook 2; S-28/S-29).
//!
//! The engine hashes with a **fixed-key SipHash-1-3** (machine-design §5): a
//! randomized or address-derived hasher is banned from every Doodle-observable
//! path (§4.1). The key is fixed and a value's hash is a pure function of its
//! *content*, so a dict's internal bucketing is reproducible. Bucketing is never
//! itself observed — a dict iterates in insertion order and resolves lookups by
//! structural `==` — so the hash only has to be deterministic, not stable across
//! builds; this is the native placeholder for the M5 `Hashable` protocol.
//!
//! **Coherence with equality (L§4.8).** `a == b` must imply `hash(a) == hash(b)`.
//! Numeric `==` is by exact value across `Int`/`BigInt`/`Float` (S-28), so a number
//! hashes by its exact mathematical value: an integer value — including an
//! integer-valued finite float — hashes by its `BigInt`; a non-integer finite
//! float, `±inf`, or the canonical NaN hashes by its bit pattern. So `1` and `1.0`
//! hash alike; `0`, `0.0`, `-0.0` hash alike; NaN has one hash.
//!
//! **Records as keys (L§9.5, L§15 hook 2; S-29).** A value record whose fields are
//! all (recursively) hashable is a key; it hashes structurally over its type and
//! fields, coherent with the nominal fieldwise `==`. A reference record, and any
//! value record with a list/dict/`ref` field reachable inside it, is **not**
//! hashable — so hashing never enters a reference graph and is total without cycle
//! detection. [`check_hashable`] enforces this and names the offending field.

use crate::heap::Heap;
use crate::machine::{RecIdx, TypeKind, Value};
use num_bigint::BigInt;
use std::hash::Hasher;

use super::compare::{decompose, kind_name};

// A fixed SipHash key. Any constant works — the hash is never serialized or
// compared across engine builds — so these are simply two fixed 64-bit words.
const K0: u64 = 0x0706_0504_0302_0100;
const K1: u64 = 0x0f0e_0d0c_0b0a_0908;

// Kind tags keep distinct kinds from colliding by content.
const TAG_NIL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STR: u8 = 4;
const TAG_BYTES: u8 = 5;
const TAG_RECORD: u8 = 6;

/// Checks that `v` may be a dict key (L§4.8, L§9.5): scalars (numbers, booleans,
/// strings, bytes, nil) and **value records whose fields are all (recursively)
/// hashable**. Lists, dicts, reference records, and any value record with such a
/// field anywhere inside are rejected — the principle is that no shared mutable
/// value be reachable from a stored key (a value-record key is the dict's own copy,
/// §4.14, so it is unreachable). `Err` carries a message naming the offending field,
/// for the raise at insertion/lookup.
pub(super) fn check_hashable(v: Value, heap: &Heap) -> Result<(), String> {
    match v {
        Value::Nil
        | Value::Bool(_)
        | Value::Int(_)
        | Value::BigInt(_)
        | Value::Float(_)
        | Value::Str(_)
        | Value::Bytes(_) => Ok(()),
        Value::Record(r) => check_record_hashable(r, heap),
        // Lists, dicts, and the reference kinds carry shared mutable identity.
        other => Err(format!("{} can't be a dict key", kind_name(other))),
    }
}

/// A value record is hashable iff it is not a `ref` record and every field is
/// (recursively) hashable (L§9.5). Names the first offending field; for a nested
/// value-record field the deeper message is propagated so it names the actual
/// non-hashable leaf, not just the intermediate field.
fn check_record_hashable(r: RecIdx, heap: &Heap) -> Result<(), String> {
    let type_idx = heap.record(r).type_idx;
    let (is_ref, type_name, field_names) = match &heap.type_value(type_idx).kind {
        TypeKind::Record(rt) => (rt.is_ref, rt.name.clone(), rt.fields.clone()),
        TypeKind::Builtin(_) => unreachable!("a record's type is a record type"),
    };
    if is_ref {
        return Err(format!(
            "`{type_name}` is a reference record, so it can't be a dict key"
        ));
    }
    let fields = heap.record(r).fields.to_vec();
    for (name, &f) in field_names.iter().zip(fields.iter()) {
        if let Err(inner) = check_hashable(f, heap) {
            // A value-record field's own message already names the deep offender;
            // any other non-hashable field is named here with its kind.
            return Err(if is_value_record(f, heap) {
                inner
            } else {
                format!("field `{name}` of `{type_name}` can't be part of a dict key ({inner})")
            });
        }
    }
    Ok(())
}

/// Whether `v` is a **value** record (a record that is not `ref`).
fn is_value_record(v: Value, heap: &Heap) -> bool {
    let Value::Record(r) = v else {
        return false;
    };
    match &heap.type_value(heap.record(r).type_idx).kind {
        TypeKind::Record(rt) => !rt.is_ref,
        TypeKind::Builtin(_) => false,
    }
}

/// The content hash of a hashable value — coherent with structural `==` (L§4.8).
/// The caller guarantees `is_hashable(v)`.
pub(super) fn hash_value(v: Value, heap: &Heap) -> u64 {
    let mut h = SipHasher13::new();
    hash_into(v, heap, &mut h);
    h.finish()
}

fn hash_into<H: Hasher>(v: Value, heap: &Heap, h: &mut H) {
    match v {
        Value::Nil => h.write_u8(TAG_NIL),
        Value::Bool(b) => {
            h.write_u8(TAG_BOOL);
            h.write_u8(b as u8);
        }
        Value::Int(_) | Value::BigInt(_) | Value::Float(_) => hash_number(v, heap, h),
        Value::Str(i) => hash_bytes(TAG_STR, heap.string(i).utf8.as_bytes(), h),
        Value::Bytes(i) => hash_bytes(TAG_BYTES, &heap.byte_string(i).bytes, h),
        Value::Record(r) => hash_record(r, heap, h),
        _ => unreachable!("a non-hashable value reached hashing (guarded by check_hashable)"),
    }
}

/// Hashes a value record structurally (L§9.5, L§15 hook 2): its declared type plus
/// each field in declaration order, coherent with the nominal fieldwise `==`
/// (L§4.13). The [`is_hashable`] gate guarantees every field is a scalar or another
/// value record, so this never enters a reference graph — total and terminating
/// without cycle detection.
fn hash_record<H: Hasher>(r: RecIdx, heap: &Heap, h: &mut H) {
    h.write_u8(TAG_RECORD);
    // Nominal identity: the declared type is part of what `==` compares, so it is
    // part of the hash (and fixes the field count). A run assigns type indices
    // deterministically and the hash is never compared across builds (module
    // header), so the raw index is a fine deterministic type discriminator.
    h.write_u32(heap.record(r).type_idx.0);
    let fields = heap.record(r).fields.to_vec();
    for f in fields {
        hash_into(f, heap, h);
    }
}

fn hash_bytes<H: Hasher>(tag: u8, bytes: &[u8], h: &mut H) {
    h.write_u8(tag);
    h.write_u64(bytes.len() as u64); // length-prefix so the content is unambiguous
    h.write(bytes);
}

fn hash_number<H: Hasher>(v: Value, heap: &Heap, h: &mut H) {
    match v {
        Value::Int(n) => hash_int(&BigInt::from(n), h),
        Value::BigInt(i) => hash_int(&heap.bigint(i).value, h),
        Value::Float(x) => {
            if x.is_finite() && x == x.trunc() {
                // Integer-valued: hash like the exact integer (`1.0`≡`1`; `-0.0`≡`0`).
                hash_int(&float_to_exact_int(x), h);
            } else {
                // Non-integer finite float, ±inf, or the canonical NaN: hash the bits.
                // NaN is canonicalized to one pattern (E§4.3), so it hashes once.
                h.write_u8(TAG_FLOAT);
                h.write_u64(x.to_bits());
            }
        }
        _ => unreachable!("hash_number on a non-number"),
    }
}

fn hash_int<H: Hasher>(n: &BigInt, h: &mut H) {
    h.write_u8(TAG_INT);
    let bytes = n.to_signed_bytes_le(); // canonical minimal two's-complement
    h.write_u64(bytes.len() as u64);
    h.write(&bytes);
}

/// The exact integer value of an integer-valued finite `f64` (`x == x.trunc()`).
fn float_to_exact_int(x: f64) -> BigInt {
    let (mant, exp) = decompose(x);
    if exp >= 0 {
        mant << exp as usize
    } else {
        // Exact: an integer-valued float has zeros in its low `-exp` bits.
        mant >> ((-exp) as usize)
    }
}

/// A fixed-key **SipHash-1-3** streaming hasher (1 compression round per block, 3
/// finalization rounds). Keyed by the fixed [`K0`]/[`K1`]; deterministic by
/// construction — no seed, no address input.
#[derive(Clone, Copy)]
struct SipHasher13 {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    tail: u64,     // partial trailing block, filled low byte first
    ntail: usize,  // bytes currently in `tail` (0..8)
    length: usize, // total bytes written
}

impl SipHasher13 {
    fn new() -> Self {
        SipHasher13 {
            v0: K0 ^ 0x736f_6d65_7073_6575,
            v1: K1 ^ 0x646f_7261_6e64_6f6d,
            v2: K0 ^ 0x6c79_6765_6e65_7261,
            v3: K1 ^ 0x7465_6462_7974_6573,
            tail: 0,
            ntail: 0,
            length: 0,
        }
    }

    #[inline]
    fn sip_round(&mut self) {
        self.v0 = self.v0.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(13);
        self.v1 ^= self.v0;
        self.v0 = self.v0.rotate_left(32);
        self.v2 = self.v2.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(16);
        self.v3 ^= self.v2;
        self.v0 = self.v0.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(21);
        self.v3 ^= self.v0;
        self.v2 = self.v2.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(17);
        self.v1 ^= self.v2;
        self.v2 = self.v2.rotate_left(32);
    }

    #[inline]
    fn absorb(&mut self, m: u64) {
        self.v3 ^= m;
        self.sip_round(); // c = 1
        self.v0 ^= m;
    }
}

impl Hasher for SipHasher13 {
    fn write(&mut self, msg: &[u8]) {
        self.length += msg.len();
        let mut msg = msg;
        // Finish any partial block held in `tail`.
        if self.ntail != 0 {
            let need = 8 - self.ntail;
            let take = need.min(msg.len());
            for (i, &b) in msg[..take].iter().enumerate() {
                self.tail |= (b as u64) << (8 * (self.ntail + i));
            }
            if take < need {
                self.ntail += take;
                return;
            }
            let m = self.tail;
            self.absorb(m);
            self.tail = 0;
            self.ntail = 0;
            msg = &msg[take..];
        }
        // Absorb whole 8-byte blocks.
        let mut chunks = msg.chunks_exact(8);
        for chunk in &mut chunks {
            self.absorb(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        // Stash the remainder for next time.
        let rem = chunks.remainder();
        for (i, &b) in rem.iter().enumerate() {
            self.tail |= (b as u64) << (8 * i);
        }
        self.ntail = rem.len();
    }

    fn finish(&self) -> u64 {
        let mut s = *self;
        // The last block is the trailing bytes with the length (mod 256) on top.
        let b = ((s.length as u64 & 0xff) << 56) | s.tail;
        s.v3 ^= b;
        s.sip_round(); // c = 1
        s.v0 ^= b;
        s.v2 ^= 0xff;
        s.sip_round(); // d = 3
        s.sip_round();
        s.sip_round();
        s.v0 ^ s.v1 ^ s.v2 ^ s.v3
    }
}

#[cfg(test)]
mod tests;
