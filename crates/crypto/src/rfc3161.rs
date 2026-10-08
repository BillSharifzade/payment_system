//! RFC 3161 timestamp tokens: build a TimeStampReq, verify a TimeStampResp.
//!
//! A token is a CMS SignedData over a TSTInfo that names the message imprint
//! (a SHA-256 digest here) and the TSA's clock reading `genTime`. Verifying it
//! means: the imprint and nonce are ours, the signed attributes bind the
//! TSTInfo and the signer certificate, the signature verifies under that
//! certificate, the certificate is a timestamping certificate, and — when trust
//! anchors are given — it chains to one of them and every certificate on the
//! path was valid at `genTime`. Revocation (CRL/OCSP) is not checked.

use std::time::Duration;

use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier, SignerInfo};
use der::asn1::{Int, ObjectIdentifier, OctetString};
use der::oid::db::{rfc5280, rfc5911, rfc5912, rfc8410};
use der::{Any, Decode, Encode, Sequence};
use sha2::Digest as _;
use spki::{AlgorithmIdentifierOwned, DecodePublicKey, SubjectPublicKeyInfoOwned};
use x509_cert::ext::pkix::{BasicConstraints, ExtendedKeyUsage, SubjectKeyIdentifier};
use x509_cert::Certificate;
use x509_tsp::{MessageImprint, TimeStampReq, TimeStampResp, TspVersion, TstInfo};

#[cfg(any(test, feature = "test-tsa"))]
pub mod test_tsa;

/// id-ct-TSTInfo (RFC 3161 §2.4.2); not in const-oid's database.
pub const ID_CT_TST_INFO: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.1.4");
/// Responses carry a certificate chain at most; anything larger is not a TSA.
pub const MAX_RESPONSE_LEN: usize = 64 * 1024;
const MAX_CHAIN_DEPTH: usize = 6;

#[derive(Debug, thiserror::Error)]
pub enum TimestampError {
    #[error("malformed timestamp: {0}")]
    Malformed(String),
    #[error("the TSA rejected the request (PKIStatus {status}){text}")]
    Rejected { status: u8, text: String },
    #[error("the token timestamps a different message imprint")]
    ImprintMismatch,
    #[error("the token's nonce does not match the request")]
    NonceMismatch,
    #[error("unsupported algorithm {0}")]
    Unsupported(String),
    #[error("invalid signature: {0}")]
    BadSignature(&'static str),
    #[error("untrusted TSA certificate: {0}")]
    Untrusted(String),
}

fn malformed(e: impl std::fmt::Display) -> TimestampError {
    TimestampError::Malformed(e.to_string())
}

/// The certificates a token must chain to: TSA roots, intermediates or the TSA
/// certificates themselves.
#[derive(Debug, Clone)]
pub struct TsaTrust {
    anchors: Vec<(Certificate, Vec<u8>)>,
}

impl TsaTrust {
    /// One or more PEM `CERTIFICATE` blocks.
    pub fn from_pem(pem: &[u8]) -> Result<Self, TimestampError> {
        let certs = Certificate::load_pem_chain(pem).map_err(malformed)?;
        if certs.is_empty() {
            return Err(malformed("no PEM certificate found"));
        }
        let anchors = certs
            .into_iter()
            .map(|c| c.to_der().map(|der| (c, der)))
            .collect::<der::Result<_>>()
            .map_err(malformed)?;
        Ok(Self { anchors })
    }

    pub fn len(&self) -> usize {
        self.anchors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }

    fn contains(&self, der: &[u8]) -> bool {
        self.anchors.iter().any(|(_, d)| d == der)
    }
}

/// What a verified token attests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTimestamp {
    /// `genTime`, Unix seconds.
    pub gen_time: i64,
    pub serial_hex: String,
    pub policy: String,
    pub signer: String,
    /// Chained to a trust anchor (false: verified only against the token's own
    /// certificate, because no anchors were given).
    pub trusted: bool,
}

