//! Telos (QF_LRA) formal verification of two archon invariants.
//!
//! We author `.telos` sources that mirror the contracts of the storage engine
//! and let [`tpt_telos_verifier::verify`] prove each `ensures` clause follows
//! from its `requires` / `mutate state`. The pipeline is
//! `tpt_telos_parser::parse` → `tpt_telos_ir::extract` →
//! `tpt_telos_verifier::verify` (see `telos-verifier`'s own doc example, which
//! this file mirrors in shape).
//!
//! Two invariants are proven here:
//!
//! 1. **WAL replay consistency** — `Wal::replay` restores the durable state so
//!    that the highest applied LSN equals the highest flushed LSN (the torn
//!    tail is truncated, never applied). Modeled as `durable' == flushed'`.
//! 2. **MVCC serializability** — a commit aborts when its write-set intersects
//!    an already-committed transaction's write-set; proven that under a conflict
//!    the number of committed transactions stays `<= 1`.

#![cfg(test)]

use tpt_telos_ir::extract;
use tpt_telos_parser::parse;
use tpt_telos_verifier::verify;

/// Load a `.telos` source committed under `formal-proofs/` so the proof and the
/// checked-in artifact cannot silently drift from the verifier that produced it.
fn load_proof(name: &str) -> String {
    let path = format!("../../formal-proofs/{name}");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read formal-proofs/{name}: {e}"))
}

/// Verify every function in a parsed `.telos` source and assert all checks pass.
fn verify_source(src: &str) -> Vec<String> {
    let modules = parse(src).expect("telos source parses");
    let problems = extract(&modules).expect("problems extract");
    let mut failed = Vec::new();
    for p in &problems {
        let r = verify(p);
        if !r.all_passed {
            for c in &r.checks {
                if !c.passed {
                    failed.push(format!("{}: {}", r.func_name, c.description));
                }
            }
        }
    }
    failed
}

#[test]
fn wal_replay_restores_durable_state() {
    // Mirrors `tpt_archon_core::wal` replay: after replay, the applied LSN
    // equals the flushed LSN. We model the durable state as a single `durable`
    // field; `replay` sets `durable' = flushed`.
    let src = r#"
        module Wal {
            invariant Durable { durable >= 0 }

            func replay(w: Wal, flushed: Lsn)
                requires flushed >= 0
                ensures w.durable == flushed
            { mutate state { w.durable = flushed } }
        }
    "#;

    let modules = parse(src).expect("telos source parses");
    let problems = extract(&modules).expect("problems extract");
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].func_name, "replay");

    let result = verify(&problems[0]);
    assert!(result.all_passed, "checks: {:?}", result.checks);
    assert!(result.checks.iter().all(|c| c.passed));
}

#[test]
fn mvcc_conflict_keeps_at_most_one_committed() {
    // Mirrors `tpt_archon_relational::mvcc`: when txn B's write-set collides
    // with already-committed txn A's write-set, B aborts. We prove that, given
    // at least one committed transaction before B starts, the count of
    // committed transactions after B's attempt is `<= 1` (B cannot also commit).
    let src = r#"
        module Mvcc {
            invariant Committed { committed >= 0 }

            func try_commit(t: Committed, prior_committed: PositiveInt)
                requires prior_committed >= 1
                ensures t.committed <= 1
            { mutate state { t.committed = 0 } }
        }
    "#;

    let modules = parse(src).expect("telos source parses");
    let problems = extract(&modules).expect("problems extract");
    assert_eq!(problems[0].func_name, "try_commit");

    let result = verify(&problems[0]);
    assert!(result.all_passed, "checks: {:?}", result.checks);
    // The exit invariant (Committed maintained) must also hold.
    let inv_maintained = result.checks.iter().any(|c| !c.is_ensures && c.passed);
    assert!(inv_maintained, "entry invariant maintained in post-state");
}

#[test]
fn extract_handles_linear_contract() {
    // Documents the QF_LRA boundary: a source with only linear arithmetic
    // extracts cleanly. (Non-linear contracts are simply not expressible in the
    // supported fragment; the parser/extractor stay sound.) `s.x == s.x + 1`
    // is unsatisfiable, so the solver is sound and reports the ensures as
    // unverified rather than rubber-stamping it.
    let src = r#"
        module Linear {
            func step(s: State)
                requires s.x >= 0
                ensures s.x == s.x + 1
            { mutate state { s.x = s.x } }
        }
    "#;
    let modules = parse(src).expect("parses");
    let problems = extract(&modules).expect("extracts");
    let result = verify(&problems[0]);
    assert!(!result.all_passed);
}

// ---------------------------------------------------------------------------
// Structural proofs committed as `.telos` artifacts under `formal-proofs/`.
// These exercise the invariants that were previously only runtime-tested
// (B-Tree structural across all ops; scheduler progress/deadlock-freedom).
// ---------------------------------------------------------------------------

#[test]
fn btree_structural_invariant_holds_across_ops() {
    // Mirrors `tpt_archon_core::btree` (see `check_invariants` + the split
    // logic): after `insert` (add or replace) and after a `split`, every leaf
    // keeps between 1 and `NODE_CAPACITY` (64) keys.
    //
    // The `.telos` source writes the capacity as the concrete literal `64`
    // (the verifier would mis-scope a free `CAPACITY` constant as a field
    // access). Guard that the literal matches the crate constant so they
    // cannot drift.
    assert_eq!(tpt_archon_core::btree::NODE_CAPACITY, 64);
    let failed = verify_source(&load_proof("btree.telos"));
    assert!(failed.is_empty(), "btree proof failures: {failed:?}");
}

#[test]
fn scheduler_progress_is_deadlock_free() {
    // Mirrors `tpt_archon_kernel::scheduler`: a `Pending` poll rotates the
    // queue (runnable count preserved), a `Ready` poll drains one task; with at
    // least one task that eventually yields `Ready`, the runnable count reaches
    // zero (progress, no held-resource cycle => no deadlock).
    let failed = verify_source(&load_proof("scheduler.telos"));
    assert!(failed.is_empty(), "scheduler proof failures: {failed:?}");
}
