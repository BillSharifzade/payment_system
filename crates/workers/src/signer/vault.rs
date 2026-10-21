//! HashiCorp Vault transit signer: an `ed25519` transit key signs the
//! checkpoint hash (`POST /v1/<transit>/sign/<key>`), so the private key stays
//! inside Vault. The worker's token can read the key's public half and sign —
//! nothing else (deploy/vault/checkpoint-signer.hcl).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use crypto::Hash;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{CheckpointSigner, SignerError};

#[derive(Clone)]
pub enum VaultAuth {
    Token(String),
    TokenFile(PathBuf),
    AppRole {
        mount: String,
        role_id: String,
        secret_id: String,
    },
}

// Credentials never reach logs.
impl std::fmt::Debug for VaultAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultAuth::Token(_) => f.write_str("Token(<redacted>)"),
            VaultAuth::TokenFile(p) => write!(f, "TokenFile({})", p.display()),
            VaultAuth::AppRole { mount, .. } => write!(f, "AppRole(auth/{mount})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VaultConfig {
    pub addr: String,
    pub transit_mount: String,
    pub key_name: String,
    pub auth: VaultAuth,
    pub ca_cert: Option<Vec<u8>>,
    /// Per attempt.
    pub timeout: Duration,
    /// Further attempts after a failure worth retrying (transport errors,
    /// timeouts, 429, 5xx — a sealed or standby Vault answers 503).
    pub retries: u32,
}

enum SendError {
    /// 403: the token expired or was revoked (or lacks the policy).
    Denied(String),
    Failed(SignerError),
}
use SendError::{Denied, Failed};

impl From<SendError> for SignerError {
    fn from(e: SendError) -> Self {
        match e {
            Denied(msg) => SignerError::Rejected(msg),
            Failed(e) => e,
        }
    }
}

struct Token {
    value: String,
    renew_after: Option<Instant>,
}

pub struct VaultSigner {
    http: reqwest::Client,
    cfg: VaultConfig,
    token: Mutex<Option<Token>>,
    public_key: [u8; 32],
    key_version: u64,
}

