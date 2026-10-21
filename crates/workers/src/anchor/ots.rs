use crypto::ots::{Attestation, DetachedTimestamp, Op, Timestamp};
use rand_core::RngCore;

use super::{anchor_imprint, read_capped, Anchor, AnchorError, Stamp, KIND_OTS, OTS_WITNESS};

/// Calendar responses are a few hundred bytes; the reference client reads at
/// most 10 000.
const MAX_CALENDAR_RESPONSE: usize = 10_000;
/// Public calendars (the reference client's default upgrade whitelist): a
/// pool's pending attestation names the backend calendar, not the pool.
const PUBLIC_CALENDAR_SUFFIXES: &[&str] = &[
    ".calendar.opentimestamps.org",
    ".calendar.eternitywall.com",
    ".calendar.catallaxy.com",
];

/// OpenTimestamps: the digest is submitted to every calendar and the merged
/// pending proof stored; `upgrade` later fetches each calendar's path to a
/// Bitcoin block. The proof is a standard `.ots` file for a file holding the
/// 32 checkpoint-hash bytes (`ots verify` checks it against a Bitcoin node).
pub struct OtsAnchor {
    calendars: Vec<String>,
    http: reqwest::Client,
}

fn split_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    Some((scheme, rest.split('/').next().unwrap_or_default()))
}

impl OtsAnchor {
    pub fn new(calendars: Vec<String>, http: reqwest::Client) -> Self {
        Self { calendars, http }
    }

    /// Only configured calendars and the public ones are asked for upgrades:
    /// the URI comes from a calendar response, and must not steer the worker
    /// to arbitrary hosts.
    fn may_upgrade_from(&self, uri: &str) -> bool {
        let Some((scheme, host)) = split_url(uri) else {
            return false;
        };
        self.calendars
            .iter()
            .any(|c| split_url(c) == Some((scheme, host)))
            || scheme == "https" && PUBLIC_CALENDAR_SUFFIXES.iter().any(|s| host.ends_with(s))
    }

    async fn submit(&self, calendar: &str, commitment: &[u8]) -> Result<Timestamp, AnchorError> {
        let resp = self
            .http
            .post(format!("{calendar}/digest"))
            .header("Accept", "application/vnd.opentimestamps.v1")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(commitment.to_vec())
            .send()
            .await
            .map_err(|e| AnchorError::Transport(format!("{calendar}: {e}")))?;
        if !resp.status().is_success() {
            return Err(AnchorError::Witness(format!(
                "{calendar} answered HTTP {}",
                resp.status()
            )));
        }
        let body = read_capped(resp, MAX_CALENDAR_RESPONSE).await?;
        let ts = Timestamp::parse(commitment.to_vec(), &body)
            .map_err(|e| AnchorError::Proof(format!("{calendar}: {e}")))?;
        if ts.attestations().is_empty() {
            return Err(AnchorError::Proof(format!(
                "{calendar}: response attests nothing"
            )));
        }
        Ok(ts)
    }

    /// None while the calendar has not committed `commitment` to a block yet.
    async fn fetch(
        &self,
        calendar: &str,
        commitment: &[u8],
    ) -> Result<Option<Timestamp>, AnchorError> {
        let resp = self
            .http
            .get(format!("{calendar}/timestamp/{}", hex::encode(commitment)))
            .header("Accept", "application/vnd.opentimestamps.v1")
            .send()
            .await
            .map_err(|e| AnchorError::Transport(format!("{calendar}: {e}")))?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => {
                let body = read_capped(resp, MAX_CALENDAR_RESPONSE).await?;
                Timestamp::parse(commitment.to_vec(), &body)
                    .map(Some)
                    .map_err(|e| AnchorError::Proof(format!("{calendar}: {e}")))
            }
            s => Err(AnchorError::Witness(format!(
                "{calendar} answered HTTP {s}"
            ))),
        }
    }
}

