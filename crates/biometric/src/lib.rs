mod cipher;
mod error;
mod matcher;
mod policy;
mod template;
mod terminal;

pub use cipher::TemplateCipher;
pub use error::{BiometricError, MatcherError};
pub use matcher::{Hit, HttpMatcher};
pub use policy::{
    decide, identification_threshold, Candidate, Decision, MatchPolicy, DEFAULT_IDENTIFY_SCALE,
};
pub use template::{Template, TemplateFormat, MAX_TEMPLATE_BYTES, MIN_TEMPLATE_BYTES};
pub use terminal::{constant_time_eq, hash_terminal_key, TerminalKey, TERMINAL_KEY_BYTES};
