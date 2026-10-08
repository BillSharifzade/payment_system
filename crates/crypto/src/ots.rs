//! OpenTimestamps proofs in the reference implementation's format
//! (python-opentimestamps `.ots` files): parse, re-serialize byte for byte,
//! build, merge calendar responses, and evaluate the commitment operations.
//!
//! A proof is a tree. Its root message is the SHA-256 digest of the stamped
//! data; every edge is an operation (append/prepend bytes, hash) and every
//! node's message is the edge applied to its parent's. Parsing recomputes each
//! message, so a parsed proof IS a verified path from the digest to every
//! attestation. What parsing cannot do is check a Bitcoin attestation: its
//! message must equal the Merkle root in the header of the block at the
//! attested height, which needs that header (a Bitcoin node, or `ots verify`).

use std::cmp::Ordering;

use sha2::Digest as _;

pub const HEADER_MAGIC: &[u8; 31] =
    b"\x00OpenTimestamps\x00\x00Proof\x00\xbf\x89\xe2\xe8\x84\xe8\x92\x94";
const MAJOR_VERSION: u8 = 1;
const OP_SHA256: u8 = 0x08;
/// Limits of the reference implementation: no message or argument longer than
/// 4096 bytes, no tree deeper than 256, attestation payloads up to 8192 bytes.
const MAX_MSG_LEN: usize = 4096;
const MAX_DEPTH: usize = 256;
const MAX_PAYLOAD_LEN: usize = 8192;
const MAX_URI_LEN: usize = 1000;
const PENDING_TAG: [u8; 8] = [0x83, 0xdf, 0xe3, 0x0d, 0x2e, 0xf9, 0x0c, 0x8e];
const BITCOIN_TAG: [u8; 8] = [0x05, 0x88, 0x96, 0x0d, 0x73, 0xd7, 0x19, 0x01];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OtsError {
    #[error("malformed OpenTimestamps proof: {0}")]
    Malformed(&'static str),
    #[error("unsupported OpenTimestamps operation {0:#04x}")]
    Unsupported(u8),
    #[error("cannot merge timestamps of different messages")]
    MessageMismatch,
}

use OtsError::Malformed;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Sha1,
    Ripemd160,
    Sha256,
    Append(Vec<u8>),
    Prepend(Vec<u8>),
    Reverse,
    Hexlify,
}

impl Op {
    fn tag(&self) -> u8 {
        match self {
            Op::Sha1 => 0x02,
            Op::Ripemd160 => 0x03,
            Op::Sha256 => OP_SHA256,
            Op::Append(_) => 0xf0,
            Op::Prepend(_) => 0xf1,
            Op::Reverse => 0xf2,
            Op::Hexlify => 0xf3,
        }
    }

    fn arg(&self) -> &[u8] {
        match self {
            Op::Append(a) | Op::Prepend(a) => a,
            _ => &[],
        }
    }

    pub fn apply(&self, msg: &[u8]) -> Result<Vec<u8>, OtsError> {
        let max_in = if *self == Op::Hexlify {
            MAX_MSG_LEN / 2
        } else {
            MAX_MSG_LEN
        };
        if msg.len() > max_in {
            return Err(Malformed("message too long"));
        }
        let out = match self {
            Op::Sha1 => {
                use sha1::Digest as _;
                sha1::Sha1::digest(msg).to_vec()
            }
            Op::Ripemd160 => {
                use ripemd::Digest as _;
                ripemd::Ripemd160::digest(msg).to_vec()
            }
            Op::Sha256 => sha2::Sha256::digest(msg).to_vec(),
            Op::Append(a) => [msg, a].concat(),
            Op::Prepend(a) => [a, msg].concat(),
            Op::Reverse => msg.iter().rev().copied().collect(),
            Op::Hexlify => hex::encode(msg).into_bytes(),
        };
        if out.is_empty() || out.len() > MAX_MSG_LEN {
            return Err(Malformed("operation result empty or too long"));
        }
        Ok(out)
    }

