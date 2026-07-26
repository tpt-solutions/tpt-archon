//! IVFFlat approximate-nearest-neighbor index for embedding columns.
//!
//! `executor::vector_topk` is an exact brute-force scan: correct at any
//! scale, but O(n) per query, which loses to pgvector's indexed search once
//! a table gets large (measured in `benches/benches/vector_compare.rs`).
//! This index partitions vectors into `nlist` clusters via k-means so a
//! query only ranks candidates from the `nprobe` nearest clusters instead of
//! every row — the same trade pgvector's IVFFlat index type makes, including
//! its recall caveat: a true nearest neighbor whose cluster wasn't among the
//! `nprobe` probed ones is missed.
//!
//! Clustering itself is done on L2-normalized (unit) vectors — i.e. by
//! cosine direction, "spherical k-means" — even though the final re-rank of
//! candidates uses the same raw inner product `vector_topk` does. Clustering
//! by raw (unnormalized) dot product was tried first and collapses: a
//! centroid that happens to end up with larger norm keeps winning more
//! points' nearest-cluster assignment every Lloyd iteration *regardless of
//! direction* (dot product scales with magnitude, not just alignment), so a
//! couple of centroids end up owning most of the dataset and `nprobe`
//! clusters cover nearly all rows — no faster than brute force. Normalizing
//! only for the clustering step (not for the stored vectors or the final
//! score) fixes that while keeping the exact-rerank metric unchanged.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Minimum live row count before building an index pays for its own k-means
/// cost; below this, callers should just brute-force scan with
/// `executor::vector_topk`.
pub const MIN_ROWS_FOR_INDEX: usize = 1000;

/// Default number of clusters to probe per search — trades recall for speed.
pub const DEFAULT_NPROBE: usize = 8;

/// Number of Lloyd's-algorithm refinement passes run at build time.
const KMEANS_ITERS: usize = 4;

/// An IVFFlat index: `nlist` centroids, each owning the ids+vectors of the
/// rows assigned to it.
#[derive(Debug, Clone)]
pub struct IvfFlatIndex {
    dim: usize,
    centroids: Vec<Vec<f32>>,
    lists: Vec<Vec<(u64, Vec<f32>)>>,
    id_to_list: BTreeMap<u64, usize>,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// `f32::sqrt` needs `std`/`libm` and isn't available in this crate's `no_std`
/// build, so this is a bit-trick initial guess (the classic "fast inverse
/// square root" magic constant) refined by a few Newton-Raphson iterations —
/// division and multiplication only, both always available on `f32`. Accurate
/// to within float rounding error, which is plenty: this only feeds
/// direction-normalization for clustering, not the final exact-rerank score.
fn sqrt_f32(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let i = x.to_bits();
    let i = 0x5f37_59df - (i >> 1);
    let mut y = f32::from_bits(i);
    y *= 1.5 - 0.5 * x * y * y;
    y *= 1.5 - 0.5 * x * y * y;
    y *= 1.5 - 0.5 * x * y * y;
    y *= 1.5 - 0.5 * x * y * y;
    x * y
}

/// L2-normalizes `v`; returns `v` unchanged if it's the zero vector (nothing
/// sane to normalize to, and it'll only ever tie every cluster anyway).
fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = sqrt_f32(dot(v, v));
    if norm == 0.0 {
        v.to_vec()
    } else {
        v.iter().map(|x| x / norm).collect()
    }
}

