//! An in-process RFC 3161 timestamp authority for tests: a P-256 CA, a TSA
//! certificate it issued (critical EKU id-kp-timeStamping) and real tokens —
//! TSTInfo in a CMS SignedData with content-type, message-digest and
//! SigningCertificateV2 signed attributes, exactly what a public TSA returns.
//! `Fault`s make it misbehave in one specific way at a time.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use cmpv2::status::{PkiStatus, PkiStatusInfo};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use der::asn1::{BitString, GeneralizedTime, Int, ObjectIdentifier, OctetString, SetOfVec};
use der::oid::db::{rfc5280, rfc5911, rfc5912};
use der::pem::LineEnding;
use der::{Any, Decode, Encode, EncodePem};
use p256::ecdsa::{DerSignature, SigningKey};
use p256::pkcs8::EncodePublicKey;
use rand_core::OsRng;
use sha2::Digest as _;
use spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use x509_cert::attr::Attribute;
use x509_cert::ext::pkix::{BasicConstraints, ExtendedKeyUsage};
use x509_cert::ext::Extension;
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::{Time, Validity};
use x509_cert::{Certificate, TbsCertificate, Version};
use x509_tsp::{TimeStampReq, TimeStampResp, TspVersion, TstInfo};

use super::{EssCertIdV2, SigningCertificateV2, ID_CT_TST_INFO};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fault {
    #[default]
    None,
    WrongNonce,
    /// Signed by a key other than the certificate's.
    ForeignKey,
    /// Signed by a certificate without the timestamping EKU.
    NoTimestampingEku,
    /// The TSTInfo was changed after signing.
    AlteredTstInfo,
    NoSigningCertificate,
    Rejected,
    /// Signed by a certificate that expired before genTime.
    Expired,
    /// genTime this many seconds off the real clock.
    Skewed(i64),
}

pub struct TestTsa {
    ca_key: SigningKey,
    ca_cert: Certificate,
    key: SigningKey,
    cert: Certificate,
    serial: AtomicU64,
    fault: Mutex<Fault>,
}

const CA_NAME: &str = "CN=Payment Test TSA Root,O=Test";
const TSA_NAME: &str = "CN=Payment Test TSA,O=Test";
const POLICY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.99999.2");

fn ecdsa_sha256() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: rfc5912::ECDSA_WITH_SHA_256,
        parameters: None,
    }
}

fn extension(oid: ObjectIdentifier, critical: bool, value: &impl Encode) -> Extension {
    Extension {
        extn_id: oid,
        critical,
        extn_value: OctetString::new(value.to_der().unwrap()).unwrap(),
    }
}

fn issue(
    serial: u8,
    subject: &str,
    key: &SigningKey,
    issuer: &str,
    issuer_key: &SigningKey,
    validity: (SystemTime, SystemTime),
    extensions: Vec<Extension>,
) -> Certificate {
    let spki_der = p256::PublicKey::from(key.verifying_key())
        .to_public_key_der()
        .unwrap();
    let tbs = TbsCertificate {
        version: Version::V3,
        serial_number: SerialNumber::new(&[serial]).unwrap(),
        signature: ecdsa_sha256(),
        issuer: Name::from_str(issuer).unwrap(),
        validity: Validity {
            not_before: Time::try_from(validity.0).unwrap(),
            not_after: Time::try_from(validity.1).unwrap(),
        },
        subject: Name::from_str(subject).unwrap(),
        subject_public_key_info: SubjectPublicKeyInfoOwned::from_der(spki_der.as_bytes()).unwrap(),
        issuer_unique_id: None,
        subject_unique_id: None,
        extensions: Some(extensions),
    };
    let sig: DerSignature =
        p256::ecdsa::signature::Signer::sign(issuer_key, &tbs.to_der().unwrap());
    Certificate {
        tbs_certificate: tbs,
        signature_algorithm: ecdsa_sha256(),
        signature: BitString::from_bytes(sig.as_bytes()).unwrap(),
    }
}

fn attribute(oid: ObjectIdentifier, value: &(impl der::EncodeValue + der::Tagged)) -> Attribute {
    Attribute {
        oid,
        values: SetOfVec::try_from(vec![Any::encode_from(value).unwrap()]).unwrap(),
    }
}

impl Default for TestTsa {
    fn default() -> Self {
        Self::new()
    }
}

impl TestTsa {
    pub fn new() -> Self {
        let now = SystemTime::now();
        let year = Duration::from_secs(365 * 86_400);
        let ca_key = SigningKey::random(&mut OsRng);
        let ca_cert = issue(
            1,
            CA_NAME,
            &ca_key,
            CA_NAME,
            &ca_key,
            (now - year, now + 10 * year),
            vec![extension(
                rfc5280::ID_CE_BASIC_CONSTRAINTS,
                true,
                &BasicConstraints {
                    ca: true,
                    path_len_constraint: None,
                },
            )],
        );
        let key = SigningKey::random(&mut OsRng);
        let cert = Self::tsa_cert(&ca_key, &key, (now - year, now + year), true);
        Self {
            ca_key,
            ca_cert,
            key,
            cert,
            serial: AtomicU64::new(1),
            fault: Mutex::new(Fault::None),
        }
    }

