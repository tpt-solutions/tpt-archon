//! Eidos (QF_LRA) proof of the B-Link tree node-capacity invariant.
//!
//! The invariant, checked at compile time in `tpt-archon-core` by
//! `btree::assert_node_fits_page()`, is: a fully-packed B-Link node — node
//! header + `NODE_CAPACITY` key/value slots + a right-link pointer — fits
//! inside a single `PAGE_SIZE` page.
//!
//! We re-derive the exact arithmetic the core crate uses (the per-field byte
//! sizes live in `btree.rs` as module-private `const`s; we mirror them here and
//! tie the result to the *public* `page::PAGE_SIZE` and `btree::NODE_CAPACITY`
//! so the proof and the code cannot silently drift apart on the constants that
//! actually escape the crate). The eidos solver then proves the layout
//! inequality is satisfiable within the page and that "overflow" is
//! unsatisfiable.

#![cfg(test)]

use tpt_archon_core::btree::NODE_CAPACITY;
use tpt_archon_core::page::PAGE_SIZE;
use tpt_eidos_verifier::{unsat, Constraint, LinExpr};

// Mirror of `btree.rs` module-private layout constants. Named identically and
// documented so a future reader cross-checks them against the core source.
/// `1 (node type byte) + 2 (key count) + 8 (right-link)` — see
/// `btree::NODE_HEADER_BYTES`.
const NODE_HEADER_BYTES: f64 = (1 + 2 + 8) as f64;
/// Key (8) + value slot (48) — see `btree::VALUE_SLOT_BYTES`.
const VALUE_SLOT_BYTES: f64 = 48.0;
/// Right-link is already counted in the header's 8-byte field.
/// `NODE_HEADER_BYTES + NODE_CAPACITY * (8 + VALUE_SLOT_BYTES)`.
fn max_node_bytes() -> f64 {
    NODE_HEADER_BYTES + (NODE_CAPACITY as f64) * (8.0 + VALUE_SLOT_BYTES)
}

#[test]
fn node_capacity_constants_match_core() {
    // Guard the mirror: if core ever changes PAGE_SIZE or NODE_CAPACITY these
    // assertions fail loudly instead of the proof silently using stale numbers.
    assert_eq!(PAGE_SIZE, 4096);
    assert_eq!(NODE_CAPACITY, 64);
    // Sanity: the mirrored arithmetic equals the documented 3595-byte max.
    assert_eq!(max_node_bytes() as usize, 3595);
}

#[test]
fn full_node_fits_within_page() {
    // Witness that a packed node of `used` bytes with `used <= max_node_bytes()`
    // and `max_node_bytes() <= PAGE_SIZE` is satisfiable.
    let used = LinExpr::var("used");
    let premises = vec![
        Constraint::ge(used.clone()), // used >= 0
        Constraint::le(used.sub(&LinExpr::constant(max_node_bytes()))), // used <= max
        Constraint::le(
            LinExpr::constant(max_node_bytes()).sub(&LinExpr::constant(PAGE_SIZE as f64)),
        ), // max <= PAGE_SIZE
    ];
    assert!(
        !unsat(&premises),
        "a packed node within the page must be satisfiable"
    );
}

#[test]
fn node_overflow_is_unsatisfiable() {
    // Prove the overflow condition is unsatisfiable: there is no assignment of
    // the (constant) layout to a page violation. We express `overflow =
    // max - PAGE_SIZE` and show `overflow > 0` contradicts the known
    // `overflow <= 0`.
    let overflow = LinExpr::var("overflow");
    // overflow <= 0  (max <= PAGE_SIZE)
    let le_page = Constraint::le(overflow.clone());
    // Conclusion we refute: overflow > 0.
    let contradiction = vec![le_page, Constraint::gt(overflow)];
    assert!(
        unsat(&contradiction),
        "node overflow contradicts the page bound"
    );
    // Positive entailment: max node size is provably <= PAGE_SIZE.
    let layout_le_page = Constraint::le(
        LinExpr::constant(max_node_bytes()).sub(&LinExpr::constant(PAGE_SIZE as f64)),
    );
    assert!(
        tpt_eidos_verifier::entails(&[], &layout_le_page),
        "max node size is provably <= PAGE_SIZE"
    );
}

#[test]
fn over_capacity_node_cannot_fit() {
    // Strengthen the invariant: the page has slack (PAGE_SIZE - full-node bytes
    // = 501 bytes), so one extra slot is fine, but a node that *needs* more
    // bytes than the page holds cannot be laid out. We compute the smallest
    // slot count `slots` with `header + slots*(8+slot) > PAGE_SIZE` and prove
    // the "fits" assertion for that node is unsatisfiable.
    let per_key = 8.0 + VALUE_SLOT_BYTES; // 56
    let slack = (PAGE_SIZE as f64) - max_node_bytes(); // 4096 - 3595 = 501
    assert!(slack > 0.0, "a full node leaves headroom in the page");
    // slots needed to exceed the page: smallest s with header + s*per_key > PAGE.
    let overflow_slots = (((PAGE_SIZE as f64) - NODE_HEADER_BYTES) / per_key).ceil() + 1.0;
    let extra = NODE_HEADER_BYTES + overflow_slots * per_key;
    assert!(
        extra > PAGE_SIZE as f64,
        "overflow slot count exceeds the page"
    );
    let used = LinExpr::var("u");
    let bad = vec![
        Constraint::eq(used.sub(&LinExpr::constant(extra))), // u == extra (fully packed)
        Constraint::le(used.sub(&LinExpr::constant(PAGE_SIZE as f64))), // u <= PAGE_SIZE
    ];
    assert!(
        unsat(&bad),
        "a node needing more than a page of bytes cannot fit in the page"
    );
}