impl Anchor for OtsAnchor {
    fn kind(&self) -> &'static str {
        KIND_OTS
    }

    fn witness(&self) -> &str {
        OTS_WITNESS
    }

    async fn stamp(&self, checkpoint_hash: &[u8; 32]) -> Result<Stamp, AnchorError> {
        let mut proof = DetachedTimestamp::new(anchor_imprint(checkpoint_hash));
        // As `ots stamp` does: a random nonce keeps the calendars from
        // learning the digest.
        let mut nonce = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut nonce);
        let node = proof
            .timestamp
            .add_op(Op::Append(nonce.to_vec()))
            .and_then(|n| n.add_op(Op::Sha256))
            .map_err(|e| AnchorError::Proof(e.to_string()))?;
        let commitment = node.msg().to_vec();
        let answers =
            futures::future::join_all(self.calendars.iter().map(|c| self.submit(c, &commitment)))
                .await;
        let mut errors = Vec::new();
        for answer in answers {
            match answer {
                Ok(ts) => node
                    .merge(ts)
                    .map_err(|e| AnchorError::Proof(e.to_string()))?,
                Err(e) => errors.push(e.to_string()),
            }
        }
        if errors.len() == self.calendars.len() {
            return Err(AnchorError::Transport(errors.join("; ")));
        }
        if !errors.is_empty() {
            tracing::warn!(errors = ?errors, "some OpenTimestamps calendars failed; stored the others' proofs");
        }
        Ok(Stamp {
            complete: false,
            proof: proof.to_bytes(),
            attested_at: None,
            bitcoin_height: None,
        })
    }

    async fn upgrade(
        &self,
        checkpoint_hash: &[u8; 32],
        proof: &[u8],
    ) -> Result<Option<Stamp>, AnchorError> {
        let mut proof =
            DetachedTimestamp::parse(proof).map_err(|e| AnchorError::Proof(e.to_string()))?;
        if proof.digest() != anchor_imprint(checkpoint_hash) {
            return Err(AnchorError::Proof(
                "pending proof is for another digest".into(),
            ));
        }
        let pending: Vec<(Vec<u8>, String)> = proof
            .timestamp
            .attestations()
            .into_iter()
            .filter_map(|(msg, a)| match a {
                Attestation::Pending(uri) => Some((msg.to_vec(), uri.clone())),
                _ => None,
            })
            .collect();
        let mut errors = Vec::new();
        for (commitment, uri) in &pending {
            if !self.may_upgrade_from(uri) {
                tracing::warn!(%uri, "not upgrading from a calendar outside ANCHOR_OTS_CALENDARS and the public pools");
                continue;
            }
            match self.fetch(uri, commitment).await {
                Ok(Some(ts)) => proof
                    .timestamp
                    .find_mut(commitment)
                    .expect("attested message is in the tree")
                    .merge(ts)
                    .map_err(|e| AnchorError::Proof(e.to_string()))?,
                Ok(None) => {}
                Err(e) => errors.push(e.to_string()),
            }
        }
        let bitcoin = proof
            .timestamp
            .bitcoin_attestations()
            .map_err(|e| AnchorError::Proof(e.to_string()))?;
        match bitcoin.iter().map(|(h, _)| *h).min() {
            Some(height) => Ok(Some(Stamp {
                complete: true,
                proof: proof.to_bytes(),
                attested_at: None,
                bitcoin_height: Some(height as i64),
            })),
            None if !errors.is_empty() && errors.len() == pending.len() => {
                Err(AnchorError::Transport(errors.join("; ")))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_only_from_known_calendars() {
        let a = OtsAnchor::new(
            vec![
                "https://a.pool.opentimestamps.org".into(),
                "http://127.0.0.1:9000".into(),
            ],
            reqwest::Client::new(),
        );
        assert!(a.may_upgrade_from("https://alice.btc.calendar.opentimestamps.org"));
        assert!(a.may_upgrade_from("https://a.pool.opentimestamps.org"));
        assert!(a.may_upgrade_from("http://127.0.0.1:9000"));
        assert!(!a.may_upgrade_from("http://127.0.0.1:9001"));
        assert!(!a.may_upgrade_from("http://alice.btc.calendar.opentimestamps.org"));
        assert!(!a.may_upgrade_from("https://evil.example/calendar.opentimestamps.org"));
        assert!(!a.may_upgrade_from("https://169.254.169.254"));
    }
}
