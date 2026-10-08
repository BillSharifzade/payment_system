use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::MatcherError;
use crate::template::Template;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Hit {
    pub enrollment_id: Uuid,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct HttpMatcher {
    client: reqwest::Client,
    base: String,
}

#[derive(Serialize)]
struct EnrollBody<'a> {
    subject: Uuid,
    format: &'a str,
    template: String,
}

#[derive(Serialize)]
struct IdentifyBody<'a> {
    format: &'a str,
    template: String,
    limit: usize,
}

#[derive(Serialize)]
struct VerifyBody<'a> {
    format: &'a str,
    template: String,
    enrollment_ids: &'a [Uuid],
}

#[derive(Deserialize)]
struct HitsResponse {
    hits: Vec<Hit>,
}

impl HttpMatcher {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, MatcherError> {
        let base = base_url.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(MatcherError::Protocol(format!(
                "matcher url must be http(s), got {base_url:?}"
            )));
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(2)))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| MatcherError::Unavailable(e.to_string()))?;
        Ok(Self { client, base })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn enroll(
        &self,
        enrollment_id: Uuid,
        subject: Uuid,
        template: &Template,
    ) -> Result<(), MatcherError> {
        let resp = self
            .client
            .put(format!("{}/v1/templates/{enrollment_id}", self.base))
            .json(&EnrollBody {
                subject,
                format: template.format().as_str(),
                template: template.to_base64(),
            })
            .send()
            .await
            .map_err(|e| MatcherError::Unavailable(e.to_string()))?;
        Self::expect_success(resp, "enroll").await
    }

    pub async fn revoke(&self, enrollment_id: Uuid) -> Result<(), MatcherError> {
        let resp = self
            .client
            .delete(format!("{}/v1/templates/{enrollment_id}", self.base))
            .send()
            .await
            .map_err(|e| MatcherError::Unavailable(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::expect_success(resp, "revoke").await
    }

    pub async fn identify(&self, probe: &Template, limit: usize) -> Result<Vec<Hit>, MatcherError> {
        let body = IdentifyBody {
            format: probe.format().as_str(),
            template: probe.to_base64(),
            limit,
        };
        self.hits("identify", &body).await
    }

    pub async fn verify(
        &self,
        probe: &Template,
        enrollment_ids: &[Uuid],
    ) -> Result<Vec<Hit>, MatcherError> {
        let body = VerifyBody {
            format: probe.format().as_str(),
            template: probe.to_base64(),
            enrollment_ids,
        };
        self.hits("verify", &body).await
    }

    async fn hits<B: Serialize>(&self, op: &str, body: &B) -> Result<Vec<Hit>, MatcherError> {
        let resp = self
            .client
            .post(format!("{}/v1/{op}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| MatcherError::Unavailable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::status_error(status, op));
        }
        let body: HitsResponse = resp
            .json()
            .await
            .map_err(|e| MatcherError::Protocol(format!("{op} response: {e}")))?;
        Ok(body.hits)
    }

    pub async fn health(&self) -> Result<(), MatcherError> {
        let resp = self
            .client
            .get(format!("{}/health", self.base))
            .send()
            .await
            .map_err(|e| MatcherError::Unavailable(e.to_string()))?;
        Self::expect_success(resp, "health").await
    }

    fn status_error(status: reqwest::StatusCode, op: &str) -> MatcherError {
        if status.is_server_error() {
            MatcherError::Unavailable(format!("{op}: matcher returned {status}"))
        } else {
            MatcherError::Protocol(format!("{op}: matcher returned {status}"))
        }
    }

    async fn expect_success(resp: reqwest::Response, op: &str) -> Result<(), MatcherError> {
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(Self::status_error(status, op))
        }
    }
}
