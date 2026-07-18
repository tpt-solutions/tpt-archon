//! A B-Link tree: an ordered `u64 -> Vec<u8>` index with sibling right-links.
//!
//! This is a Lehman-&-Yao–style B-Link tree. Every node has a *right-link*
//! pointer to its next sibling and a *high key*; a reader that arrives at a node
//! which has since been split can follow the right-link to find the correct
//! node without holding a latch on the parent. That is what makes reads
//! latch-free in a concurrent setting. This implementation is single-threaded
//! (arena-backed) but keeps the right-link/high-key structure so the concurrent
//! version is a drop-in evolution rather than a rewrite.
//!
//! # Compile-time capacity invariant
//!
//! [`BTree::CAPACITY`] is checked at compile time to fit within
//! [`PAGE_SIZE`](crate::page::PAGE_SIZE): the const assertion in
//! [`assert_node_fits_page`] fails to compile if a node's maximum on-page size
//! could exceed a page. This is the role `tpt-eidos` is intended to play; until
//! that dependency is wired in, the guarantee is a `const` panic in a `const`
//! context (a genuine compile-time check).

use alloc::vec::Vec;

use crate::page::PAGE_SIZE;

/// Keys are fixed-width `u64`; values are arbitrary byte strings.
pub type Key = u64;

/// The maximum number of keys per node.
///
/// Chosen so a leaf's worst-case on-page footprint (keys + fixed-size value
/// slots + node header + right-link) fits in a [`PAGE_SIZE`] page. See
/// [`assert_node_fits_page`].
pub const NODE_CAPACITY: usize = 64;

/// Bytes reserved per value slot for the on-page fit calculation.
const VALUE_SLOT_BYTES: usize = 48;
/// Node header: node-type byte + key count + right-link pointer.
const NODE_HEADER_BYTES: usize = 1 + 2 + 8;

/// Compile-time proof that a full node fits within one page.
///
/// Referencing this const in a `const` context forces evaluation; the
/// `assert!` runs at compile time and fails the build if the invariant breaks.
pub const fn assert_node_fits_page() -> usize {
    let per_key = 8 + VALUE_SLOT_BYTES; // key + value slot
    let max = NODE_HEADER_BYTES + NODE_CAPACITY * per_key;
    assert!(
        max <= PAGE_SIZE,
        "B-Link node maximum size must not exceed PAGE_SIZE"
    );
    max
}

// Force the const assertion to be evaluated at compile time.
const _NODE_FITS: usize = assert_node_fits_page();

type NodeId = usize;

#[derive(Debug)]
enum Node {
    Leaf {
        keys: Vec<Key>,
        vals: Vec<Vec<u8>>,
        /// Right-link to the next leaf (Lehman & Yao), or `None` at the end.
        right: Option<NodeId>,
        /// High key: the smallest key *not* covered by this node.
        high_key: Option<Key>,
    },
    Internal {
        keys: Vec<Key>,
        children: Vec<NodeId>,
        right: Option<NodeId>,
        high_key: Option<Key>,
    },
}

/// A B-Link tree mapping [`Key`]s to byte-string values.
#[derive(Debug)]
pub struct BTree {
    nodes: Vec<Node>,
    root: NodeId,
}

impl Default for BTree {
    fn default() -> Self {
        Self::new()
    }
}

impl BTree {
    /// The per-node key capacity, exposed for callers/tests.
    pub const CAPACITY: usize = NODE_CAPACITY;

    /// Creates an empty tree (a single empty leaf as the root).
    pub fn new() -> Self {
        let nodes = alloc::vec![Node::Leaf {
            keys: Vec::new(),
            vals: Vec::new(),
            right: None,
            high_key: None,
        }];
        Self { nodes, root: 0 }
    }

    /// Looks up `key`, returning its value if present.
    pub fn get(&self, key: Key) -> Option<&[u8]> {
        let mut id = self.root;
        loop {
            match &self.nodes[id] {
                Node::Internal {
                    keys,
                    children,
                    right,
                    high_key,
                } => {
                    // If key is beyond this node's coverage, follow the right-link.
                    if let Some(hk) = high_key {
                        if key >= *hk {
                            if let Some(r) = right {
                                id = *r;
                                continue;
                            }
                        }
                    }
                    let idx = match keys.binary_search(&key) {
                        Ok(i) => i + 1,
                        Err(i) => i,
                    };
                    id = children[idx];
                }
                Node::Leaf {
                    keys,
                    vals,
                    right,
                    high_key,
                } => {
                    if let Some(hk) = high_key {
                        if key >= *hk {
                            if let Some(r) = right {
                                id = *r;
                                continue;
                            }
                        }
                    }
                    return match keys.binary_search(&key) {
                        Ok(i) => Some(&vals[i]),
                        Err(_) => None,
                    };
                }
            }
        }
    }

