use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crypto::rfc3161::{timestamp_request, verify_response, TsaTrust, MAX_RESPONSE_LEN};
use rand_core::RngCore;

use super::{anchor_imprint, read_capped, Anchor, AnchorError, Stamp, KIND_RFC3161};

/// A TSA's clock may differ from ours by this much at issuance.
pub const MAX_CLOCK_SKEW: Duration = Duration::from_secs(300);

/// An RFC 3161 timestamp authority. Every response is verified before it is
/// stored: our imprint and nonce, a signature chaining to the configured TSA
/// certificates, a genTime within MAX_CLOCK_SKEW of our clock.
pub struct TsaAnchor {
    url: String,
    trust: TsaTrust,
    http: reqwest::Client,
}

impl TsaAnchor {
    pub fn new(url: String, trust: TsaTrust, http: reqwest::Client) -> Self {
        Self { url, trust, http }
    }
}

impl Anchor for TsaAnchor {
    fn kind(&self) -> &'static str {
        KIND_RFC3161
    }

    fn witness(&self) -> &str {
        &self.url
    }

    async fn stamp(&self, checkpoint_hash: &[u8; 32]) -> Result<Stamp, AnchorError> {
        let imprint = anchor_imprint(checkpoint_hash);
        let nonce = rand_core::OsRng.next_u64();
        let resp = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/timestamp-query")
            .header("Accept", "application/timestamp-reply")
            .body(timestamp_request(&imprint, nonce))
            .send()
            .await
            .map_err(|e| AnchorError::Transport(format!("{}: {e}", self.url)))?;
        if !resp.status().is_success() {
            return Err(AnchorError::Witness(format!(
                "{} answered HTTP {}",
                self.url,
                resp.status()
            )));
        }
        let body = read_capped(resp, MAX_RESPONSE_LEN).await?;
        let verified = verify_response(&body, &imprint, Some(nonce), Some(&self.trust))
            .map_err(|e| AnchorError::Proof(format!("{}: {e}", self.url)))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        if verified.gen_time.abs_diff(now) > MAX_CLOCK_SKEW.as_secs() {
            return Err(AnchorError::Proof(format!(
                "{}: genTime {} is {}s from our clock",
                self.url,
                verified.gen_time,
                verified.gen_time - now
            )));
        }
        Ok(Stamp {
            complete: true,
            proof: body,
            attested_at: Some(verified.gen_time),
            bitcoin_height: None,
        })
    }
}
