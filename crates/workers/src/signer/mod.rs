//! Who signs checkpoints. `local` keeps the Ed25519 key in this process (as
//! before); `vault` asks HashiCorp Vault's transit engine and `pkcs11` an HSM,
//! so the private key never exists here. Whatever the backend, every signature
//! is checked against the backend's public key before a checkpoint is written,
//! and verification only ever uses pinned public keys (WORKER_TRUSTED_PUBLIC_KEYS).

mod pkcs11;
mod vault;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use crypto::{Hash, Sealer};

use crate::env::{http_url, ConfigError, Env};
pub use pkcs11::{Pkcs11Config, Pkcs11Signer};
pub use vault::{VaultAuth, VaultConfig, VaultSigner};

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    /// Worth retrying: the backend was unreachable, timed out or overloaded.
    #[error("{0}")]
    Unavailable(String),
    /// Retrying will not help: denied, misconfigured, wrong key type.
    #[error("{0}")]
    Rejected(String),
}

pub trait CheckpointSigner: Send + Sync {
    fn public_key(&self) -> [u8; 32];

    /// A pure Ed25519 signature over the 32 checkpoint-hash bytes.
    fn sign(&self, hash: &Hash) -> impl Future<Output = Result<[u8; 64], SignerError>> + Send;
}

impl CheckpointSigner for Sealer {
    fn public_key(&self) -> [u8; 32] {
        self.public_key_bytes()
    }

    async fn sign(&self, hash: &Hash) -> Result<[u8; 64], SignerError> {
        Ok(self.sign_hash(hash))
    }
}

pub enum AnySigner {
    Local(Sealer),
    Vault(VaultSigner),
    Pkcs11(Pkcs11Signer),
}

impl AnySigner {
    pub fn kind(&self) -> &'static str {
        match self {
            AnySigner::Local(_) => "local",
            AnySigner::Vault(_) => "vault",
            AnySigner::Pkcs11(_) => "pkcs11",
        }
    }
}

impl CheckpointSigner for AnySigner {
    fn public_key(&self) -> [u8; 32] {
        match self {
            AnySigner::Local(s) => s.public_key_bytes(),
            AnySigner::Vault(s) => s.public_key(),
            AnySigner::Pkcs11(s) => s.public_key(),
        }
    }

    async fn sign(&self, hash: &Hash) -> Result<[u8; 64], SignerError> {
        match self {
            AnySigner::Local(s) => Ok(s.sign_hash(hash)),
            AnySigner::Vault(s) => s.sign(hash).await,
            AnySigner::Pkcs11(s) => s.sign(hash).await,
        }
    }
}

/// WORKER_SIGNER and the settings of the chosen backend.
#[derive(Debug, Clone)]
pub enum SignerConfig {
    /// WORKER_SIGNING_KEY[_FILE], parsed by the binary as before.
    Local,
    Vault(VaultConfig),
    Pkcs11(Pkcs11Config),
}

impl SignerConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            SignerConfig::Local => "local",
            SignerConfig::Vault(_) => "vault",
            SignerConfig::Pkcs11(_) => "pkcs11",
        }
    }

    pub fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let kind = env.get("WORKER_SIGNER")?;
        let cfg = match kind.as_deref().unwrap_or("local") {
            "local" => return Ok(SignerConfig::Local),
            "vault" => SignerConfig::Vault(vault_config(env)?),
            "pkcs11" => SignerConfig::Pkcs11(pkcs11_config(env)?),
            other => {
                return Err(ConfigError(format!(
                    "WORKER_SIGNER={other:?}: expected local, vault or pkcs11"
                )))
            }
        };
        // The point of an external signer is that no key is here; a leftover
        // one would still be readable by anyone who reads the environment.
        for key in ["WORKER_SIGNING_KEY", "WORKER_SIGNING_KEY_FILE"] {
            if env.get(key)?.is_some() {
                return Err(ConfigError(format!(
                    "{key} is set but WORKER_SIGNER={}: remove it (keep its public key in WORKER_TRUSTED_PUBLIC_KEYS)",
                    cfg.kind()
                )));
            }
        }
        Ok(cfg)
    }

    /// Reaches an external signer and loads its public key. (A local signer is
    /// built from WORKER_SIGNING_KEY and never connects.)
    pub async fn connect(&self) -> Result<AnySigner, SignerError> {
        match self {
            SignerConfig::Local => Err(SignerError::Rejected(
                "the local signer is built from WORKER_SIGNING_KEY".into(),
            )),
            SignerConfig::Vault(cfg) => VaultSigner::connect(cfg.clone())
                .await
                .map(AnySigner::Vault),
            SignerConfig::Pkcs11(cfg) => {
                let cfg = cfg.clone();
                tokio::task::spawn_blocking(move || Pkcs11Signer::connect(cfg))
                    .await
                    .map_err(|e| SignerError::Unavailable(format!("pkcs11 connect task: {e}")))?
                    .map(AnySigner::Pkcs11)
            }
        }
    }
}