    fn tsa_cert(
        ca_key: &SigningKey,
        key: &SigningKey,
        validity: (SystemTime, SystemTime),
        eku: bool,
    ) -> Certificate {
        let mut extensions = vec![extension(
            rfc5280::ID_CE_BASIC_CONSTRAINTS,
            true,
            &BasicConstraints {
                ca: false,
                path_len_constraint: None,
            },
        )];
        if eku {
            extensions.push(extension(
                rfc5280::ID_CE_EXT_KEY_USAGE,
                true,
                &ExtendedKeyUsage(vec![rfc5280::ID_KP_TIME_STAMPING]),
            ));
        }
        issue(2, TSA_NAME, key, CA_NAME, ca_key, validity, extensions)
    }

    pub fn ca_pem(&self) -> String {
        self.ca_cert.to_pem(LineEnding::LF).unwrap()
    }

    pub fn tsa_pem(&self) -> String {
        self.cert.to_pem(LineEnding::LF).unwrap()
    }

    pub fn set_fault(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    /// The DER TimeStampResp for a DER TimeStampReq. Panics on a malformed
    /// request (a test bug, not a TSA behaviour under test).
    pub fn respond(&self, request: &[u8]) -> Vec<u8> {
        let fault = *self.fault.lock().unwrap();
        let req = TimeStampReq::from_der(request).expect("valid TimeStampReq");
        if fault == Fault::Rejected {
            return TimeStampResp {
                status: PkiStatusInfo {
                    status: PkiStatus::Rejection,
                    status_string: None,
                    fail_info: None,
                },
                time_stamp_token: None,
            }
            .to_der()
            .unwrap();
        }

        let now = SystemTime::now();
        let gen_time = match fault {
            Fault::Skewed(secs) if secs >= 0 => now + Duration::from_secs(secs as u64),
            Fault::Skewed(secs) => now - Duration::from_secs(secs.unsigned_abs()),
            _ => now,
        };
        let nonce = match fault {
            Fault::WrongNonce => Some(Int::new(&[0x55]).unwrap()),
            _ => req.nonce.clone(),
        };
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        let mut tst = TstInfo {
            version: TspVersion::V1,
            policy: POLICY,
            message_imprint: req.message_imprint.clone(),
            serial_number: Int::new(&serial.to_be_bytes()[7..]).unwrap(),
            gen_time: GeneralizedTime::from_system_time(gen_time).unwrap(),
            accuracy: None,
            ordering: false,
            nonce,
            tsa: None,
            extensions: None,
        };
        let year = Duration::from_secs(365 * 86_400);
        let cert = match fault {
            Fault::NoTimestampingEku => {
                Self::tsa_cert(&self.ca_key, &self.key, (now - year, now + year), false)
            }
            Fault::Expired => {
                Self::tsa_cert(&self.ca_key, &self.key, (now - 2 * year, now - year), true)
            }
            _ => self.cert.clone(),
        };
        let cert_der = cert.to_der().unwrap();
        let tst_der = tst.to_der().unwrap();

        let mut attrs = vec![
            attribute(rfc5911::ID_CONTENT_TYPE, &ID_CT_TST_INFO),
            attribute(
                rfc5911::ID_MESSAGE_DIGEST,
                &OctetString::new(sha2::Sha256::digest(&tst_der).to_vec()).unwrap(),
            ),
        ];
        if fault != Fault::NoSigningCertificate {
            attrs.push(attribute(
                rfc5911::ID_AA_SIGNING_CERTIFICATE_V_2,
                &SigningCertificateV2 {
                    certs: vec![EssCertIdV2 {
                        hash_algorithm: None,
                        cert_hash: OctetString::new(sha2::Sha256::digest(&cert_der).to_vec())
                            .unwrap(),
                        issuer_serial: None,
                    }],
                    policies: None,
                },
            ));
        }
        let attrs = SetOfVec::try_from(attrs).unwrap();
        let signer = if fault == Fault::ForeignKey {
            SigningKey::random(&mut OsRng)
        } else {
            self.key.clone()
        };
        let sig: DerSignature =
            p256::ecdsa::signature::Signer::sign(&signer, &attrs.to_der().unwrap());

        if fault == Fault::AlteredTstInfo {
            tst.serial_number = Int::new(&[0x7f]).unwrap();
        }
        let econtent = OctetString::new(tst.to_der().unwrap()).unwrap();
        let signed_data = SignedData {
            version: CmsVersion::V3,
            digest_algorithms: SetOfVec::try_from(vec![super::sha256_alg()]).unwrap(),
            encap_content_info: EncapsulatedContentInfo {
                econtent_type: ID_CT_TST_INFO,
                econtent: Some(Any::encode_from(&econtent).unwrap()),
            },
            certificates: req.cert_req.then(|| {
                CertificateSet(
                    SetOfVec::try_from(vec![CertificateChoices::Certificate(cert.clone())])
                        .unwrap(),
                )
            }),
            crls: None,
            signer_infos: SignerInfos(
                SetOfVec::try_from(vec![SignerInfo {
                    version: CmsVersion::V1,
                    sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
                        issuer: cert.tbs_certificate.issuer.clone(),
                        serial_number: cert.tbs_certificate.serial_number.clone(),
                    }),
                    digest_alg: super::sha256_alg(),
                    signed_attrs: Some(attrs),
                    signature_algorithm: ecdsa_sha256(),
                    signature: OctetString::new(sig.as_bytes()).unwrap(),
                    unsigned_attrs: None,
                }])
                .unwrap(),
            ),
        };
        TimeStampResp {
            status: PkiStatusInfo {
                status: PkiStatus::Accepted,
                status_string: None,
                fail_info: None,
            },
            time_stamp_token: Some(ContentInfo {
                content_type: rfc5911::ID_SIGNED_DATA,
                content: Any::encode_from(&signed_data).unwrap(),
            }),
        }
        .to_der()
        .unwrap()
    }
}
