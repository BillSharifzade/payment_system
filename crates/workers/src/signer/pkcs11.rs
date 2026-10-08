//! PKCS#11 signer: an Ed25519 key (CKK_EC_EDWARDS) generated inside an HSM
//! signs with CKM_EDDSA (PKCS#11 v3.0), so the private key never leaves the
//! token. Tested against SoftHSM2; a hardware HSM needs the same: a token
//! label, a key label shared by the private and public key objects, a PIN.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crypto::Hash;
use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::error::{Error as CkError, RvError};
use cryptoki::mechanism::eddsa::{EddsaParams, EddsaSignatureScheme};
use cryptoki::mechanism::Mechanism;
use cryptoki::object::{Attribute, AttributeType, KeyType, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::types::AuthPin;

use super::{CheckpointSigner, SignerError};

#[derive(Clone)]
pub struct Pkcs11Config {
    pub module: PathBuf,
    pub token_label: String,
    pub key_label: String,
    pub pin: String,
}

impl std::fmt::Debug for Pkcs11Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkcs11Config")
            .field("module", &self.module)
            .field("token_label", &self.token_label)
            .field("key_label", &self.key_label)
            .finish_non_exhaustive()
    }
}

struct Inner {
    ctx: Pkcs11,
    cfg: Pkcs11Config,
    session: Mutex<Option<(Session, ObjectHandle)>>,
    public_key: [u8; 32],
}

pub struct Pkcs11Signer(Arc<Inner>);

fn ck(what: &str) -> impl FnOnce(CkError) -> SignerError + '_ {
    move |e| SignerError::Unavailable(format!("PKCS#11 {what}: {e}"))
}

/// CKA_EC_POINT of an Edwards key: the DER OCTET STRING PKCS#11 v3.0
/// prescribes, or the raw 32 bytes some tokens return.
fn ec_point(raw: &[u8]) -> Option<[u8; 32]> {
    match raw {
        [0x04, 0x20, key @ ..] if key.len() == 32 => key.try_into().ok(),
        key => key.try_into().ok(),
    }
}

impl Inner {
    /// A logged-in session on the token labelled `token_label`, and the
    /// private key and public key bytes labelled `key_label`.
    fn open(&self) -> Result<(Session, ObjectHandle, [u8; 32]), SignerError> {
        let ctx = &self.ctx;
        let cfg = &self.cfg;
        let slot = ctx
            .get_slots_with_token()
            .map_err(ck("list slots"))?
            .into_iter()
            .find(|s| {
                ctx.get_token_info(*s)
                    .is_ok_and(|i| i.label().trim_end() == cfg.token_label)
            })
            .ok_or_else(|| {
                SignerError::Unavailable(format!("no PKCS#11 token labelled {:?}", cfg.token_label))
            })?;
        let session = ctx.open_ro_session(slot).map_err(ck("open session"))?;
        match session.login(UserType::User, Some(&AuthPin::from(cfg.pin.as_str()))) {
            Ok(()) | Err(CkError::Pkcs11(RvError::UserAlreadyLoggedIn, _)) => {}
            Err(CkError::Pkcs11(rv @ (RvError::PinIncorrect | RvError::PinLocked), _)) => {
                return Err(SignerError::Rejected(format!("PKCS#11 login: {rv}")))
            }
            Err(e) => return Err(ck("login")(e)),
        }
        let find = |class: ObjectClass| -> Result<ObjectHandle, SignerError> {
            let found = session
                .find_objects(&[
                    Attribute::Class(class),
                    Attribute::KeyType(KeyType::EC_EDWARDS),
                    Attribute::Label(cfg.key_label.as_bytes().to_vec()),
                ])
                .map_err(ck("find objects"))?;
            match found.as_slice() {
                [one] => Ok(*one),
                found => Err(SignerError::Rejected(format!(
                    "expected one Ed25519 {class} labelled {:?} on the token, found {}",
                    cfg.key_label,
                    found.len()
                ))),
            }
        };
        let key = find(ObjectClass::PRIVATE_KEY)?;
        let public = find(ObjectClass::PUBLIC_KEY)?;
        let public_key = session
            .get_attributes(public, &[AttributeType::EcPoint])
            .map_err(ck("read CKA_EC_POINT"))?
            .into_iter()
            .find_map(|a| match a {
                Attribute::EcPoint(p) => ec_point(&p),
                _ => None,
            })
            .ok_or_else(|| {
                SignerError::Rejected(
                    "the public key's CKA_EC_POINT is not an Ed25519 point".into(),
                )
            })?;
        Ok((session, key, public_key))
    }