fn bounded<T: PartialOrd + std::fmt::Display + std::str::FromStr>(
    env: &Env,
    key: &str,
    default: T,
    range: std::ops::RangeInclusive<T>,
) -> Result<T, ConfigError> {
    let v = env.parse(key)?.unwrap_or(default);
    if !range.contains(&v) {
        return Err(ConfigError(format!(
            "{key}={v} is outside {}..={}",
            range.start(),
            range.end()
        )));
    }
    Ok(v)
}

/// A Vault mount or key name: path segments of [A-Za-z0-9_.-], no `..`.
fn vault_path(env: &Env, key: &str, default: &str) -> Result<String, ConfigError> {
    let v = env.get(key)?.unwrap_or_else(|| default.to_string());
    let ok = v.split('/').all(|seg| {
        !seg.is_empty()
            && seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    });
    if !ok {
        return Err(ConfigError(format!(
            "{key}={v:?} is not a valid Vault path"
        )));
    }
    Ok(v)
}

fn vault_config(env: &Env) -> Result<VaultConfig, ConfigError> {
    let addr = env
        .get("VAULT_ADDR")?
        .ok_or_else(|| ConfigError("WORKER_SIGNER=vault needs VAULT_ADDR".into()))?;
    let token_file = env.get("VAULT_TOKEN_FILE")?;
    let token = env.get("VAULT_TOKEN")?;
    let role_id = env.secret("VAULT_ROLE_ID")?;
    let secret_id = env.secret("VAULT_SECRET_ID")?;
    let auth = match (token, token_file, role_id, secret_id) {
        (Some(_), Some(_), _, _) => {
            return Err(ConfigError("set VAULT_TOKEN or VAULT_TOKEN_FILE, not both".into()))
        }
        (Some(token), None, None, None) => VaultAuth::Token(token),
        // Re-read on every (re-)authentication: Vault Agent rewrites the sink file.
        (None, Some(path), None, None) => {
            crate::env::read_secret("VAULT_TOKEN_FILE", &path)?;
            VaultAuth::TokenFile(PathBuf::from(path))
        }
        (None, None, Some(role_id), Some(secret_id)) => VaultAuth::AppRole {
            mount: vault_path(env, "VAULT_APPROLE_MOUNT", "approle")?,
            role_id,
            secret_id,
        },
        (None, None, None, None) => {
            return Err(ConfigError(
                "WORKER_SIGNER=vault needs VAULT_TOKEN_FILE (or VAULT_TOKEN), or VAULT_ROLE_ID + VAULT_SECRET_ID".into(),
            ))
        }
        _ => {
            return Err(ConfigError(
                "Vault auth: use a token OR an AppRole (VAULT_ROLE_ID and VAULT_SECRET_ID both), not a mix".into(),
            ))
        }
    };
    let ca_cert = match env.get("VAULT_CACERT")? {
        None => None,
        Some(path) => Some(
            std::fs::read(&path).map_err(|e| ConfigError(format!("VAULT_CACERT={path}: {e}")))?,
        ),
    };
    Ok(VaultConfig {
        addr: http_url("VAULT_ADDR", &addr)?,
        transit_mount: vault_path(env, "VAULT_TRANSIT_MOUNT", "transit")?,
        key_name: vault_path(env, "VAULT_TRANSIT_KEY", "checkpoint-signing")?,
        auth,
        ca_cert,
        timeout: Duration::from_millis(bounded(env, "VAULT_TIMEOUT_MS", 5_000, 100..=60_000)?),
        retries: bounded(env, "VAULT_RETRIES", 2, 0..=10)?,
    })
}

