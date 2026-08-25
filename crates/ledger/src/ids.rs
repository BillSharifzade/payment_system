//! Strongly-typed identifiers.
//!
//! These are newtypes around [`Uuid`] so the compiler stops you from ever
//! passing an account id where a transaction id is expected — a cheap way to
//! eliminate a whole class of mix-up bugs in the money path.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a fresh random (v4) id.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// The underlying UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }
    };
}

uuid_newtype!(
    /// Identifies an [`crate::Account`].
    AccountId
);
uuid_newtype!(
    /// Identifies a [`crate::Transaction`].
    TransactionId
);
uuid_newtype!(
    /// Identifies a single [`crate::Entry`] (posting) within a transaction.
    EntryId
);