    fn read(tag: u8, r: &mut Reader) -> Result<Self, OtsError> {
        Ok(match tag {
            0x02 => Op::Sha1,
            0x03 => Op::Ripemd160,
            OP_SHA256 => Op::Sha256,
            0xf0 => Op::Append(r.varbytes(1, MAX_MSG_LEN)?),
            0xf1 => Op::Prepend(r.varbytes(1, MAX_MSG_LEN)?),
            0xf2 => Op::Reverse,
            0xf3 => Op::Hexlify,
            other => return Err(OtsError::Unsupported(other)),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.tag());
        if let Op::Append(a) | Op::Prepend(a) = self {
            write_varbytes(out, a);
        }
    }
}

// The reference implementation serializes ops sorted by (tag, argument).
impl Ord for Op {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.tag(), self.arg()).cmp(&(other.tag(), other.arg()))
    }
}

impl PartialOrd for Op {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attestation {
    /// A calendar promised to commit this message to Bitcoin; ask it at `uri`.
    Pending(String),
    /// This message is the Merkle root of the Bitcoin block at this height.
    Bitcoin(u64),
    /// Any other notary (Litecoin, Ethereum, future ones), kept verbatim.
    Unknown([u8; 8], Vec<u8>),
}

impl Attestation {
    fn tag(&self) -> [u8; 8] {
        match self {
            Attestation::Pending(_) => PENDING_TAG,
            Attestation::Bitcoin(_) => BITCOIN_TAG,
            Attestation::Unknown(tag, _) => *tag,
        }
    }

    fn read(r: &mut Reader) -> Result<Self, OtsError> {
        let tag: [u8; 8] = r.bytes(8)?.try_into().expect("8 bytes");
        let payload = r.varbytes(0, MAX_PAYLOAD_LEN)?;
        let mut p = Reader(&payload);
        let att = match tag {
            PENDING_TAG => {
                let uri = p.varbytes(0, MAX_URI_LEN)?;
                let valid = |c: &u8| c.is_ascii_alphanumeric() || b"-._/:".contains(c);
                if !uri.iter().all(valid) {
                    return Err(Malformed("pending attestation URI has invalid characters"));
                }
                Attestation::Pending(String::from_utf8(uri).expect("ASCII"))
            }
            BITCOIN_TAG => Attestation::Bitcoin(p.varuint()?),
            _ => return Ok(Attestation::Unknown(tag, payload)),
        };
        if !p.0.is_empty() {
            return Err(Malformed("trailing bytes in attestation"));
        }
        Ok(att)
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tag());
        let mut payload = Vec::new();
        match self {
            Attestation::Pending(uri) => write_varbytes(&mut payload, uri.as_bytes()),
            Attestation::Bitcoin(height) => write_varuint(&mut payload, *height),
            Attestation::Unknown(_, raw) => payload.extend_from_slice(raw),
        }
        write_varbytes(out, &payload);
    }
}

impl Ord for Attestation {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tag()
            .cmp(&other.tag())
            .then_with(|| match (self, other) {
                (Attestation::Pending(a), Attestation::Pending(b)) => a.cmp(b),
                (Attestation::Bitcoin(a), Attestation::Bitcoin(b)) => a.cmp(b),
                (Attestation::Unknown(_, a), Attestation::Unknown(_, b)) => a.cmp(b),
                _ => Ordering::Equal,
            })
    }
}

impl PartialOrd for Attestation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], OtsError> {
        if self.0.len() < n {
            return Err(Malformed("truncated"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, OtsError> {
        Ok(self.bytes(1)?[0])
    }

    fn varuint(&mut self) -> Result<u64, OtsError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            let bits = u64::from(b & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(Malformed("varuint overflows 64 bits"));
            }
            value |= bits << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Malformed("varuint overflows 64 bits"))
    }

    fn varbytes(&mut self, min: usize, max: usize) -> Result<Vec<u8>, OtsError> {
        let len = usize::try_from(self.varuint()?).map_err(|_| Malformed("length"))?;
        if len < min || len > max {
            return Err(Malformed("length out of range"));
        }
        Ok(self.bytes(len)?.to_vec())
    }
}

fn write_varuint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8 & 0x7f) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn write_varbytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varuint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A commitment tree rooted at `msg`. Attestations and operations are kept in
/// the reference implementation's sort order, without duplicates, so
/// serialization is canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestamp {
    msg: Vec<u8>,
    attestations: Vec<Attestation>,
    ops: Vec<(Op, Timestamp)>,
}