/// The cluster whose centroid is closest in cosine direction to `unit_v`.
/// Both `centroids` and `unit_v` must already be L2-normalized — see the
/// module docs on why clustering uses cosine direction while the final
/// candidate re-rank still uses raw inner product.
fn nearest_cluster(centroids: &[Vec<f32>], unit_v: &[f32]) -> usize {
    centroids
        .iter()
        .enumerate()
        .map(|(i, c)| (i, dot(c, unit_v)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(core::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// `sqrt(n)` is the standard IVFFlat rule of thumb (it's what pgvector's own
/// docs recommend); clamped so build cost stays bounded on very large tables
/// and so at least one cluster always exists. Integer binary-search sqrt —
/// no float division needed here, so no reason to route this through
/// `sqrt_f32` and its precision caveats.
fn nlist_for(n: usize) -> usize {
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if mid.checked_mul(mid).is_some_and(|sq| sq <= n) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo.clamp(1, 256)
}

/// Deterministic seeding (no RNG dependency, and reproducible in tests):
/// picks evenly spaced unit vectors across input order as the initial
/// centroids.
fn seed_centroids(unit_vectors: &[Vec<f32>], nlist: usize) -> Vec<Vec<f32>> {
    let n = unit_vectors.len();
    (0..nlist)
        .map(|i| unit_vectors[i * n / nlist].clone())
        .collect()
}

impl IvfFlatIndex {
    /// Builds an index over `vectors` (row id, embedding pairs). Panics if
    /// `vectors` is empty or embeddings have mismatched dimensions — callers
    /// gate on [`MIN_ROWS_FOR_INDEX`] before calling this.
    pub fn build(vectors: &[(u64, Vec<f32>)]) -> Self {
        assert!(
            !vectors.is_empty(),
            "cannot build an index over zero vectors"
        );
        let dim = vectors[0].1.len();
        let nlist = nlist_for(vectors.len());
        // Cluster on direction (unit vectors), not the raw magnitude-skewed
        // vectors — see the module docs.
        let unit_vectors: Vec<Vec<f32>> = vectors.iter().map(|(_, v)| normalize(v)).collect();
        let mut centroids = seed_centroids(&unit_vectors, nlist);

        for _ in 0..KMEANS_ITERS {
            let mut sums = alloc::vec![alloc::vec![0f32; dim]; nlist];
            let mut counts = alloc::vec![0usize; nlist];
            for uv in &unit_vectors {
                let c = nearest_cluster(&centroids, uv);
                for (s, x) in sums[c].iter_mut().zip(uv.iter()) {
                    *s += x;
                }
                counts[c] += 1;
            }
            for c in 0..nlist {
                if counts[c] > 0 {
                    for (centroid_dim, sum) in centroids[c].iter_mut().zip(sums[c].iter()) {
                        *centroid_dim = sum / counts[c] as f32;
                    }
                    // Re-normalize: the mean of unit vectors isn't itself
                    // unit length, and `nearest_cluster` assumes it is
                    // (standard spherical k-means).
                    centroids[c] = normalize(&centroids[c]);
                }
            }
        }

        let mut lists: Vec<Vec<(u64, Vec<f32>)>> = alloc::vec![Vec::new(); nlist];
        let mut id_to_list = BTreeMap::new();
        for ((id, v), uv) in vectors.iter().zip(unit_vectors.iter()) {
            let c = nearest_cluster(&centroids, uv);
            lists[c].push((*id, v.clone()));
            id_to_list.insert(*id, c);
        }

        Self {
            dim,
            centroids,
            lists,
            id_to_list,
        }
    }

    /// Embedding dimension this index was built for.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of live rows tracked by the index.
    pub fn len(&self) -> usize {
        self.id_to_list.len()
    }

    /// Whether the index tracks zero rows.
    pub fn is_empty(&self) -> bool {
        self.id_to_list.is_empty()
    }

    /// Assigns `v` to its nearest cluster and adds it under `id`, replacing
    /// any prior entry for `id`.
    pub fn insert(&mut self, id: u64, v: &[f32]) {
        self.remove(id);
        let c = nearest_cluster(&self.centroids, &normalize(v));
        self.lists[c].push((id, v.to_vec()));
        self.id_to_list.insert(id, c);
    }

    /// Removes `id` from the index, if present.
    pub fn remove(&mut self, id: u64) {
        if let Some(c) = self.id_to_list.remove(&id) {
            self.lists[c].retain(|(rid, _)| *rid != id);
        }
    }

    /// Returns the row ids of the approximate `k` nearest neighbors to
    /// `query`, ranked exactly within the `nprobe` clusters whose centroid is
    /// closest to `query`. A true nearest neighbor in an unprobed cluster is
    /// missed — same recall trade as pgvector's IVFFlat.
    pub fn search(&self, query: &[f32], k: usize, nprobe: usize) -> Vec<u64> {
        if self.centroids.is_empty() {
            return Vec::new();
        }
        let nprobe = nprobe.clamp(1, self.centroids.len());
        // Cluster selection uses cosine direction (matching how the index
        // was built); the candidates gathered from those clusters are still
        // re-ranked below by raw inner product, matching `vector_topk`.
        let unit_query = normalize(query);
        let mut ranked_centroids: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, dot(c, &unit_query)))
            .collect();
        ranked_centroids
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));

        let mut scored: Vec<(u64, f32)> = Vec::new();
        for &(c, _) in ranked_centroids.iter().take(nprobe) {
            for (id, v) in &self.lists[c] {
                scored.push((*id, dot(v, query)));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));
        scored.into_iter().take(k).map(|(id, _)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corner_vectors(n: usize, dim: usize) -> Vec<(u64, Vec<f32>)> {
        (0..n)
            .map(|i| {
                let mut v = alloc::vec![0f32; dim];
                v[i % dim] = 1.0;
                (i as u64, v)
            })
            .collect()
    }

    #[test]
    fn finds_exact_match() {
        let vectors = corner_vectors(2000, 32);
        let idx = IvfFlatIndex::build(&vectors);
        let query = vectors[7].1.clone();
        // Probe enough clusters that the exact match is virtually guaranteed
        // to be found — this is a recall sanity check, not an exhaustiveness
        // proof (IVF is approximate by construction).
        let top = idx.search(&query, 1, 32);
        assert_eq!(top[0], 7);
    }

    /// One-hot vectors with `dim == n` so every row occupies its own unique
    /// dimension — self dot product is 1.0 and every other pair is exactly
    /// 0.0, so "is this the nearest neighbor to itself" is unambiguous and
    /// these tests aren't sensitive to k-means' cluster assignment.
    fn unique_one_hot_vectors(n: usize) -> Vec<(u64, Vec<f32>)> {
        corner_vectors(n, n)
    }

    #[test]
    fn insert_then_search_finds_it() {
        let mut vectors = unique_one_hot_vectors(300);
        let held_out = vectors.pop().unwrap();
        let mut idx = IvfFlatIndex::build(&vectors);
        assert_eq!(idx.len(), 299);
        idx.insert(held_out.0, &held_out.1);
        assert_eq!(idx.len(), 300);
        // Exhaustive probe (nprobe >= nlist): this test checks insert/search
        // mechanics, not IVF recall, so it shouldn't be sensitive to which
        // cluster k-means happened to assign the held-out vector to.
        let top = idx.search(&held_out.1, 1, 256);
        assert_eq!(top[0], held_out.0);
    }

    #[test]
    fn remove_drops_id_from_results() {
        let vectors = corner_vectors(1200, 16);
        let mut idx = IvfFlatIndex::build(&vectors);
        let target = vectors[3].clone();
        idx.remove(target.0);
        assert_eq!(idx.len(), 1199);
        let top = idx.search(&target.1, 5, 16);
        assert!(!top.contains(&target.0));
    }

    #[test]
    fn reinsert_moves_between_clusters() {
        // dim = n + 1 so there's a spare dimension no original vector
        // occupies — otherwise re-inserting id 0 onto another row's exact
        // slot would tie with that row and make "did it move" ambiguous.
        let vectors = corner_vectors(300, 301);
        let mut idx = IvfFlatIndex::build(&vectors);
        let mut moved = alloc::vec![0f32; 301];
        moved[300] = 1.0;
        idx.insert(0, &moved);
        assert_eq!(idx.len(), 300);
        let top = idx.search(&moved, 1, 256);
        assert_eq!(top[0], 0);
    }
}
