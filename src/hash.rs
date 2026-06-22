//! THE ONE OPERATION + THE TRINITY OF MERGES — sanguine's `HashTrinity` made code.
//!
//! Ported from `~/Workspace/sanguine` (`income/collapse_core/src/hash.rs`,
//! `proof/Substrate/Algebra/HashTrinity.lean`) and `~/Workspace/song`
//! (`Song/Table.lean`, `Song/Fold.lean`). There is one data structure (the
//! dictionary-as-applicative) and one operation, `insert_with f` — the merge
//! `f` is the only freedom (`merge_is_the_only_freedom`). Argument order is the
//! theorem's: `get (insert_with f k v m) k = f(new, old)`.
//!
//! The four faces, and ONLY these (anything else is the zoo):
//!   NB  (0)  `set_merge`/`first_merge` — idempotent          → Set
//!   B   (1)  `map_merge`               — replace             → Map
//!   B/U (/)  `acc_merge_*`             — a monoid (+/max/min) → Accumulator
//!   4th      `link_merge`              — link (V = Vec)       → Graph
//!
//! THE LAW OF EXCLUSIVITY: every piece of STATE is one of the four faces and
//! every update goes through `insert_with` (or its accumulator carrier, `Acc`).
//! Iteration exists only as the fold that drives merges
//! (`CollapseShape.collapseFold = List.foldl`); pure value computation between
//! merges is the applicative. Ad-hoc mutable locals — flags, trackers, hand
//! `while`/`loop`, `sort` — are the zoo, denied here.
//!
//! CARRIER NOTE (dense keys, from the sanguine source verbatim). A table whose
//! keys are dense `0..n` ids is the SAME dictionary face on an array carrier
//! (`Vec` indexed by id; CSR adjacency for the Graph face). Hashing a dense id
//! buys nothing and costs ~5×. The face is the law; the carrier is data — pick
//! the array when the key domain is dense, the hash table when it is sparse.
//! sdirstat's `Vec<Node>` arena and `Vec<Vec<usize>>` children are this exact
//! array carrier; the Acc below is the B/U accumulator on the byte carrier.

use std::collections::btree_map::Entry as OEntry;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

pub type HMap<K, V> = HashMap<K, V>;
/// The ordered table — the one add-on; same operation, ordered key walk (so a
/// problem that needs *order* never reaches for a `sort` call). The next
/// adopter is `emit_json`'s per-directory top-K, which still sorts (the zoo).
pub type OMap<K, V> = BTreeMap<K, V>;

/// The one operation: update `k` by merging the new value with the old via `f`.
/// `f(new, old)` — the order of `get_insert_with_self`.
pub fn insert_with<K: Eq + Hash, V>(map: &mut HMap<K, V>, k: K, v: V, f: impl FnOnce(V, &V) -> V) {
    match map.entry(k) {
        Entry::Occupied(mut e) => {
            let merged = f(v, e.get());
            *e.get_mut() = merged;
        }
        Entry::Vacant(e) => {
            e.insert(v);
        }
    }
}

/// The one operation at the ordered table — identical semantics.
pub fn insert_with_ord<K: Ord, V>(map: &mut OMap<K, V>, k: K, v: V, f: impl FnOnce(V, &V) -> V) {
    match map.entry(k) {
        OEntry::Occupied(mut e) => {
            let merged = f(v, e.get());
            *e.get_mut() = merged;
        }
        OEntry::Vacant(e) => {
            e.insert(v);
        }
    }
}

/// NB (0): the idempotent merge — once in, in. → Set.
pub fn set_merge(new: bool, old: &bool) -> bool {
    new || *old
}
/// NB (0): first write wins (the interning/dedup face). → Set on a Map carrier.
pub fn first_merge<A: Clone>(_new: A, old: &A) -> A {
    old.clone()
}
/// B (1): replace — last write wins. → Map.
pub fn map_merge<A>(new: A, _old: &A) -> A {
    new
}
/// B/U (/): the monoid merge, additive face (the size fold is this). → Accumulator.
pub fn acc_merge_add<A: Copy + std::ops::Add<Output = A>>(new: A, old: &A) -> A {
    *old + new
}
/// B/U (/): the monoid merge, max face (tropical/max-plus). → Accumulator.
pub fn acc_merge_max<A: Copy + PartialOrd>(new: A, old: &A) -> A {
    if new > *old { new } else { *old }
}
/// The 4th: V = Vec of links, merge = ++ (the children adjacency). → Graph.
pub fn link_merge<E: Clone>(new: Vec<E>, old: &[E]) -> Vec<E> {
    let mut v = old.to_vec();
    v.extend(new);
    v
}

