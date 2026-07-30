//! Forward-compatible closed vocabularies.
//!
//! Every enumerated value in this contract has the same two requirements, which pull
//! in opposite directions:
//!
//! * **Exhaustiveness.** A consumer must be forced to decide what it does with every
//!   command outcome, apply lane and availability reason. A `match` that compiles is
//!   the cheapest possible proof that nothing was forgotten.
//! * **Forward compatibility.** A v1.0 consumer reading a v1.7 document must not fail
//!   because v1.3 added an outcome. Unknown members degrade; they never abort a parse.
//!
//! [`semantic_enum`] generates both: a real Rust enum with an explicit
//! `Unrecognized(String)` arm. Unknown wire values land in that arm, keep their
//! original spelling for round-tripping, and still force consumers to handle the case.

/// Generate a forward-compatible string-valued vocabulary.
///
/// Produces an enum with one variant per member plus `Unrecognized(String)`, along with
/// `as_str`, `is_recognized`, `known()`, `KNOWN_WIRE`, `Display`, `From<&str>`,
/// `FromStr`, `Serialize` and `Deserialize`.
macro_rules! semantic_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A member added by a newer minor version of the contract.
            ///
            /// The original spelling is retained so the value round-trips unchanged.
            /// Consumers must handle this arm by degrading safely — never by hiding a
            /// control's observed value or removing the user's way out of the state.
            Unrecognized(String),
        }

        impl $name {
            /// Wire spellings of every member this build recognizes.
            pub const KNOWN_WIRE: &'static [&'static str] = &[ $( $wire, )+ ];

            /// Every member this build recognizes.
            pub fn known() -> Vec<Self> {
                vec![ $( Self::$variant, )+ ]
            }

            /// The wire spelling of this member.
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Unrecognized(raw) => raw.as_str(),
                }
            }

            /// Whether this build understands this member.
            pub fn is_recognized(&self) -> bool {
                !matches!(self, Self::Unrecognized(_))
            }
        }

        impl From<&str> for $name {
            fn from(raw: &str) -> Self {
                match raw {
                    $( $wire => Self::$variant, )+
                    other => Self::Unrecognized(other.to_string()),
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;
            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Ok(Self::from(raw))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Ok(Self::from(raw.as_str()))
            }
        }
    };
}

pub(crate) use semantic_enum;
