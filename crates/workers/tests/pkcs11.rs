//! The PKCS#11 signer against SoftHSM2: an Ed25519 key generated inside the
//! token (sensitive, non-extractable) signs checkpoints through CKM_EDDSA.
//! Its own test binary: it points SOFTHSM2_CONF at a throwaway token directory.

mod common;

use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::mechanism::Mechanism;
use cryptoki::object::{Attribute, AttributeType, KeyType, ObjectClass};
use cryptoki::session::UserType;
use cryptoki::types::AuthPin;
use workers::env::Env;
use workers::signer::{AnySigner, CheckpointSigner, SignerConfig, SignerError};

const TOKEN: &str = "payment-test";
const KEY: &str = "checkpoint-signing";
/// DER OID 1.3.101.112 (id-Ed25519), the CKA_EC_PARAMS of an Ed25519 key.
const ED25519_PARAMS: [u8; 5] = [0x06, 0x03, 0x2b, 0x65, 0x70];

fn config(module: &str, key: &str, pin: &str) -> SignerConfig {
    SignerConfig::from_env(&Env::from_pairs([
        ("WORKER_SIGNER", "pkcs11"),
        ("PKCS11_MODULE", module),
        ("PKCS11_TOKEN_LABEL", TOKEN),
        ("PKCS11_KEY_LABEL", key),
        ("PKCS11_PIN", pin),
    ]))
    .unwrap()
}

#[tokio::test]
#[ignore = "requires SoftHSM2 (softhsm2-util; PKCS11_TEST_MODULE, default /usr/lib/softhsm/libsofthsm2.so)"]
async fn softhsm_ed25519_key_signs_and_never_leaves_the_token() {
    let module = std::env::var("PKCS11_TEST_MODULE")
        .unwrap_or_else(|_| "/usr/lib/softhsm/libsofthsm2.so".into());
    let installed = std::path::Path::new(&module).is_file()
        && std::process::Command::new("softhsm2-util")
            .arg("--version")
            .output()
            .is_ok();
    if !common::external("SoftHSM2 (softhsm2-util and PKCS11_TEST_MODULE)", installed) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("softhsm-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    let conf = dir.join("softhsm2.conf");
    std::fs::write(
        &conf,
        format!(
            "directories.tokendir = {}\nobjectstore.backend = file\nlog.level = ERROR\n",
            dir.join("tokens").display()
        ),
    )
    .unwrap();
    // SoftHSM reads it when the module initializes, below and in the signer.
    std::env::set_var("SOFTHSM2_CONF", &conf);
    let init = std::process::Command::new("softhsm2-util")
        .args([
            "--init-token",
            "--free",
            "--label",
            TOKEN,
            "--pin",
            "1234",
            "--so-pin",
            "5678",
        ])
        .output()
        .expect("softhsm2-util");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    // Generate the key pair inside the token, as an operator would.
    let ctx = Pkcs11::new(&module).unwrap();
    ctx.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK))
        .unwrap();
    let slot = ctx
        .get_slots_with_token()
        .unwrap()
        .into_iter()
        .find(|s| ctx.get_token_info(*s).unwrap().label().trim_end() == TOKEN)
        .unwrap();
    let session = ctx.open_rw_session(slot).unwrap();
    session
        .login(UserType::User, Some(&AuthPin::from("1234")))
        .unwrap();
    let label = Attribute::Label(KEY.as_bytes().to_vec());
    let (_, private) = session
        .generate_key_pair(
            &Mechanism::EccEdwardsKeyPairGen,
            &[
                Attribute::Token(true),
                Attribute::Verify(true),
                Attribute::EcParams(ED25519_PARAMS.to_vec()),
                label.clone(),
            ],
            &[
                Attribute::Token(true),
                Attribute::Private(true),
                Attribute::Sensitive(true),
                Attribute::Extractable(false),
                Attribute::Sign(true),
                label,
            ],
        )
        .unwrap();

    let AnySigner::Pkcs11(signer) = config(&module, KEY, "1234").connect().await.unwrap() else {
        panic!("a PKCS#11 signer")
    };
    // Many concurrent checkpoints through one session.
    let hashes: Vec<_> = (0..16u8).map(|i| crypto::sha256(&[i])).collect();
    let sigs = futures::future::join_all(hashes.iter().map(|h| signer.sign(h))).await;
    for (hash, sig) in hashes.iter().zip(sigs) {
        crypto::verify_hash(&signer.public_key(), hash, &sig.unwrap()).expect("pure Ed25519");
    }

    // The private key cannot be read out of the token.
    assert!(session
        .get_attributes(private, &[AttributeType::Value])
        .map_or(true, |a| a.is_empty()));
    let sensitive = session
        .get_attributes(
            private,
            &[AttributeType::Sensitive, AttributeType::Extractable],
        )
        .unwrap();
    assert!(
        sensitive.contains(&Attribute::Sensitive(true))
            && sensitive.contains(&Attribute::Extractable(false))
    );
    let found = session
        .find_objects(&[
            Attribute::Class(ObjectClass::PRIVATE_KEY),
            Attribute::KeyType(KeyType::EC_EDWARDS),
        ])
        .unwrap();
    assert_eq!(found.len(), 1);

    // Login state is per application: logging out here kills the signer's
    // session too, which it reopens (and logs into) on the next signature.
    session.logout().unwrap();
    let hash = crypto::sha256(b"after logout");
    crypto::verify_hash(
        &signer.public_key(),
        &hash,
        &signer.sign(&hash).await.unwrap(),
    )
    .unwrap();
    drop(signer);
    session.logout().unwrap();
    assert!(
        matches!(config(&module, KEY, "0000").connect().await, Err(SignerError::Rejected(m)) if m.contains("PIN"))
    );
    assert!(
        matches!(config(&module, "no-such-key", "1234").connect().await, Err(SignerError::Rejected(m)) if m.contains("found 0"))
    );
    std::fs::remove_dir_all(dir).unwrap();
}