fn vault_errors(body: &Value) -> String {
    body["errors"]
        .as_array()
        .map(|e| {
            e.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "no error detail".into())
}

impl VaultSigner {
    /// Authenticates, loads the key's newest version and public key, and makes
    /// one test signature, so a misconfiguration fails here and not at the
    /// first checkpoint.
    pub async fn connect(cfg: VaultConfig) -> Result<Self, SignerError> {
        let mut http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .connect_timeout(cfg.timeout);
        if let Some(pem) = &cfg.ca_cert {
            for cert in reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| SignerError::Rejected(format!("VAULT_CACERT: {e}")))?
            {
                http = http.add_root_certificate(cert);
            }
        }
        let http = http
            .build()
            .map_err(|e| SignerError::Rejected(format!("HTTP client: {e}")))?;
        let mut signer = Self {
            http,
            cfg,
            token: Mutex::new(None),
            public_key: [0; 32],
            key_version: 0,
        };

        let path = format!("{}/keys/{}", signer.cfg.transit_mount, signer.cfg.key_name);
        let key = signer.call(Method::GET, &path, None).await?;
        let data = &key["data"];
        if data["type"] != "ed25519" {
            return Err(SignerError::Rejected(format!(
                "transit key {} is {}, not ed25519",
                signer.cfg.key_name, data["type"]
            )));
        }
        if data["exportable"] == true || data["allow_plaintext_backup"] == true {
            tracing::warn!(key = %signer.cfg.key_name, "the Vault transit key is EXPORTABLE or allows plaintext backup: its private half can leave Vault. Recreate it with exportable=false (deploy/vault/bootstrap.sh)");
        }
        let version = data["latest_version"]
            .as_u64()
            .ok_or_else(|| SignerError::Rejected("transit key has no latest_version".into()))?;
        let public_key = data["keys"][version.to_string()]["public_key"]
            .as_str()
            .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
            .ok_or_else(|| {
                SignerError::Rejected(format!(
                    "transit key version {version} has no Ed25519 public key"
                ))
            })?;
        signer.public_key = public_key;
        signer.key_version = version;

        let probe = crypto::sha256(b"payment-workers vault signer self-test");
        let sig = signer.sign(&probe).await?;
        crypto::verify_hash(&public_key, &probe, &sig).map_err(|_| {
            SignerError::Rejected(
                "Vault's test signature does not verify under its public key".into(),
            )
        })?;
        Ok(signer)
    }

    async fn token(&self, refresh: bool) -> Result<String, SignerError> {
        let mut guard = self.token.lock().await;
        let fresh = guard
            .as_ref()
            .is_some_and(|t| t.renew_after.is_none_or(|at| Instant::now() < at));
        if let (false, true, Some(t)) = (refresh, fresh, guard.as_ref()) {
            return Ok(t.value.clone());
        }
        let token = match &self.cfg.auth {
            VaultAuth::Token(t) => Token {
                value: t.clone(),
                renew_after: None,
            },
            VaultAuth::TokenFile(path) => Token {
                value: crate::env::read_secret("VAULT_TOKEN_FILE", &path.to_string_lossy())
                    .map_err(|e| SignerError::Rejected(e.0))?,
                renew_after: None,
            },
            VaultAuth::AppRole {
                mount,
                role_id,
                secret_id,
            } => {
                let body = json!({ "role_id": role_id, "secret_id": secret_id });
                let url = format!("{}/v1/auth/{mount}/login", self.cfg.addr);
                let resp = self.send(Method::POST, &url, None, Some(&body)).await?;
                let auth = &resp["auth"];
                let value = auth["client_token"]
                    .as_str()
                    .ok_or_else(|| SignerError::Rejected("AppRole login returned no token".into()))?
                    .to_string();
                // Log in again after two thirds of the lease, before it lapses.
                let lease = auth["lease_duration"].as_u64().unwrap_or(0);
                Token {
                    value,
                    renew_after: (lease > 0)
                        .then(|| Instant::now() + Duration::from_secs(lease * 2 / 3)),
                }
            }
        };
        let value = token.value.clone();
        *guard = Some(token);
        Ok(value)
    }

    /// One authenticated call with retries; a 403 re-authenticates once.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, SignerError> {
        let url = format!("{}/v1/{path}", self.cfg.addr);
        let token = self.token(false).await?;
        match self
            .send(method.clone(), &url, Some(&token), body.as_ref())
            .await
        {
            Err(Denied(_)) => {
                let token = self.token(true).await?;
                Ok(self.send(method, &url, Some(&token), body.as_ref()).await?)
            }
            other => Ok(other?),
        }
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, SendError> {
        let mut last = String::new();
        for attempt in 0..=self.cfg.retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200 << (attempt - 1).min(4))).await;
            }
            let mut req = self.http.request(method.clone(), url);
            if let Some(token) = token {
                req = req.header("X-Vault-Token", token);
            }
            if let Some(body) = body {
                req = req.json(body);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    last = format!("{method} {url}: {e}");
                    continue;
                }
            };
            let status = resp.status();
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            if status.is_success() {
                return Ok(body);
            }
            let msg = format!(
                "{} from Vault {method} {url}: {}",
                status.as_u16(),
                vault_errors(&body)
            );
            match status {
                StatusCode::TOO_MANY_REQUESTS => last = msg,
                s if s.is_server_error() => last = msg,
                StatusCode::FORBIDDEN => return Err(Denied(msg)),
                _ => return Err(Failed(SignerError::Rejected(msg))),
            }
        }
        Err(Failed(SignerError::Unavailable(last)))
    }
}

impl CheckpointSigner for VaultSigner {
    fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    /// Signs with the key version read at connect time, so rotating the key
    /// in Vault never silently changes which public key the chain needs.
    async fn sign(&self, hash: &Hash) -> Result<[u8; 64], SignerError> {
        let path = format!("{}/sign/{}", self.cfg.transit_mount, self.cfg.key_name);
        let body = json!({
            "input": base64::engine::general_purpose::STANDARD.encode(hash.as_bytes()),
            "key_version": self.key_version,
        });
        let resp = self.call(Method::POST, &path, Some(body)).await?;
        let raw = resp["data"]["signature"]
            .as_str()
            .ok_or_else(|| SignerError::Rejected("sign response has no signature".into()))?;
        let rest = raw
            .strip_prefix("vault:v")
            .ok_or_else(|| SignerError::Rejected(format!("unexpected signature format {raw:?}")))?;
        let (version, b64) = rest
            .split_once(':')
            .ok_or_else(|| SignerError::Rejected(format!("unexpected signature format {raw:?}")))?;
        if version.parse::<u64>().ok() != Some(self.key_version) {
            return Err(SignerError::Rejected(format!(
                "Vault signed with key version {version}, expected {}",
                self.key_version
            )));
        }
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok()
            .and_then(|s| <[u8; 64]>::try_from(s).ok())
            .ok_or_else(|| SignerError::Rejected("signature is not 64 bytes".into()))
    }
}