    /// Whether `key` is present.
    pub fn contains(&self, key: Key) -> bool {
        self.get(key).is_some()
    }

    /// Returns all `(key, value)` pairs with `lo <= key < hi`, in key order.
    ///
    /// Uses the leaf right-links to scan sequentially without revisiting the
    /// upper levels.
    pub fn range(&self, lo: Key, hi: Key) -> Vec<(Key, Vec<u8>)> {
        let mut out = Vec::new();
        if lo >= hi {
            return out;
        }
        // Descend to the leaf that should contain `lo`.
        let mut id = self.leftmost_leaf_for(lo);
        loop {
            match &self.nodes[id] {
                Node::Leaf {
                    keys, vals, right, ..
                } => {
                    for (i, &k) in keys.iter().enumerate() {
                        if k >= hi {
                            return out;
                        }
                        if k >= lo {
                            out.push((k, vals[i].clone()));
                        }
                    }
                    match right {
                        Some(r) => id = *r,
                        None => return out,
                    }
                }
                _ => unreachable!("leftmost_leaf_for returns a leaf"),
            }
        }
    }

    fn leftmost_leaf_for(&self, key: Key) -> NodeId {
        let mut id = self.root;
        loop {
            match &self.nodes[id] {
                Node::Internal { keys, children, .. } => {
                    let idx = match keys.binary_search(&key) {
                        Ok(i) => i + 1,
                        Err(i) => i,
                    };
                    id = children[idx];
                }
                Node::Leaf { .. } => return id,
            }
        }
    }

    /// Inserts or replaces `key -> value`.
    pub fn insert(&mut self, key: Key, value: Vec<u8>) {
        let root = self.root;
        if let Some(split) = self.insert_rec(root, key, value) {
            // Root split: create a new root above the two halves.
            let (sep_key, right_id) = split;
            let new_root = Node::Internal {
                keys: alloc::vec![sep_key],
                children: alloc::vec![self.root, right_id],
                right: None,
                high_key: None,
            };
            self.nodes.push(new_root);
            self.root = self.nodes.len() - 1;
        }
    }

    /// Returns `Some((separator_key, new_right_node_id))` if `id` split.
    fn insert_rec(&mut self, id: NodeId, key: Key, value: Vec<u8>) -> Option<(Key, NodeId)> {
        match &mut self.nodes[id] {
            Node::Leaf { keys, vals, .. } => {
                match keys.binary_search(&key) {
                    Ok(i) => {
                        vals[i] = value; // replace
                        None
                    }
                    Err(i) => {
                        keys.insert(i, key);
                        vals.insert(i, value);
                        if keys.len() > NODE_CAPACITY {
                            Some(self.split_leaf(id))
                        } else {
                            None
                        }
                    }
                }
            }
            Node::Internal { keys, children, .. } => {
                let idx = match keys.binary_search(&key) {
                    Ok(i) => i + 1,
                    Err(i) => i,
                };
                let child = children[idx];
                if let Some((sep, right_id)) = self.insert_rec(child, key, value) {
                    if let Node::Internal { keys, children, .. } = &mut self.nodes[id] {
                        let pos = match keys.binary_search(&sep) {
                            Ok(i) => i,
                            Err(i) => i,
                        };
                        keys.insert(pos, sep);
                        children.insert(pos + 1, right_id);
                        if keys.len() > NODE_CAPACITY {
                            return Some(self.split_internal(id));
                        }
                    }
                }
                None
            }
        }
    }

    fn split_leaf(&mut self, id: NodeId) -> (Key, NodeId) {
        let mid = NODE_CAPACITY.div_ceil(2);
        let (r_keys, r_vals, old_right, old_high, sep) = match &mut self.nodes[id] {
            Node::Leaf {
                keys,
                vals,
                right,
                high_key,
            } => {
                let r_keys = keys.split_off(mid);
                let r_vals = vals.split_off(mid);
                let sep = r_keys[0];
                let old_right = right.take();
                let old_high = *high_key;
                *high_key = Some(sep);
                (r_keys, r_vals, old_right, old_high, sep)
            }
            _ => unreachable!(),
        };
        let new_id = self.nodes.len();
        self.nodes.push(Node::Leaf {
            keys: r_keys,
            vals: r_vals,
            right: old_right,
            high_key: old_high,
        });
        // Link the left node's right pointer to the new node.
        if let Node::Leaf { right, .. } = &mut self.nodes[id] {
            *right = Some(new_id);
        }
        (sep, new_id)
    }

