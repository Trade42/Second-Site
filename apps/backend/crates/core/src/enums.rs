//! `string_enum!`: Python `StrEnum` equivalents with identical wire strings.

/// A string value that does not name any variant of the enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{value}' is not a valid {enum_name}")]
pub struct UnknownEnumValue {
    pub enum_name: &'static str,
    pub value: String,
}

/// Define a string enum: serde uses exactly the given strings, `Display`/`as_str` return them,
/// `FromStr` parses them, and ordering compares the strings (as Python compares `StrEnum` members).
///
/// Crates that use this macro must depend on `serde` with the `derive` feature.
#[macro_export]
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $value:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ::serde::Serialize, ::serde::Deserialize)]
        $vis enum $name {
            $( $(#[$vmeta])* #[serde(rename = $value)] $variant, )+
        }

        impl $name {
            /// Every member, in declaration order (Python's `list(Enum)`).
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub const fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $value,)+
                }
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::core::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::UnknownEnumValue;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                match s {
                    $($value => Ok($name::$variant),)+
                    _ => Err($crate::UnknownEnumValue {
                        enum_name: stringify!($name),
                        value: s.to_owned(),
                    }),
                }
            }
        }

        impl ::core::cmp::PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<::core::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl ::core::cmp::Ord for $name {
            fn cmp(&self, other: &Self) -> ::core::cmp::Ordering {
                self.as_str().cmp(other.as_str())
            }
        }
    };
}
