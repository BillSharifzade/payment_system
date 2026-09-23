mod cipher;
mod error;
mod matcher;
mod policy;
mod template;

pub use cipher::TemplateCipher;
pub use error::{BiometricError, MatcherError};
pub use matcher::{Hit, HttpMatcher};
pub use policy::{decide, Candidate, Decision, MatchPolicy};
pub use template::{Template, TemplateFormat, MAX_TEMPLATE_BYTES, MIN_TEMPLATE_BYTES};
