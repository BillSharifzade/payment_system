//! Merkle trees (checkpoint roots): every leaf's proof verifies against the root, has one step
//! per level and its direction bits spell the leaf's index; a different leaf, another leaf's
//! proof, a mutated sibling, root or proof length never verify.
//!
//! `verify_proof` has a known gap, asserted narrowly: an odd level pairs its last node with
//! itself, so flipping the direction bit of that step still verifies (and `merkle_root` does not
//! bind the leaf count: [a, b, c] and [a, b, c, c] share a root). The target checks that such a
//! flip verifies only at a self-paired step, and that `verify_proof_at` — which derives every
//! direction from the index — accepts each honest proof and refuses every flip.
#![no_main]

use arbitrary::Arbitrary;
use crypto::{
    leaf_hash, merkle_proof, merkle_root, verify_proof, verify_proof_at, Hash, ProofStep,
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Tamper {
    Leaf(u16),
    OtherProof(u16),
    Sibling(u8, u8, u8),
    Flip(u8),
    Truncate(u8),
    Extend([u8; 32], bool),
    Root(u8, u8),
}

#[derive(Arbitrary, Debug)]
struct Input {
    leaves: Vec<u8>,
    index: u16,
    tamper: Tamper,
}

fn levels(n: usize) -> usize {
    n.next_power_of_two().trailing_zeros() as usize
}

fn node(left: &Hash, right: &Hash) -> Hash {
    let mut buf = vec![0x01];
    buf.extend_from_slice(left.as_bytes());
    buf.extend_from_slice(right.as_bytes());
    crypto::sha256(&buf)
}

fuzz_target!(|input: Input| {
    assert_eq!(merkle_root(&[]), None);
    let data = &input.leaves[..input.leaves.len().min(40)];
    if data.is_empty() {
        return;
    }
    let leaves: Vec<Hash> = data.iter().map(|b| leaf_hash(&[*b])).collect();
    let n = leaves.len();
    let root = merkle_root(&leaves).unwrap();
    assert_eq!(merkle_proof(&leaves, n), None);

    let proofs: Vec<Vec<ProofStep>> = (0..n).map(|i| merkle_proof(&leaves, i).unwrap()).collect();
    for (i, proof) in proofs.iter().enumerate() {
        assert!(verify_proof(leaves[i], proof, root), "leaf {i} of {n}");
        assert!(
            verify_proof_at(leaves[i], i, n, proof, root),
            "leaf {i} of {n} at its index"
        );
        assert!(
            !verify_proof_at(leaves[i], (i + 1) % n.max(2), n, proof, root),
            "leaf {i} at another index"
        );
        assert_eq!(proof.len(), levels(n));
        let spelled: usize = proof
            .iter()
            .enumerate()
            .map(|(k, s)| (s.sibling_is_left as usize) << k)
            .sum();
        assert_eq!(spelled, i, "direction bits of leaf {i}");
    }

    let k = input.index as usize % n;
    let (leaf, proof) = (leaves[k], &proofs[k]);
    let step_at = |s: u8| (!proof.is_empty()).then(|| s as usize % proof.len());
    match input.tamper {
        Tamper::Leaf(j) => {
            let other = leaves[j as usize % n];
            if other != leaf {
                assert!(!verify_proof(other, proof, root));
            }
        }
        Tamper::OtherProof(j) => {
            let j = j as usize % n;
            if leaves[j] != leaf {
                assert!(!verify_proof(leaf, &proofs[j], root));
            }
        }
        Tamper::Sibling(s, byte, x) => {
            if let (Some(s), true) = (step_at(s), x != 0) {
                let mut p = proof.clone();
                let mut bytes = *p[s].sibling.as_bytes();
                bytes[byte as usize % 32] ^= x;
                p[s].sibling = Hash::from_bytes(bytes);
                assert!(!verify_proof(leaf, &p, root));
            }
        }
        Tamper::Flip(s) => {
            if let Some(s) = step_at(s) {
                let mut p = proof.clone();
                p[s].sibling_is_left ^= true;
                assert!(
                    !verify_proof_at(leaf, k, n, &p, root),
                    "a flipped direction verified at its index"
                );
                if verify_proof(leaf, &p, root) {
                    let acc = proof[..s].iter().fold(leaf, |acc, st| {
                        if st.sibling_is_left {
                            node(&st.sibling, &acc)
                        } else {
                            node(&acc, &st.sibling)
                        }
                    });
                    assert_eq!(
                        proof[s].sibling, acc,
                        "a flipped direction verified at a step with a real sibling"
                    );
                }
            }
        }
        Tamper::Truncate(len) => {
            let len = len as usize;
            if len < proof.len() {
                assert!(!verify_proof(leaf, &proof[..len], root));
            }
        }
        Tamper::Extend(h, left) => {
            let mut p = proof.clone();
            p.push(ProofStep {
                sibling: Hash::from_bytes(h),
                sibling_is_left: left,
            });
            assert!(!verify_proof(leaf, &p, root));
        }
        Tamper::Root(byte, x) => {
            if x != 0 {
                let mut bytes = *root.as_bytes();
                bytes[byte as usize % 32] ^= x;
                assert!(!verify_proof(leaf, proof, Hash::from_bytes(bytes)));
            }
        }
    }
});
