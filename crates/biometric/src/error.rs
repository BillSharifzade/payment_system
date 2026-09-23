#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BiometricError {
    #[error("unsupported template format {0:?}")]
    UnsupportedFormat(String),

    #[error("template must be between {min} and {max} bytes, got {got}")]
    TemplateSize { min: usize, max: usize, got: usize },

    #[error("template bytes do not look like a {0} record")]
    TemplateMagic(&'static str),

    #[error("template is not valid base64")]
    Base64,

    #[error("template key must be 32 bytes of hex")]
    Key,

    #[error("sealed template is malformed or was tampered with")]
    Sealed,
}

#[derive(Debug, thiserror::Error)]
pub enum MatcherError {
    #[error("matcher unavailable: {0}")]
    Unavailable(String),

    #[error("matcher protocol error: {0}")]
    Protocol(String),
}
