//! Validated string wrappers for provider request fields

/// Declares a string newtype that only its `parse` function can build
macro_rules! validated {
    ($(#[$meta:meta])* $name:ident, $message:literal, $valid:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps a value
            pub fn parse(value: impl Into<String>) -> $crate::error::ProviderResult<Self> {
                let value = value.into();
                let valid: fn(&str) -> bool = $valid;
                if !valid(&value) {
                    return Err($crate::error::ProviderError::validation($message));
                }

                Ok(Self(value))
            }

            /// Returns the value
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

pub(crate) use validated;