impl Timestamp {
    pub fn new(msg: Vec<u8>) -> Self {
        Self {
            msg,
            attestations: Vec::new(),
            ops: Vec::new(),
        }
    }

    pub fn msg(&self) -> &[u8] {
        &self.msg
    }

    pub fn attest(&mut self, attestation: Attestation) {
        if let Err(i) = self.attestations.binary_search(&attestation) {
            self.attestations.insert(i, attestation);
        }
    }

    /// The subtree reached through `op`, created (and its message computed)
    /// if it does not exist yet.
    pub fn add_op(&mut self, op: Op) -> Result<&mut Timestamp, OtsError> {
        let i = match self.ops.binary_search_by(|(o, _)| o.cmp(&op)) {
            Ok(i) => i,
            Err(i) => {
                let msg = op.apply(&self.msg)?;
                self.ops.insert(i, (op, Timestamp::new(msg)));
                i
            }
        };
        Ok(&mut self.ops[i].1)
    }

    /// Adds every operation and attestation of `other` (same message) to this
    /// tree — how calendar responses and upgrades are combined.
    pub fn merge(&mut self, other: Timestamp) -> Result<(), OtsError> {
        if other.msg != self.msg {
            return Err(OtsError::MessageMismatch);
        }
        for a in other.attestations {
            self.attest(a);
        }
        for (op, sub) in other.ops {
            self.add_op(op)?.merge(sub)?;
        }
        Ok(())
    }

    /// Every attestation with the message it attests, depth first.
    pub fn attestations(&self) -> Vec<(&[u8], &Attestation)> {
        let mut out: Vec<(&[u8], &Attestation)> = self
            .attestations
            .iter()
            .map(|a| (self.msg.as_slice(), a))
            .collect();
        for (_, sub) in &self.ops {
            out.extend(sub.attestations());
        }
        out
    }

    /// The node whose message is `msg`, if any.
    pub fn find_mut(&mut self, msg: &[u8]) -> Option<&mut Timestamp> {
        if self.msg == msg {
            return Some(self);
        }
        self.ops.iter_mut().find_map(|(_, sub)| sub.find_mut(msg))
    }

    /// Bitcoin attestations as (block height, Merkle root in header byte
    /// order). The root must be 32 bytes to be one.
    pub fn bitcoin_attestations(&self) -> Result<Vec<(u64, [u8; 32])>, OtsError> {
        self.attestations()
            .into_iter()
            .filter_map(|(msg, a)| match a {
                Attestation::Bitcoin(h) => Some((*h, msg)),
                _ => None,
            })
            .map(|(h, msg)| {
                msg.try_into()
                    .map(|root| (h, root))
                    .map_err(|_| Malformed("Bitcoin attestation on a message that is not 32 bytes"))
            })
            .collect()
    }

    /// A serialized timestamp (a calendar response) for `msg`.
    pub fn parse(msg: Vec<u8>, bytes: &[u8]) -> Result<Self, OtsError> {
        let mut r = Reader(bytes);
        let ts = Self::read(&mut r, msg, MAX_DEPTH)?;
        if !r.0.is_empty() {
            return Err(Malformed("trailing bytes"));
        }
        Ok(ts)
    }

    fn read(r: &mut Reader, msg: Vec<u8>, depth: usize) -> Result<Self, OtsError> {
        if depth == 0 {
            return Err(Malformed("tree too deep"));
        }
        let mut ts = Timestamp::new(msg);
        let mut tag = r.u8()?;
        while tag == 0xff {
            let next = r.u8()?;
            ts.read_edge(r, next, depth)?;
            tag = r.u8()?;
        }
        ts.read_edge(r, tag, depth)?;
        Ok(ts)
    }

