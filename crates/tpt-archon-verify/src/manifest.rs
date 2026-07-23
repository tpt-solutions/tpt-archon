//! Manifest verification for `.telos` proof sources.
//!
//! Every `.telos` file under `formal-proofs/` must have a matching
//! `<name>.telos.proof.json` manifest recording the source's SHA-256 digest.
//! CI fails if a manifest is missing, or if the recorded hash no longer
//! matches the source file's bytes (`ArchonError::ManifestTampered` in the
//! design spec) — this is what stops an unverified or silently-edited
//! `.telos` file from drifting away from what `telos.rs` actually proved.

#![cfg(test)]

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Manifest {
    source: String,
    sha256: String,
    #[allow(dead_code)]
    proof_ids: Vec<String>,
}

fn read(name: &str) -> String {
    let path = format!("../../formal-proofs/{name}");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read formal-proofs/{name}: {e}"))
}

fn load_manifest(source_name: &str) -> Manifest {
    let manifest_name = format!("{source_name}.proof.json");
    let raw = std::fs::read_to_string(format!("../../formal-proofs/{manifest_name}"))
        .unwrap_or_else(|e| panic!("missing manifest formal-proofs/{manifest_name}: {e}"));
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("malformed manifest formal-proofs/{manifest_name}: {e}"))
}

/// Asserts `formal-proofs/{source_name}` has a manifest whose recorded
/// `sha256` matches the source file's actual bytes.
fn verify_manifest(source_name: &str) {
    let manifest = load_manifest(source_name);
    assert_eq!(
        manifest.source, source_name,
        "manifest for {source_name} declares a different source: {}",
        manifest.source
    );

    let source_bytes = read(source_name);
    let digest = Sha256::digest(source_bytes.as_bytes());
    let actual_hex = format!("{digest:x}");

    assert_eq!(
        manifest.sha256, actual_hex,
        "ArchonError::ManifestTampered: formal-proofs/{source_name} does not match its manifest \
         (manifest sha256 {}, actual {actual_hex})",
        manifest.sha256
    );
}

#[test]
fn btree_telos_manifest_matches_source() {
    verify_manifest("btree.telos");
}

#[test]
fn scheduler_telos_manifest_matches_source() {
    verify_manifest("scheduler.telos");
}