    fn split_internal(&mut self, id: NodeId) -> (Key, NodeId) {
        let mid = NODE_CAPACITY.div_ceil(2);
        let (sep, r_keys, r_children, old_right, old_high) = match &mut self.nodes[id] {
            Node::Internal {
                keys,
                children,
                right,
                high_key,
            } => {
                let sep = keys[mid];
                let r_keys = keys.split_off(mid + 1);
                keys.pop(); // remove the separator that moves up
                let r_children = children.split_off(mid + 1);
                let old_right = right.take();
                let old_high = *high_key;
                *high_key = Some(sep);
                (sep, r_keys, r_children, old_right, old_high)
            }
            _ => unreachable!(),
        };
        let new_id = self.nodes.len();
        self.nodes.push(Node::Internal {
            keys: r_keys,
            children: r_children,
            right: old_right,
            high_key: old_high,
        });
        if let Node::Internal { right, .. } = &mut self.nodes[id] {
            *right = Some(new_id);
        }
        (sep, new_id)
    }

    /// Total number of key/value pairs (walks the leaf right-link chain).
    pub fn len(&self) -> usize {
        let mut id = self.leftmost_leaf();
        let mut n = 0;
        loop {
            match &self.nodes[id] {
                Node::Leaf { keys, right, .. } => {
                    n += keys.len();
                    match right {
                        Some(r) => id = *r,
                        None => return n,
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn leftmost_leaf(&self) -> NodeId {
        let mut id = self.root;
        loop {
            match &self.nodes[id] {
                Node::Internal { children, .. } => id = children[0],
                Node::Leaf { .. } => return id,
            }
        }
    }

    /// Debug/verification helper: checks the structural invariant that every
    /// leaf's keys are sorted and each leaf's keys are all `< high_key`.
    #[cfg(test)]
    fn check_invariants(&self) -> bool {
        let mut id = self.leftmost_leaf();
        let mut prev: Option<Key> = None;
        loop {
            match &self.nodes[id] {
                Node::Leaf {
                    keys,
                    right,
                    high_key,
                    vals,
                } => {
                    if keys.len() != vals.len() {
                        return false;
                    }
                    for w in keys.windows(2) {
                        if w[0] >= w[1] {
                            return false;
                        }
                    }
                    if let (Some(p), Some(&first)) = (prev, keys.first()) {
                        if first <= p {
                            return false;
                        }
                    }
                    if let (Some(hk), Some(&last)) = (high_key, keys.last()) {
                        if last >= *hk {
                            return false;
                        }
                    }
                    prev = keys.last().copied();
                    match right {
                        Some(r) => id = *r,
                        None => return true,
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_capacity_fits_page() {
        assert!(assert_node_fits_page() <= PAGE_SIZE);
    }

    #[test]
    fn insert_and_point_lookup() {
        let mut t = BTree::new();
        t.insert(5, alloc::vec![50]);
        t.insert(1, alloc::vec![10]);
        t.insert(9, alloc::vec![90]);
        assert_eq!(t.get(1), Some(&[10][..]));
        assert_eq!(t.get(5), Some(&[50][..]));
        assert_eq!(t.get(9), Some(&[90][..]));
        assert_eq!(t.get(2), None);
        assert!(t.contains(5));
        assert!(!t.contains(2));
    }

    #[test]
    fn insert_replaces_existing() {
        let mut t = BTree::new();
        t.insert(7, alloc::vec![1]);
        t.insert(7, alloc::vec![2]);
        assert_eq!(t.get(7), Some(&[2][..]));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn many_inserts_trigger_splits_and_stay_consistent() {
        let mut t = BTree::new();
        // Insert enough to force multiple levels of splits.
        for k in 0..1000u64 {
            t.insert(k, alloc::vec![(k % 256) as u8]);
        }
        assert_eq!(t.len(), 1000);
        assert!(t.check_invariants());
        for k in 0..1000u64 {
            assert_eq!(t.get(k), Some(&[(k % 256) as u8][..]), "missing key {k}");
        }
        assert_eq!(t.get(1000), None);
    }

    #[test]
    fn reverse_order_inserts() {
        let mut t = BTree::new();
        for k in (0..500u64).rev() {
            t.insert(k, alloc::vec![1]);
        }
        assert_eq!(t.len(), 500);
        assert!(t.check_invariants());
        for k in 0..500u64 {
            assert!(t.contains(k));
        }
    }

    #[test]
    fn range_scan_via_right_links() {
        let mut t = BTree::new();
        for k in 0..300u64 {
            t.insert(k, alloc::vec![k as u8]);
        }
        let r = t.range(100, 110);
        let keys: Vec<Key> = r.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, (100..110).collect::<Vec<_>>());

        // Empty and full ranges.
        assert!(t.range(10, 10).is_empty());
        assert_eq!(t.range(0, 300).len(), 300);
        assert_eq!(t.range(295, 1000).len(), 5);
    }
}
