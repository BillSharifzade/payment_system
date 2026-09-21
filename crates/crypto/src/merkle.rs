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
}
