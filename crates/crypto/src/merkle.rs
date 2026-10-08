use crate::hash::{sha256, Hash};
use sha2::{Digest, Sha256};

pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut buf = Vec::with_capacity(1 + data.len());
    buf.push(0x00);
    buf.extend_from_slice(data);
    sha256(&buf)
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update([0x01]);
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    Hash::from_bytes(hasher.finalize().into())
}

pub fn merkle_root(leaves: &[Hash]) -> Option<Hash> {
    if leaves.is_empty() {
        return None;
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = &pair[0];
            let right = pair.get(1).unwrap_or(left);
            next.push(node_hash(left, right));
        }
        level = next;
    }
    Some(level[0])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofStep {
    pub sibling: Hash,
    pub sibling_is_left: bool,
}

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

/// Verifies a proof against `root` only. It does not bind the leaf's position: an
/// odd level pairs its last node with itself, so `merkle_root([a, b, c]) ==
/// merkle_root([a, b, c, c])` and a self-paired step verifies with either
/// direction bit. Auditors should use [`verify_proof_at`].
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

/// Verifies that `leaf` is leaf number `index` of a tree of `leaf_count` leaves with
/// root `root`: every direction bit is derived from `index`, a self-paired step's
/// sibling must be the running hash itself, and the proof has exactly the tree's height.
/// `leaf_count` is only as trustworthy as its source: it is not committed in the root.
pub fn verify_proof_at(
    leaf: Hash,
    index: usize,
    leaf_count: usize,
    proof: &[ProofStep],
    root: Hash,
) -> bool {
    if index >= leaf_count {
        return false;
    }
    let (mut acc, mut idx, mut len) = (leaf, index, leaf_count);
    let mut steps = proof.iter();
    while len > 1 {
        let Some(step) = steps.next() else {
            return false;
        };
        let self_paired = idx == len - 1 && len % 2 == 1;
        if step.sibling_is_left != (idx % 2 == 1) || (self_paired && step.sibling != acc) {
            return false;
        }
        acc = if step.sibling_is_left {
            node_hash(&step.sibling, &acc)
        } else {
            node_hash(&acc, &step.sibling)
        };
        idx /= 2;
        len = len.div_ceil(2);
    }
    steps.next().is_none() && acc == root
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
        let mut tampered = l.clone();
        tampered[3] = leaf_hash(b"forged");
        assert_ne!(merkle_root(&tampered).unwrap(), root);
        let proof = merkle_proof(&l, 3).unwrap();
        assert!(!verify_proof(leaf_hash(b"forged"), &proof, root));
    }

    #[test]
    fn wrong_proof_index_does_not_verify() {
        let l = leaves(8);
        let root = merkle_root(&l).unwrap();
        let proof = merkle_proof(&l, 2).unwrap();
        assert!(!verify_proof(l[5], &proof, root));
    }

    #[test]
    fn a_proof_binds_its_index() {
        for n in 1..=9 {
            let ls = leaves(n);
            let root = merkle_root(&ls).unwrap();
            for k in 0..n {
                let p = merkle_proof(&ls, k).unwrap();
                assert!(verify_proof_at(ls[k], k, n, &p, root), "leaf {k} of {n}");
                for other in (0..n + 2).filter(|&o| o != k) {
                    assert!(
                        !verify_proof_at(ls[k], other, n, &p, root),
                        "{k} as {other} of {n}"
                    );
                }
                for s in 0..p.len() {
                    let mut flipped = p.clone();
                    flipped[s].sibling_is_left ^= true;
                    assert!(
                        !verify_proof_at(ls[k], k, n, &flipped, root),
                        "flip {s} of leaf {k}/{n}"
                    );
                }
            }
        }
        // The duplicate-last-leaf ambiguity verify_proof cannot see.
        let ls = leaves(3);
        let root = merkle_root(&ls).unwrap();
        let mut dup = ls.clone();
        dup.push(ls[2]);
        assert_eq!(merkle_root(&dup).unwrap(), root);
        let p = merkle_proof(&dup, 3).unwrap();
        assert!(verify_proof(ls[2], &p, root));
        assert!(!verify_proof_at(ls[2], 3, 3, &p, root));
        let mut flipped = merkle_proof(&ls, 2).unwrap();
        flipped[0].sibling_is_left ^= true;
        assert!(verify_proof(ls[2], &flipped, root));
        assert!(!verify_proof_at(ls[2], 2, 3, &flipped, root));
    }
}
