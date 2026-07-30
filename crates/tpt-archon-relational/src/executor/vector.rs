//! Embedding similarity search (the `f32[]` / RAG use case).

use alloc::vec::Vec;

/// Cosine-style vector similarity search over stored embedding rows.
///
/// This is the CPU fallback for the RAG/embeddings use case; the `gpu` feature
/// would route the same call to `tpt-gpu-*`. Returns the row indices of the
/// `k` nearest embeddings to `query` by dot-product similarity.
pub fn vector_topk(embeddings: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let dot: f32 = e.iter().zip(query).map(|(a, b)| a * b).sum();
            (i, dot)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(i, _)| i).collect()
}