    fn read_edge(&mut self, r: &mut Reader, tag: u8, depth: usize) -> Result<(), OtsError> {
        if tag == 0x00 {
            self.attest(Attestation::read(r)?);
        } else {
            let op = Op::read(tag, r)?;
            let sub = Self::read(r, op.apply(&self.msg)?, depth - 1)?;
            self.add_op(op)?.merge(sub)?;
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        let Some((last_att, other_atts)) = self.attestations.split_last() else {
            return self.write_ops(out);
        };
        for a in other_atts {
            out.extend_from_slice(&[0xff, 0x00]);
            a.write(out);
        }
        if self.ops.is_empty() {
            out.push(0x00);
            last_att.write(out);
        } else {
            out.extend_from_slice(&[0xff, 0x00]);
            last_att.write(out);
            self.write_ops(out);
        }
    }

    fn write_ops(&self, out: &mut Vec<u8>) {
        for (i, (op, sub)) in self.ops.iter().enumerate() {
            if i + 1 < self.ops.len() {
                out.push(0xff);
            }
            op.write(out);
            sub.write(out);
        }
    }
}

/// A `.ots` file: header, the SHA-256 digest of the stamped data, its tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedTimestamp {
    pub timestamp: Timestamp,
}

impl DetachedTimestamp {
    pub fn new(digest: [u8; 32]) -> Self {
        Self {
            timestamp: Timestamp::new(digest.to_vec()),
        }
    }

