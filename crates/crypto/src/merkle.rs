//! A SHA-256 Merkle tree with inclusion proofs.
//!
//! Used to commit a whole batch of transactions to a single 32-byte root. Any
//! change to any transaction in the batch changes the root, and a short
//! inclusion proof lets anyone verify a specific transaction is part of a
//! published root — the basis of the tamper-evidence checkpoints (DESIGN.md §6).
//!
//! Leaves and internal nodes are hashed with distinct domain-separation prefixes
//! (`0x00` / `0x01`) to prevent second-preimage attacks that conflate the two.

use crate::hash::{sha256, Hash};
use sha2::{Digest, Sha256};

/// Hash an arbitrary byte string into a Merkle *leaf*.
pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut buf = Vec::with_capacity(1 + data.len());
    buf.push(0x00);
    buf.extend_from_slice(data);
    sha256(&buf)
}

/// Hash two child hashes into a parent *node*.
fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update([0x01]);
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    Hash::from_bytes(hasher.finalize().into())
}

/// Compute the Merkle root of a list of leaf hashes. Returns `None` for an empty
/// list. With an odd number of nodes at any level, the last node is paired with
/// itself (Bitcoin-style).
pub fn merkle_root(leaves: &[Hash]) -> Option<Hash> {
    if leaves.is_empty() {
        return None;
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = &pair[0];
            let right = pair.get(1).unwrap_or(left); // duplicate last if odd
            next.push(node_hash(left, right));
        }
        level = next;
    }
    Some(level[0])
}

/// One step of an inclusion proof: a sibling hash and which side it sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofStep {
    pub sibling: Hash,
    /// True if the sibling is the *left* node (so our running hash is on the right).
    pub sibling_is_left: bool,
}

/// Build an inclusion proof for the leaf at `index`. Returns `None` if the index
/// is out of range.
pub fn merkle_proof(leaves: &[Hash], index: usize) -> Option<Vec<ProofStep>> {
    if index >= leaves.len() {
        return None;
    }
    let mut proof = Vec::new();
    let mut level = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        let sibling_idx = if idx.is_multiple_of(2) {
            idx + 1
        } else {
            idx - 1
        };
        // If the sibling is past the end (odd level), the node pairs with itself.
        let sibling = *level.get(sibling_idx).unwrap_or(&level[idx]);
        proof.push(ProofStep {
            sibling,
            sibling_is_left: sibling_idx < idx,
        });

        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = &pair[0];
            let right = pair.get(1).unwrap_or(left);
            next.push(node_hash(left, right));
        }
        level = next;
        idx /= 2;
    }
    Some(proof)
}

/// Verify that `leaf` is included under `root` given its proof.
pub fn verify_proof(leaf: Hash, proof: &[ProofStep], root: Hash) -> bool {
    let mut acc = leaf;
    for step in proof {
        acc = if step.sibling_is_left {
            node_hash(&step.sibling, &acc)
        } else {
            node_hash(&acc, &step.sibling)
        };
    }
    acc == root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Hash> {
        (0..n)
            .map(|i| leaf_hash(format!("tx-{i}").as_bytes()))
            .collect()
    }

    #[test]
    fn empty_has_no_root() {
        assert!(merkle_root(&[]).is_none());
    }

    #[test]
    fn single_leaf_root_is_itself() {
        let l = leaves(1);
        assert_eq!(merkle_root(&l).unwrap(), l[0]);
    }

    #[test]
    fn proofs_verify_for_every_leaf_various_sizes() {
        // Include odd counts to exercise the duplicate-last path.
        for n in [1usize, 2, 3, 4, 5, 7, 8, 16, 17] {
            let l = leaves(n);
            let root = merkle_root(&l).unwrap();
            for i in 0..n {
                let proof = merkle_proof(&l, i).unwrap();
                assert!(verify_proof(l[i], &proof, root), "n={n} i={i}");
            }
        }
    }

    #[test]
    fn tampering_breaks_the_root() {
        let l = leaves(8);
        let root = merkle_root(&l).unwrap();
        // Flip one leaf; the root must change.
        let mut tampered = l.clone();
        tampered[3] = leaf_hash(b"forged");
        assert_ne!(merkle_root(&tampered).unwrap(), root);
        // And the original proof for the tampered leaf must fail against the root.
        let proof = merkle_proof(&l, 3).unwrap();
        assert!(!verify_proof(leaf_hash(b"forged"), &proof, root));
    }

    #[test]
    fn wrong_proof_index_does_not_verify() {
        let l = leaves(8);
        let root = merkle_root(&l).unwrap();
        let proof = merkle_proof(&l, 2).unwrap();
        // Using leaf 5 with leaf 2's proof must fail.
        assert!(!verify_proof(l[5], &proof, root));
    }
}