fn pkcs11_config(env: &Env) -> Result<Pkcs11Config, ConfigError> {
    let need = |key: &str| {
        env.get(key)?
            .ok_or_else(|| ConfigError(format!("WORKER_SIGNER=pkcs11 needs {key}")))
    };
    let module = PathBuf::from(need("PKCS11_MODULE")?);
    if !module.is_file() {
        return Err(ConfigError(format!(
            "PKCS11_MODULE={}: no such file",
            module.display()
        )));
    }
    Ok(Pkcs11Config {
        module,
        token_label: need("PKCS11_TOKEN_LABEL")?,
        key_label: need("PKCS11_KEY_LABEL")?,
        pin: env
            .secret("PKCS11_PIN")?
            .ok_or_else(|| ConfigError("WORKER_SIGNER=pkcs11 needs PKCS11_PIN_FILE".into()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Result<SignerConfig, ConfigError> {
        SignerConfig::from_env(&Env::from_pairs(pairs.iter().copied()))
    }

    #[test]
    fn signer_selection_is_strict() {
        assert!(matches!(parse(&[]).unwrap(), SignerConfig::Local));
        assert!(matches!(
            parse(&[("WORKER_SIGNER", "local")]).unwrap(),
            SignerConfig::Local
        ));
        assert!(parse(&[("WORKER_SIGNER", "Vault")]).is_err());
        assert!(parse(&[("WORKER_SIGNER", "hsm")]).is_err());

        let vault = [
            ("WORKER_SIGNER", "vault"),
            ("VAULT_ADDR", "http://vault:8200/"),
        ];
        let with =
            |extra: &[(&'static str, &'static str)]| parse(&[vault.as_slice(), extra].concat());
        assert!(with(&[]).is_err(), "no auth");
        let SignerConfig::Vault(cfg) = with(&[("VAULT_TOKEN", "t")]).unwrap() else {
            panic!()
        };
        assert_eq!(cfg.addr, "http://vault:8200");
        assert_eq!(
            (cfg.transit_mount.as_str(), cfg.key_name.as_str()),
            ("transit", "checkpoint-signing")
        );
        assert_eq!((cfg.timeout, cfg.retries), (Duration::from_secs(5), 2));
        assert!(matches!(
            with(&[("VAULT_ROLE_ID", "r"), ("VAULT_SECRET_ID", "s")]).unwrap(),
            SignerConfig::Vault(VaultConfig {
                auth: VaultAuth::AppRole { .. },
                ..
            })
        ));
        assert!(with(&[("VAULT_ROLE_ID", "r")]).is_err(), "half an AppRole");
        assert!(with(&[
            ("VAULT_TOKEN", "t"),
            ("VAULT_ROLE_ID", "r"),
            ("VAULT_SECRET_ID", "s")
        ])
        .is_err());
        assert!(with(&[("VAULT_TOKEN", "t"), ("VAULT_TOKEN_FILE", "/x")]).is_err());
        assert!(with(&[("VAULT_TOKEN_FILE", "/nonexistent/token")]).is_err());
        assert!(with(&[("VAULT_TOKEN", "t"), ("VAULT_TIMEOUT_MS", "0")]).is_err());
        assert!(with(&[("VAULT_TOKEN", "t"), ("VAULT_RETRIES", "-1")]).is_err());
        assert!(with(&[("VAULT_TOKEN", "t"), ("VAULT_TRANSIT_KEY", "../sys")]).is_err());
        assert!(with(&[
            ("VAULT_TOKEN", "t"),
            ("VAULT_CACERT", "/nonexistent/ca.pem")
        ])
        .is_err());
        assert!(parse(&[
            ("WORKER_SIGNER", "vault"),
            ("VAULT_ADDR", "vault:8200"),
            ("VAULT_TOKEN", "t")
        ])
        .is_err());
        let leftover = with(&[
            ("VAULT_TOKEN", "t"),
            ("WORKER_SIGNING_KEY_FILE", "/run/secrets/k"),
        ]);
        assert!(leftover.unwrap_err().0.contains("remove it"));

        let pkcs11 = [
            ("WORKER_SIGNER", "pkcs11"),
            ("PKCS11_TOKEN_LABEL", "t"),
            ("PKCS11_KEY_LABEL", "k"),
            ("PKCS11_PIN", "1234"),
        ];
        assert!(
            parse(&[pkcs11.as_slice(), &[("PKCS11_MODULE", "/nonexistent.so")]].concat()).is_err()
        );
        assert!(parse(&pkcs11).is_err(), "no module");
    }
}
