use std::{fmt, str::FromStr};

use smol_str::SmolStr;

use crate::store_path;

/// A derivation output name.
///
/// This is a derivation output name, so the 'out' or 'bin' bit that has
/// been verified to not contain invalid characters.
///
/// Output names may also not be empty or be called `drv`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde_with::DeserializeFromStr, serde_with::SerializeDisplay)
)]
pub struct OutputName(SmolStr);

impl OutputName {
    /// Return this output name as a str
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Tries to construct from a &'static str.
    pub fn from_static(s: &'static str) -> Result<Self, ParseOutputNameError> {
        validate(s)?;

        Ok(Self(SmolStr::new_static(s)))
    }

    /// Return `OutputName` for default output named `"out"`.
    pub const fn out() -> Self {
        Self(SmolStr::new_static("out"))
    }
}

fn validate<S: AsRef<str>>(s: S) -> Result<(), ParseOutputNameError> {
    let name = s.as_ref();
    store_path::validate_name(name.as_bytes())?;

    // Disallow the reserved 'drv' name, which may appear in store path names,
    // but not in Derivations.
    if name == "drv" {
        return Err(ParseOutputNameError::ReservedNameDrv);
    }

    Ok(())
}

impl PartialEq<&str> for OutputName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl PartialEq<str> for OutputName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<OutputName> for &str {
    fn eq(&self, other: &OutputName) -> bool {
        *self == other.as_str()
    }
}
impl PartialEq<OutputName> for str {
    fn eq(&self, other: &OutputName) -> bool {
        self == other.as_str()
    }
}
impl PartialEq<String> for OutputName {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<OutputName> for String {
    fn eq(&self, other: &OutputName) -> bool {
        self.as_str() == other.as_str()
    }
}

impl fmt::Display for OutputName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for OutputName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for OutputName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Default for OutputName {
    fn default() -> Self {
        Self::out()
    }
}

impl FromStr for OutputName {
    type Err = ParseOutputNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        validate(s)?;

        Ok(Self(SmolStr::new(s)))
    }
}

impl TryFrom<String> for OutputName {
    type Error = ParseOutputNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate(&value)?;

        Ok(Self(SmolStr::new(value)))
    }
}

impl From<OutputName> for String {
    fn from(value: OutputName) -> Self {
        value.0.into()
    }
}

impl From<&OutputName> for String {
    fn from(value: &OutputName) -> Self {
        value.as_str().into()
    }
}

/// The error type for when parsing an [`OutputName`] fails.
#[derive(thiserror::Error, Debug, PartialEq)]
#[allow(missing_docs)]
pub enum ParseOutputNameError {
    #[error("Invalid length")]
    InvalidLength,
    #[error("Invalid name")]
    InvalidName,
    #[error("Invalid reserved name 'drv'")]
    ReservedNameDrv,
}

impl From<store_path::ParseStorePathNameError> for ParseOutputNameError {
    fn from(value: store_path::ParseStorePathNameError) -> Self {
        match value {
            store_path::ParseStorePathNameError::Length => ParseOutputNameError::InvalidLength,
            store_path::ParseStorePathNameError::Name => ParseOutputNameError::InvalidName,
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::OutputName;

    #[rstest]
    #[should_panic(expected = "InvalidName")]
    #[case("bin{n")]
    #[should_panic(expected = "InvalidName")]
    #[case("bin{n")]
    #[should_panic(expected = "InvalidName")]
    #[case(" bin{n")]
    #[should_panic(expected = "InvalidName")]
    #[case("invalid name")]
    #[should_panic(expected = "InvalidName")]
    #[case("invalid/name")]
    #[should_panic(expected = "ReservedNameDrv")]
    #[case("drv")]
    #[should_panic(expected = "InvalidLength")]
    #[case("")]
    fn parse_fail(#[case] value: &str) {
        value.parse::<OutputName>().unwrap();
    }

    #[rstest]
    #[case("out")]
    #[case("dev")]
    #[case("lib")]
    #[case("bin")]
    #[case("debug")]
    fn parse(#[case] value: &str) {
        value.parse::<OutputName>().unwrap();
    }

    #[test]
    fn size() {
        assert_eq!(size_of::<OutputName>(), size_of::<String>());
    }
}
