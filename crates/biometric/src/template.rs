use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::error::BiometricError;

pub const MAX_TEMPLATE_BYTES: usize = 16 * 1024;
pub const MIN_TEMPLATE_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateFormat {
    Iso19794_2,
    Ansi378,
    Raw,
}

impl TemplateFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            TemplateFormat::Iso19794_2 => "iso-19794-2",
            TemplateFormat::Ansi378 => "ansi-378",
            TemplateFormat::Raw => "raw",
        }
    }

    pub fn parse(s: &str) -> Result<Self, BiometricError> {
        match s {
            "iso-19794-2" => Ok(TemplateFormat::Iso19794_2),
            "ansi-378" => Ok(TemplateFormat::Ansi378),
            "raw" => Ok(TemplateFormat::Raw),
            other => Err(BiometricError::UnsupportedFormat(other.to_string())),
        }
    }

    fn validate(&self, bytes: &[u8]) -> Result<(), BiometricError> {
        match self {
            TemplateFormat::Iso19794_2 | TemplateFormat::Ansi378 => {
                if bytes.starts_with(b"FMR\0") {
                    Ok(())
                } else {
                    Err(BiometricError::TemplateMagic(self.as_str()))
                }
            }
            TemplateFormat::Raw => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    format: TemplateFormat,
    bytes: Vec<u8>,
}

impl Template {
    pub fn new(format: TemplateFormat, bytes: Vec<u8>) -> Result<Self, BiometricError> {
        if bytes.len() < MIN_TEMPLATE_BYTES || bytes.len() > MAX_TEMPLATE_BYTES {
            return Err(BiometricError::TemplateSize {
                min: MIN_TEMPLATE_BYTES,
                max: MAX_TEMPLATE_BYTES,
                got: bytes.len(),
            });
        }
        format.validate(&bytes)?;
        Ok(Self { format, bytes })
    }

    pub fn from_base64(format: &str, encoded: &str) -> Result<Self, BiometricError> {
        let format = TemplateFormat::parse(format)?;
        let bytes = BASE64
            .decode(encoded.trim())
            .map_err(|_| BiometricError::Base64)?;
        Self::new(format, bytes)
    }

    pub fn format(&self) -> TemplateFormat {
        self.format
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn to_base64(&self) -> String {
        BASE64.encode(&self.bytes)
    }

    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.format.as_str().as_bytes());
        h.update([0u8]);
        h.update(&self.bytes);
        h.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_template_round_trips_through_base64() {
        let t = Template::new(TemplateFormat::Raw, vec![7u8; 64]).unwrap();
        let again = Template::from_base64("raw", &t.to_base64()).unwrap();
        assert_eq!(t, again);
        assert_eq!(t.hash(), again.hash());
    }

    #[test]
    fn size_limits_are_enforced() {
        assert!(matches!(
            Template::new(TemplateFormat::Raw, vec![1u8; 3]),
            Err(BiometricError::TemplateSize { got: 3, .. })
        ));
        assert!(matches!(
            Template::new(TemplateFormat::Raw, vec![1u8; MAX_TEMPLATE_BYTES + 1]),
            Err(BiometricError::TemplateSize { .. })
        ));
    }

    #[test]
    fn iso_records_need_the_fmr_magic() {
        let bad = Template::new(TemplateFormat::Iso19794_2, vec![0u8; 64]);
        assert_eq!(bad, Err(BiometricError::TemplateMagic("iso-19794-2")));
        let mut good = b"FMR\0 20\0".to_vec();
        good.resize(64, 0);
        assert!(Template::new(TemplateFormat::Ansi378, good).is_ok());
    }

    #[test]
    fn hash_depends_on_format() {
        let mut bytes = b"FMR\0".to_vec();
        bytes.resize(32, 9);
        let a = Template::new(TemplateFormat::Iso19794_2, bytes.clone()).unwrap();
        let b = Template::new(TemplateFormat::Raw, bytes).unwrap();
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn unknown_format_and_bad_base64_are_rejected() {
        assert_eq!(
            Template::from_base64("png", "AAAA"),
            Err(BiometricError::UnsupportedFormat("png".into()))
        );
        assert_eq!(
            Template::from_base64("raw", "not base64!"),
            Err(BiometricError::Base64)
        );
    }
}