/// The one recursor — `foldr`, the eliminator (`Song/Fold.lean`: "the algebra
/// `(op, init)` is the only freedom"). Iteration that is not this is the zoo.
pub fn fold<A, B>(op: impl Fn(&A, B) -> B, init: B, xs: &[A]) -> B {
    xs.iter().rfold(init, |acc, x| op(x, acc))
}

const HEX_UP: &[u8; 16] = b"0123456789ABCDEF";
const HEX_LO: &[u8; 16] = b"0123456789abcdef";

/// `Acc` — the B/U accumulator on the byte carrier: the append monoid
/// (`acc_merge` over `Vec<u8>`) that *one* fold drives a serialization into.
/// One buffer, grown by pushes; the single `write_all` of `as_slice()` at the
/// end is the one cut to the metal (Axiom XI — the syscall is the cut). There
/// is **no `format!`** here: an integer is emitted by the radix unfold
/// (`iter::successors`, the fold's dual — the numeral carries its own bounded
/// iteration, terminating where an open `while` would not be allowed).
pub struct Acc {
    buf: Vec<u8>,
}

impl Acc {
    pub fn with_capacity(n: usize) -> Acc {
        Acc { buf: Vec::with_capacity(n) }
    }
    pub fn byte(&mut self, b: u8) -> &mut Self {
        self.buf.push(b);
        self
    }
    pub fn bytes(&mut self, s: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(s);
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }

    /// Base-`radix` unfold into the buffer, most-significant digit first. The
    /// digits emerge least-significant from `successors`, so the just-written
    /// tail is reversed — that is the radix bit-order, not a `sort` merge.
    fn radix(&mut self, v: u64, radix: u64, digits: &[u8; 16]) -> &mut Self {
        let start = self.buf.len();
        std::iter::successors(Some(v), move |&r| (r >= radix).then_some(r / radix))
            .for_each(|r| self.buf.push(digits[(r % radix) as usize]));
        self.buf[start..].reverse();
        self
    }
    /// Decimal, like `{}`.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.radix(v, 10, HEX_LO)
    }
    /// Octal, like `{:o}` (no `0` prefix — the caller adds it, as the old `0{:o}` did).
    pub fn oct(&mut self, v: u64) -> &mut Self {
        self.radix(v, 8, HEX_LO)
    }
    /// Lowercase hex, like `{:x}`.
    pub fn hex(&mut self, v: u64) -> &mut Self {
        self.radix(v, 16, HEX_LO)
    }

    /// The QDirStat cache escape, folded straight into the buffer: a byte that
    /// is control/space or `%` becomes `%XX` (uppercase, like `{:02X}`); every
    /// other byte passes through **raw**. (The old `esc` round-tripped each
    /// byte through `char`, which re-encoded UTF-8 continuation bytes ≥0x80 to
    /// two bytes — corrupting non-ASCII names. Raw passthrough is the fix.)
    pub fn esc(&mut self, s: &str) -> &mut Self {
        s.bytes().for_each(|b| {
            if b <= 0x20 || b == b'%' {
                self.buf.push(b'%');
                self.buf.push(HEX_UP[(b >> 4) as usize]);
                self.buf.push(HEX_UP[(b & 0xF) as usize]);
            } else {
                self.buf.push(b);
            }
        });
        self
    }

    /// JSON string escape, folded into the buffer (the other emit face's escape):
    /// `" \ \n \r \t` map to their JSON forms, other control bytes to `\u00XX`,
    /// everything else passes through as its raw UTF-8. Matches the old
    /// `json_escape`'s output byte-for-byte.
    pub fn esc_json(&mut self, s: &str) -> &mut Self {
        s.chars().for_each(|c| match c {
            '"' => self.buf.extend_from_slice(b"\\\""),
            '\\' => self.buf.extend_from_slice(b"\\\\"),
            '\n' => self.buf.extend_from_slice(b"\\n"),
            '\r' => self.buf.extend_from_slice(b"\\r"),
            '\t' => self.buf.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                let v = c as u32; // control byte: \u00XX (always two high zero nibbles here)
                self.buf.extend_from_slice(b"\\u00");
                self.buf.push(HEX_LO[((v >> 4) & 0xF) as usize]);
                self.buf.push(HEX_LO[(v & 0xF) as usize]);
            }
            c => {
                let mut b4 = [0u8; 4];
                self.buf.extend_from_slice(c.encode_utf8(&mut b4).as_bytes());
            }
        });
        self
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
    /// Consume into a `String` (the buffer is built from valid-UTF-8 pushes;
    /// lossy only as a defensive fallback).
    pub fn into_string(self) -> String {
        String::from_utf8(self.buf).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}