    /// Signs; a failed session (HSM restarted, token re-inserted) is reopened
    /// once — and must still hold the same key.
    fn sign(&self, data: &[u8; 32]) -> Result<[u8; 64], SignerError> {
        let mechanism = Mechanism::Eddsa(EddsaParams::new(EddsaSignatureScheme::Pure));
        let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        for attempt in 0..2 {
            if guard.is_none() {
                let (session, key, public_key) = self.open()?;
                if public_key != self.public_key {
                    return Err(SignerError::Rejected(
                        "the token's key labelled PKCS11_KEY_LABEL changed".into(),
                    ));
                }
                *guard = Some((session, key));
            }
            let (session, key) = guard.as_ref().expect("opened above");
            match session.sign(&mechanism, *key, data) {
                Ok(sig) => {
                    return sig.try_into().map_err(|_| {
                        SignerError::Rejected("EdDSA signature is not 64 bytes".into())
                    })
                }
                Err(e) if attempt == 0 => {
                    tracing::warn!(error = %e, "PKCS#11 sign failed; reopening the session");
                    *guard = None;
                }
                Err(e) => return Err(ck("sign")(e)),
            }
        }
        unreachable!("the second attempt returns")
    }
}

impl Pkcs11Signer {
    /// Loads the module, logs in, finds the key and makes one test signature.
    /// Blocking (module calls are synchronous).
    pub fn connect(cfg: Pkcs11Config) -> Result<Self, SignerError> {
        let ctx = Pkcs11::new(&cfg.module).map_err(|e| {
            SignerError::Rejected(format!("PKCS11_MODULE={}: {e}", cfg.module.display()))
        })?;
        match ctx.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK)) {
            Ok(()) | Err(CkError::Pkcs11(RvError::CryptokiAlreadyInitialized, _)) => {}
            Err(e) => return Err(ck("initialize")(e)),
        }
        let mut inner = Inner {
            ctx,
            cfg,
            session: Mutex::new(None),
            public_key: [0; 32],
        };
        let (session, key, public_key) = inner.open()?;
        inner.public_key = public_key;
        inner.session = Mutex::new(Some((session, key)));

        let probe = crypto::sha256(b"payment-workers pkcs11 signer self-test");
        let sig = inner.sign(probe.as_bytes())?;
        crypto::verify_hash(&public_key, &probe, &sig).map_err(|_| {
            SignerError::Rejected(
                "the token's test signature does not verify under its public key (is CKM_EDDSA pure Ed25519?)"
                    .into(),
            )
        })?;
        Ok(Self(Arc::new(inner)))
    }
}

impl CheckpointSigner for Pkcs11Signer {
    fn public_key(&self) -> [u8; 32] {
        self.0.public_key
    }

    async fn sign(&self, hash: &Hash) -> Result<[u8; 64], SignerError> {
        let inner = self.0.clone();
        let data = *hash.as_bytes();
        tokio::task::spawn_blocking(move || inner.sign(&data))
            .await
            .map_err(|e| SignerError::Unavailable(format!("PKCS#11 sign task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::ec_point;

    #[test]
    fn ec_point_accepts_der_and_raw() {
        let key = [7u8; 32];
        assert_eq!(ec_point(&[&[0x04, 0x20][..], &key].concat()), Some(key));
        assert_eq!(ec_point(&key), Some(key));
        assert_eq!(ec_point(&[0x04, 0x20, 1, 2]), None);
        assert_eq!(ec_point(&[0u8; 33]), None);
    }
}
