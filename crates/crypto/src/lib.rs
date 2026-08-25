//! Cryptographic primitives for the payment system's tamper-evidence layer.
//!
//! Pure (no database, no async): SHA-256 hashing, a Merkle tree with inclusion
//! proofs, and Ed25519 signing. The `workers` crate composes these into the
//! signed checkpoint chain described in DESIGN.md §6.

mod hash;
mod merkle;
mod signing;

pub use hash::{sha256, Hash};
pub use merkle::{leaf_hash, merkle_proof, merkle_root, verify_proof, ProofStep};
pub use signing::{verify_hash, Sealer, SigningError};