    pub fn digest(&self) -> &[u8] {
        self.timestamp.msg()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, OtsError> {
        let mut r = Reader(bytes);
        if r.bytes(HEADER_MAGIC.len())? != HEADER_MAGIC {
            return Err(Malformed("not an OpenTimestamps proof"));
        }
        if r.u8()? != MAJOR_VERSION {
            return Err(Malformed("unsupported major version"));
        }
        match r.u8()? {
            OP_SHA256 => {}
            other => return Err(OtsError::Unsupported(other)),
        }
        let digest = r.bytes(32)?.to_vec();
        let timestamp = Timestamp::read(&mut r, digest, MAX_DEPTH)?;
        if !r.0.is_empty() {
            return Err(Malformed("trailing bytes"));
        }
        Ok(Self { timestamp })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = HEADER_MAGIC.to_vec();
        out.push(MAJOR_VERSION);
        out.push(OP_SHA256);
        out.extend_from_slice(self.timestamp.msg());
        self.timestamp.write(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/ots/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    // hello-world.txt(.ots) from the reference client's examples: stamped in
    // 2015, upgraded to Bitcoin block 358391. The proof carries the calendar's
    // Bitcoin transaction and the block's Merkle path.
    #[test]
    fn reference_complete_proof_parses_evaluates_and_round_trips() {
        let bytes = fixture("hello-world.txt.ots");
        let proof = DetachedTimestamp::parse(&bytes).unwrap();
        assert_eq!(
            proof.digest(),
            sha2::Sha256::digest(b"Hello World!\n").as_slice()
        );
        let bitcoin = proof.timestamp.bitcoin_attestations().unwrap();
        assert_eq!(bitcoin.len(), 1);
        let (height, root) = bitcoin[0];
        assert_eq!(height, 358_391);
        // Block 358391's Merkle root in header byte order (what `ots info`
        // prints); `bitcoin-cli getblockheader` shows it byte-reversed.
        assert_eq!(
            hex::encode(root),
            "007ee445d23ad061af4a36b809501fab1ac4f2d7e7a739817dd0cbb7ec661b8a"
        );
        assert_eq!(proof.to_bytes(), bytes, "canonical re-serialization");
        assert!(DetachedTimestamp::parse(&bytes[..bytes.len() - 1]).is_err());
        assert!(DetachedTimestamp::parse(&[bytes.as_slice(), &[0]].concat()).is_err());
    }

    #[test]
    fn reference_pending_proof_round_trips() {
        let bytes = fixture("incomplete.txt.ots");
        let proof = DetachedTimestamp::parse(&bytes).unwrap();
        let atts = proof.timestamp.attestations();
        assert_eq!(atts.len(), 1);
        assert_eq!(
            atts[0].1,
            &Attestation::Pending("https://alice.btc.calendar.opentimestamps.org".into())
        );
        assert!(proof.timestamp.bitcoin_attestations().unwrap().is_empty());
        assert_eq!(proof.to_bytes(), bytes);
    }

    #[test]
    fn build_merge_and_tamper() {
        let digest = [7u8; 32];
        let mut proof = DetachedTimestamp::new(digest);
        let commitment = {
            let node = proof.timestamp.add_op(Op::Append(vec![1, 2, 3])).unwrap();
            node.add_op(Op::Sha256).unwrap().msg().to_vec()
        };
        // Two calendars answer for the same commitment.
        let mut a = Timestamp::new(commitment.clone());
        a.add_op(Op::Prepend(vec![9]))
            .unwrap()
            .attest(Attestation::Pending("https://a.example".into()));
        let mut b = Timestamp::new(commitment.clone());
        b.add_op(Op::Prepend(vec![8]))
            .unwrap()
            .attest(Attestation::Pending("https://b.example".into()));
        let node = proof.timestamp.find_mut(&commitment).unwrap();
        node.merge(Timestamp::parse(commitment.clone(), &a.to_bytes()).unwrap())
            .unwrap();
        node.merge(b).unwrap();
        assert!(node.merge(Timestamp::new(vec![0])).is_err());
        assert_eq!(proof.timestamp.attestations().len(), 2);

        let bytes = proof.to_bytes();
        let parsed = DetachedTimestamp::parse(&bytes).unwrap();
        assert_eq!(parsed, proof);
        assert_eq!(parsed.to_bytes(), bytes);

        // Changing any byte on the path changes the attested messages.
        let mut tampered = bytes.clone();
        let at = HEADER_MAGIC.len() + 2 + 31; // last digest byte
        tampered[at] ^= 1;
        let other = DetachedTimestamp::parse(&tampered).unwrap();
        assert_ne!(
            other.timestamp.attestations()[0].0,
            proof.timestamp.attestations()[0].0
        );
    }

    #[test]
    fn limits_and_encodings() {
        for n in [0u64, 1, 127, 128, 300, 358_391, u64::MAX] {
            let mut buf = Vec::new();
            write_varuint(&mut buf, n);
            assert_eq!(Reader(&buf).varuint().unwrap(), n);
        }
        assert!(Reader(&[0xff; 11]).varuint().is_err());
        assert!(Op::Append(vec![0; MAX_MSG_LEN]).apply(&[1]).is_err());
        assert_eq!(Op::Hexlify.apply(&[0xab]).unwrap(), b"ab");
        assert!(Op::Hexlify.apply(&[0; MAX_MSG_LEN / 2 + 1]).is_err());
        // An attestation with nothing to attach to, an unknown op, a bad URI.
        assert!(Timestamp::parse(vec![1], &[]).is_err());
        assert_eq!(
            Timestamp::parse(vec![1], &[0x67]),
            Err(OtsError::Unsupported(0x67))
        );
        let mut bad_uri = vec![0x00];
        Attestation::Pending("https://x".into()).write(&mut bad_uri);
        let i = bad_uri.len() - 1;
        bad_uri[i] = b'?';
        assert!(Timestamp::parse(vec![1], &bad_uri).is_err());
        // Depth beyond the reference limit.
        let mut deep = vec![OP_SHA256; MAX_DEPTH];
        deep.push(0x00);
        Attestation::Bitcoin(1).write(&mut deep);
        assert!(Timestamp::parse(vec![1], &deep).is_err());
        assert!(Timestamp::parse(vec![1], &deep[1..]).is_ok());
    }
}