fn uint_bytes(n: u64) -> Vec<u8> {
    let be = n.to_be_bytes();
    let first = be.iter().position(|b| *b != 0).unwrap_or(7);
    let mut out = be[first..].to_vec();
    if out[0] & 0x80 != 0 {
        out.insert(0, 0);
    }
    out
}

fn sha256_alg() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: rfc5912::ID_SHA_256,
        parameters: None,
    }
}

/// DER TimeStampReq for a SHA-256 `imprint`, asking for the TSA certificate in
/// the token so the stored response verifies on its own.
pub fn timestamp_request(imprint: &[u8; 32], nonce: u64) -> Vec<u8> {
    TimeStampReq {
        version: TspVersion::V1,
        message_imprint: MessageImprint {
            hash_algorithm: sha256_alg(),
            hashed_message: OctetString::new(imprint.to_vec()).expect("32 bytes"),
        },
        req_policy: None,
        nonce: Some(Int::new(&uint_bytes(nonce)).expect("minimal positive integer")),
        cert_req: true,
        extensions: None,
    }
    .to_der()
    .expect("encodable request")
}

#[derive(Clone, Copy)]
enum Hash {
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn from_oid(oid: &ObjectIdentifier) -> Result<Self, TimestampError> {
        match *oid {
            rfc5912::ID_SHA_256 => Ok(Self::Sha256),
            rfc5912::ID_SHA_384 => Ok(Self::Sha384),
            rfc5912::ID_SHA_512 => Ok(Self::Sha512),
            _ => Err(TimestampError::Unsupported(format!("digest {oid}"))),
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

/// Verifies `signature` over `msg` under `spki`. `digest_hint` is the CMS
/// digestAlgorithm, needed when the signature algorithm is bare rsaEncryption.
fn verify_signature(
    spki: &SubjectPublicKeyInfoOwned,
    alg: &AlgorithmIdentifierOwned,
    digest_hint: Option<Hash>,
    msg: &[u8],
    signature: &[u8],
) -> Result<(), TimestampError> {
    use p256::ecdsa::signature::hazmat::PrehashVerifier;
    fn bad<E>(_: E) -> TimestampError {
        TimestampError::BadSignature("does not verify under the signer's key")
    }
    let key_der = spki.to_der().map_err(malformed)?;
    let (key_alg, hash) = match alg.oid {
        rfc5912::SHA_256_WITH_RSA_ENCRYPTION => (rfc5912::RSA_ENCRYPTION, Some(Hash::Sha256)),
        rfc5912::SHA_384_WITH_RSA_ENCRYPTION => (rfc5912::RSA_ENCRYPTION, Some(Hash::Sha384)),
        rfc5912::SHA_512_WITH_RSA_ENCRYPTION => (rfc5912::RSA_ENCRYPTION, Some(Hash::Sha512)),
        rfc5912::RSA_ENCRYPTION => (rfc5912::RSA_ENCRYPTION, digest_hint),
        rfc5912::ECDSA_WITH_SHA_256 => (rfc5912::ID_EC_PUBLIC_KEY, Some(Hash::Sha256)),
        rfc5912::ECDSA_WITH_SHA_384 => (rfc5912::ID_EC_PUBLIC_KEY, Some(Hash::Sha384)),
        rfc5912::ECDSA_WITH_SHA_512 => (rfc5912::ID_EC_PUBLIC_KEY, Some(Hash::Sha512)),
        rfc8410::ID_ED_25519 => (rfc8410::ID_ED_25519, None),
        other => return Err(TimestampError::Unsupported(format!("signature {other}"))),
    };
    if spki.algorithm.oid != key_alg {
        return Err(TimestampError::BadSignature(
            "signature algorithm does not match the key type",
        ));
    }
    match (key_alg, hash) {
        (rfc5912::RSA_ENCRYPTION, Some(hash)) => {
            let key = rsa::RsaPublicKey::from_public_key_der(&key_der).map_err(malformed)?;
            let scheme = match hash {
                Hash::Sha256 => rsa::Pkcs1v15Sign::new::<sha2::Sha256>(),
                Hash::Sha384 => rsa::Pkcs1v15Sign::new::<sha2::Sha384>(),
                Hash::Sha512 => rsa::Pkcs1v15Sign::new::<sha2::Sha512>(),
            };
            key.verify(scheme, &hash.digest(msg), signature)
                .map_err(bad)
        }
        (rfc5912::ID_EC_PUBLIC_KEY, Some(hash)) => {
            let curve: ObjectIdentifier = spki
                .algorithm
                .parameters
                .as_ref()
                .ok_or_else(|| malformed("EC key without a named curve"))?
                .decode_as()
                .map_err(malformed)?;
            let prehash = hash.digest(msg);
            match curve {
                rfc5912::SECP_256_R_1 => {
                    let key = p256::ecdsa::VerifyingKey::from_public_key_der(&key_der)
                        .map_err(malformed)?;
                    let sig = p256::ecdsa::Signature::from_der(signature).map_err(bad)?;
                    key.verify_prehash(&prehash, &sig).map_err(bad)
                }
                rfc5912::SECP_384_R_1 => {
                    let key = p384::ecdsa::VerifyingKey::from_public_key_der(&key_der)
                        .map_err(malformed)?;
                    let sig = p384::ecdsa::Signature::from_der(signature).map_err(bad)?;
                    key.verify_prehash(&prehash, &sig).map_err(bad)
                }
                other => Err(TimestampError::Unsupported(format!("curve {other}"))),
            }
        }
        (rfc8410::ID_ED_25519, None) => {
            let raw: [u8; 32] = spki
                .subject_public_key
                .as_bytes()
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| malformed("Ed25519 key is not 32 bytes"))?;
            let key = ed25519_dalek::VerifyingKey::from_bytes(&raw).map_err(malformed)?;
            let sig = ed25519_dalek::Signature::from_slice(signature).map_err(bad)?;
            ed25519_dalek::Verifier::verify(&key, msg, &sig).map_err(bad)
        }
        _ => Err(TimestampError::Unsupported(
            "rsaEncryption without a digest".into(),
        )),
    }
}

fn verify_issued_by(cert: &Certificate, issuer: &Certificate) -> Result<(), TimestampError> {
    let tbs = cert.tbs_certificate.to_der().map_err(malformed)?;
    let sig = cert
        .signature
        .as_bytes()
        .ok_or_else(|| malformed("certificate signature has unused bits"))?;
    verify_signature(
        &issuer.tbs_certificate.subject_public_key_info,
        &cert.signature_algorithm,
        None,
        &tbs,
        sig,
    )
}

fn valid_at(cert: &Certificate, at: Duration) -> bool {
    let v = &cert.tbs_certificate.validity;
    v.not_before.to_unix_duration() <= at && at <= v.not_after.to_unix_duration()
}

fn is_ca(cert: &Certificate) -> bool {
    matches!(
        cert.tbs_certificate.get::<BasicConstraints>(),
        Ok(Some((_, BasicConstraints { ca: true, .. })))
    )
}

/// Walks issuer links from `signer` to a trust anchor. Every certificate on the
/// path must be valid at `at`, every issuer a CA.
fn chain_to_anchor(
    signer: &Certificate,
    embedded: &[&Certificate],
    trust: &TsaTrust,
    at: Duration,
) -> Result<(), TimestampError> {
    let subject = |c: &Certificate| c.tbs_certificate.subject.to_string();
    let mut current = signer;
    for _ in 0..MAX_CHAIN_DEPTH {
        if !valid_at(current, at) {
            return Err(TimestampError::Untrusted(format!(
                "{} was not valid at genTime",
                subject(current)
            )));
        }
        if trust.contains(&current.to_der().map_err(malformed)?) {
            return Ok(());
        }
        let issuer = trust
            .anchors
            .iter()
            .map(|(c, _)| c)
            .chain(embedded.iter().copied())
            .filter(|c| c.tbs_certificate.subject == current.tbs_certificate.issuer)
            .filter(|c| !std::ptr::eq(*c, current))
            .find(|c| verify_issued_by(current, c).is_ok())
            .ok_or_else(|| {
                TimestampError::Untrusted(format!(
                    "no trusted issuer for {} (issuer {})",
                    subject(current),
                    current.tbs_certificate.issuer
                ))
            })?;
        if !is_ca(issuer) {
            return Err(TimestampError::Untrusted(format!(
                "issuer {} is not a CA",
                subject(issuer)
            )));
        }
        current = issuer;
    }
    Err(TimestampError::Untrusted(
        "certificate chain too long".into(),
    ))
}

/// ESSCertIDv2 / SigningCertificateV2 (RFC 5035) and their SHA-1 ancestors
/// (RFC 2634): the signed attribute binding the signature to one certificate.
#[derive(Sequence)]
struct EssCertIdV2 {
    #[asn1(optional = "true")]
    hash_algorithm: Option<AlgorithmIdentifierOwned>,
    cert_hash: OctetString,
    #[asn1(optional = "true")]
    issuer_serial: Option<Any>,
}

#[derive(Sequence)]
struct SigningCertificateV2 {
    certs: Vec<EssCertIdV2>,
    #[asn1(optional = "true")]
    policies: Option<Any>,
}

#[derive(Sequence)]
struct EssCertId {
    cert_hash: OctetString,
    #[asn1(optional = "true")]
    issuer_serial: Option<Any>,
}

#[derive(Sequence)]
struct SigningCertificate {
    certs: Vec<EssCertId>,
    #[asn1(optional = "true")]
    policies: Option<Any>,
}

fn single_attr(si: &SignerInfo, oid: ObjectIdentifier) -> Result<Option<&Any>, TimestampError> {
    let attrs = si.signed_attrs.as_ref().expect("checked by caller");
    let mut found = attrs.iter().filter(|a| a.oid == oid);
    match (found.next(), found.next()) {
        (None, _) => Ok(None),
        (Some(a), None) if a.values.len() == 1 => Ok(a.values.get(0)),
        _ => Err(malformed(format!("signed attribute {oid} is repeated"))),
    }
}

fn check_signing_certificate(si: &SignerInfo, cert_der: &[u8]) -> Result<(), TimestampError> {
    let mismatch =
        || TimestampError::BadSignature("signing-certificate attribute names another certificate");
    if let Some(v2) = single_attr(si, rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2)? {
        let attr: SigningCertificateV2 = v2.decode_as().map_err(malformed)?;
        let first = attr.certs.first().ok_or_else(mismatch)?;
        let hash = match &first.hash_algorithm {
            Some(alg) => Hash::from_oid(&alg.oid)?,
            None => Hash::Sha256,
        };
        return (first.cert_hash.as_bytes() == hash.digest(cert_der).as_slice())
            .then_some(())
            .ok_or_else(mismatch);
    }
    if let Some(v1) = single_attr(si, rfc5911::ID_AA_SIGNING_CERTIFICATE)? {
        let attr: SigningCertificate = v1.decode_as().map_err(malformed)?;
        let first = attr.certs.first().ok_or_else(mismatch)?;
        use sha1::Digest as _;
        return (first.cert_hash.as_bytes() == sha1::Sha1::digest(cert_der).as_slice())
            .then_some(())
            .ok_or_else(mismatch);
    }
    Err(TimestampError::BadSignature(
        "no signing-certificate attribute",
    ))
}

fn matches_sid(cert: &Certificate, sid: &SignerIdentifier) -> bool {
    match sid {
        SignerIdentifier::IssuerAndSerialNumber(ias) => {
            cert.tbs_certificate.issuer == ias.issuer
                && cert.tbs_certificate.serial_number == ias.serial_number
        }
        SignerIdentifier::SubjectKeyIdentifier(ski) => matches!(
            cert.tbs_certificate.get::<SubjectKeyIdentifier>(),
            Ok(Some((_, own))) if own == *ski
        ),
    }
}

/// Verifies a DER TimeStampResp for `imprint` (SHA-256). `nonce` is checked
/// when given (at issuance; a stored token is re-verified without it). With
/// `trust` the signer must chain to an anchor; without it the signature is
/// checked against the token's own certificate only and `trusted` is false.
pub fn verify_response(
    response: &[u8],
    imprint: &[u8; 32],
    nonce: Option<u64>,
    trust: Option<&TsaTrust>,
) -> Result<VerifiedTimestamp, TimestampError> {
    if response.len() > MAX_RESPONSE_LEN {
        return Err(malformed("response too large"));
    }
    let resp = TimeStampResp::from_der(response).map_err(malformed)?;
    let status = resp.status.status as u8;
    if status > 1 {
        let text = resp
            .status
            .status_string
            .iter()
            .flatten()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let text = if text.is_empty() {
            text
        } else {
            format!(": {text}")
        };
        return Err(TimestampError::Rejected { status, text });
    }
    let token = resp
        .time_stamp_token
        .ok_or_else(|| malformed("granted response carries no token"))?;
    verify_token(&token, imprint, nonce, trust)
}

fn verify_token(
    token: &ContentInfo,
    imprint: &[u8; 32],
    nonce: Option<u64>,
    trust: Option<&TsaTrust>,
) -> Result<VerifiedTimestamp, TimestampError> {
    if token.content_type != rfc5911::ID_SIGNED_DATA {
        return Err(malformed("token is not SignedData"));
    }
    let sd: SignedData = token.content.decode_as().map_err(malformed)?;
    if sd.encap_content_info.econtent_type != ID_CT_TST_INFO {
        return Err(malformed("token does not encapsulate a TSTInfo"));
    }
    let econtent: OctetString = sd
        .encap_content_info
        .econtent
        .as_ref()
        .ok_or_else(|| malformed("token has no TSTInfo"))?
        .decode_as()
        .map_err(malformed)?;
    let tst = TstInfo::from_der(econtent.as_bytes()).map_err(malformed)?;

    let mi = &tst.message_imprint;
    if mi.hash_algorithm.oid != rfc5912::ID_SHA_256 || mi.hashed_message.as_bytes() != imprint {
        return Err(TimestampError::ImprintMismatch);
    }
    if let Some(n) = nonce {
        if tst.nonce.as_ref().map(|i| i.as_bytes()) != Some(uint_bytes(n).as_slice()) {
            return Err(TimestampError::NonceMismatch);
        }
    }
    let gen_time = tst.gen_time.to_unix_duration();

    let [si] = sd.signer_infos.0.as_slice() else {
        return Err(malformed("token must have exactly one signer"));
    };
    let embedded: Vec<&Certificate> = sd
        .certificates
        .as_ref()
        .map(|set| {
            set.0
                .iter()
                .filter_map(|c| match c {
                    CertificateChoices::Certificate(c) => Some(c),
                    CertificateChoices::Other(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let signer = embedded
        .iter()
        .copied()
        .chain(trust.iter().flat_map(|t| t.anchors.iter().map(|(c, _)| c)))
        .find(|c| matches_sid(c, &si.sid))
        .ok_or_else(|| TimestampError::Untrusted("signer certificate not found".into()))?;
    let signer_der = signer.to_der().map_err(malformed)?;

    let digest_alg = Hash::from_oid(&si.digest_alg.oid)?;
    if si.signed_attrs.is_none() {
        return Err(malformed("token has no signed attributes"));
    }
    let content_type: ObjectIdentifier = single_attr(si, rfc5911::ID_CONTENT_TYPE)?
        .ok_or_else(|| malformed("no content-type attribute"))?
        .decode_as()
        .map_err(malformed)?;
    if content_type != ID_CT_TST_INFO {
        return Err(malformed("content-type attribute is not id-ct-TSTInfo"));
    }
    let message_digest: OctetString = single_attr(si, rfc5911::ID_MESSAGE_DIGEST)?
        .ok_or_else(|| malformed("no message-digest attribute"))?
        .decode_as()
        .map_err(malformed)?;
    if message_digest.as_bytes() != digest_alg.digest(econtent.as_bytes()).as_slice() {
        return Err(TimestampError::BadSignature(
            "message-digest attribute does not match the TSTInfo",
        ));
    }
    check_signing_certificate(si, &signer_der)?;
    let signed = si
        .signed_attrs
        .as_ref()
        .expect("checked above")
        .to_der()
        .map_err(malformed)?;
    verify_signature(
        &signer.tbs_certificate.subject_public_key_info,
        &si.signature_algorithm,
        Some(digest_alg),
        &signed,
        si.signature.as_bytes(),
    )?;

    let timestamping = matches!(
        signer.tbs_certificate.get::<ExtendedKeyUsage>(),
        Ok(Some((_, eku))) if eku.0.contains(&rfc5280::ID_KP_TIME_STAMPING)
    );
    if !timestamping {
        return Err(TimestampError::Untrusted(
            "signer certificate lacks extendedKeyUsage id-kp-timeStamping".into(),
        ));
    }
    if let Some(trust) = trust {
        chain_to_anchor(signer, &embedded, trust, gen_time)?;
    }

    Ok(VerifiedTimestamp {
        gen_time: gen_time.as_secs() as i64,
        serial_hex: hex::encode(tst.serial_number.as_bytes()),
        policy: tst.policy.to_string(),
        signer: signer.tbs_certificate.subject.to_string(),
        trusted: trust.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::test_tsa::{Fault, TestTsa};
    use super::*;

    fn imprint(tag: &[u8]) -> [u8; 32] {
        sha2::Sha256::digest(tag).into()
    }

    #[test]
    fn nonce_encoding_is_a_minimal_positive_integer() {
        assert_eq!(uint_bytes(0), [0]);
        assert_eq!(uint_bytes(0x7f), [0x7f]);
        assert_eq!(uint_bytes(0x80), [0, 0x80]);
        assert_eq!(
            uint_bytes(u64::MAX),
            [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn request_round_trips() {
        let req = timestamp_request(&imprint(b"x"), 42);
        let parsed = TimeStampReq::from_der(&req).unwrap();
        assert!(parsed.cert_req);
        assert_eq!(
            parsed.message_imprint.hashed_message.as_bytes(),
            imprint(b"x")
        );
        assert_eq!(parsed.nonce.unwrap().as_bytes(), [42]);
    }

    #[test]
    fn a_token_from_the_test_tsa_verifies_and_reports_gen_time() {
        let tsa = TestTsa::new();
        let trust = TsaTrust::from_pem(tsa.ca_pem().as_bytes()).unwrap();
        let im = imprint(b"checkpoint");
        let resp = tsa.respond(&timestamp_request(&im, 7));
        let v = verify_response(&resp, &im, Some(7), Some(&trust)).unwrap();
        assert!(v.trusted);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((v.gen_time - now).abs() < 5, "{v:?}");
        assert!(v.signer.contains("Test TSA"), "{v:?}");
        // A stored token re-verifies without the nonce; without anchors only
        // against its own certificate.
        assert!(
            verify_response(&resp, &im, None, Some(&trust))
                .unwrap()
                .trusted
        );
        assert!(!verify_response(&resp, &im, None, None).unwrap().trusted);
    }

    #[test]
    fn every_binding_is_checked() {
        let tsa = TestTsa::new();
        let trust = TsaTrust::from_pem(tsa.ca_pem().as_bytes()).unwrap();
        let im = imprint(b"checkpoint");
        let req = timestamp_request(&im, 7);
        let resp = tsa.respond(&req);

        assert!(matches!(
            verify_response(&resp, &imprint(b"other"), Some(7), Some(&trust)),
            Err(TimestampError::ImprintMismatch)
        ));
        assert!(matches!(
            verify_response(&resp, &im, Some(8), Some(&trust)),
            Err(TimestampError::NonceMismatch)
        ));
        let stranger = TestTsa::new();
        let other_trust = TsaTrust::from_pem(stranger.ca_pem().as_bytes()).unwrap();
        assert!(matches!(
            verify_response(&resp, &im, Some(7), Some(&other_trust)),
            Err(TimestampError::Untrusted(_))
        ));
        // The TSA's own certificate may be pinned instead of its CA.
        let pinned = TsaTrust::from_pem(tsa.tsa_pem().as_bytes()).unwrap();
        assert!(verify_response(&resp, &im, Some(7), Some(&pinned)).is_ok());

        for (fault, check) in [
            (Fault::WrongNonce, "nonce"),
            (Fault::ForeignKey, "signature"),
            (Fault::NoTimestampingEku, "timeStamping"),
            (Fault::AlteredTstInfo, "message-digest"),
            (Fault::NoSigningCertificate, "signing-certificate"),
            (Fault::Rejected, "rejected"),
            (Fault::Expired, "valid at genTime"),
        ] {
            tsa.set_fault(fault);
            let err = verify_response(&tsa.respond(&req), &im, Some(7), Some(&trust))
                .expect_err(check)
                .to_string();
            assert!(err.contains(check), "{fault:?}: {err}");
        }
        tsa.set_fault(Fault::None);
        assert!(verify_response(&tsa.respond(&req), &im, Some(7), Some(&trust)).is_ok());

        let mut flipped = resp.clone();
        let last = flipped.len() - 10;
        flipped[last] ^= 1;
        assert!(verify_response(&flipped, &im, Some(7), Some(&trust)).is_err());
        assert!(verify_response(&[0u8; 4], &im, None, None).is_err());
        assert!(TsaTrust::from_pem(b"not a certificate").is_err());
    }

    // Tokens issued by OpenSSL's TSA (`openssl ts -reply`) with an RSA-4096
    // root, an RSA-2048 TSA certificate and ESS SigningCertificate v1 — an
    // independent implementation, the algorithms public TSAs use. See
    // tests/fixtures/rfc3161/make.sh.
    #[test]
    fn verifies_openssl_issued_rsa_tokens() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rfc3161");
        let read = |f: &str| std::fs::read(format!("{dir}/{f}")).unwrap();
        let trust = TsaTrust::from_pem(&read("root.pem")).unwrap();
        let datum = read("datum.bin");
        let im: [u8; 32] = sha2::Sha256::digest(&datum).into();
        let nonce =
            u64::from_str_radix(String::from_utf8(read("nonce.txt")).unwrap().trim(), 16).unwrap();

        let v = verify_response(&read("response.tsr"), &im, Some(nonce), Some(&trust)).unwrap();
        assert!(v.trusted && v.signer.contains("Fixture TSA"), "{v:?}");
        assert_eq!(v.policy, "1.3.6.1.4.1.99999.1");
        // The certificate-less variant verifies when the TSA certificate is
        // configured, and not otherwise.
        let bare = read("response-nocert.tsr");
        assert!(matches!(
            verify_response(&bare, &im, None, Some(&trust)),
            Err(TimestampError::Untrusted(_))
        ));
        let with_tsa = TsaTrust::from_pem(&[read("root.pem"), read("tsa.pem")].concat()).unwrap();
        assert!(verify_response(&bare, &im, None, Some(&with_tsa)).is_ok());
        assert!(matches!(
            verify_response(&read("response.tsr"), &imprint(b"x"), None, Some(&trust)),
            Err(TimestampError::ImprintMismatch)
        ));
    }
}
