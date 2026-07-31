use std::str::FromStr;

use crate::nixhash;
use crate::nixhash::HashAlgo;
use crate::nixhash::NixHash;

/// Represents the information about the hash of a single-output FOD.
/// We store it in a [OutputHashMode] and [NixHash].
/// The serde model uses a different format, as we want to emit the same JSON:
/// There we use `hashAlgo` and `hash`:
///  - `hashAlgo`: optional `r:` prefix (for recursive),
///    followed by hash algo identifier (`sha1`, `sha256`, `sha512`, `md5`)
///  - `hash`: hexlower-encoded digest
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputHash {
    /// Hashing mode for this output. Either `Flat` or `Recursive`.
    pub mode: OutputHashMode,
    /// The expected hash for this output.
    pub hash: NixHash,
}

/// Whether the FOD describes the hash of the raw contents (only possible if it's a single file),
/// or a digest over the NAR representation of the contents.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum OutputHashMode {
    ///The output uses flat hashing mode.
    #[default]
    Flat,
    ///The output uses recursive hashing mode. This is also called NAR hashing mode.
    Recursive,
}

impl OutputHashMode {
    /// Return the prefix for this `OutputMode` as used in ATerm representation.
    pub const fn as_mode_prefix(&self) -> &'static str {
        match self {
            OutputHashMode::Flat => "",
            OutputHashMode::Recursive => "r:",
        }
    }
}

impl FromStr for OutputHashMode {
    type Err = ParseOutputHashModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" | "flat" => Ok(Self::Flat),
            "recursive" => Ok(Self::Recursive),
            _ => Err(ParseOutputHashModeError::InvalidHashMode(s.to_owned())),
        }
    }
}

impl OutputHash {
    /// Construct from a string containing the algo (with an optional `r:` prefix), and a digest.
    pub fn from_mode_algo_and_digest(
        mode_and_algo: &str,
        digest: impl AsRef<[u8]>,
    ) -> Result<Self, nixhash::Error> {
        let (hash_mode, algo_str) = if let Some(algo_str) = mode_and_algo.strip_prefix("r:") {
            (OutputHashMode::Recursive, algo_str)
        } else {
            (OutputHashMode::Flat, mode_and_algo)
        };

        let algo = algo_str.parse()?;

        Ok(OutputHash {
            mode: hash_mode,
            hash: NixHash::from_algo_and_digest(algo, digest.as_ref())?,
        })
    }

    /// Returns the OutputHashMode prefix str and the algo, concatenated.
    /// This is used in the ATerm representation.
    pub const fn as_mode_and_algo_str(&self) -> &'static str {
        match self.mode {
            OutputHashMode::Flat => self.hash.algo().as_str(),
            OutputHashMode::Recursive => match self.hash.algo() {
                HashAlgo::Md5 => "r:md5",
                HashAlgo::Sha1 => "r:sha1",
                HashAlgo::Sha256 => "r:sha256",
                HashAlgo::Sha512 => "r:sha512",
            },
        }
    }
}

/// Errors that can occur during the validation of a specific
// [crate::derivation::Output] of a [crate::derivation::Derivation].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseOutputHashModeError {
    #[error("Invalid hash mode: {0}")]
    InvalidHashMode(String),
}

#[cfg(test)]
mod tests {
    use crate::nixhash::NixHash;

    use super::{OutputHash, OutputHashMode};
    use hex_literal::hex;
    use rstest::rstest;

    const DIGEST_SHA256: [u8; 32] =
        hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);

    #[rstest]
    #[case::sha256_flat("sha256", &DIGEST_SHA256, OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone()})]
    #[case::sha256_recursive("r:sha256", &DIGEST_SHA256, OutputHash { mode: OutputHashMode::Recursive, hash: NIXHASH_SHA256.clone()})]
    fn test_from_algo_and_mode_and_digest(
        #[case] algo_and_mode: &str,
        #[case] digest: &[u8],
        #[case] expected: OutputHash,
    ) {
        assert_eq!(
            expected,
            OutputHash::from_mode_algo_and_digest(algo_and_mode, digest).expect("to parse")
        );
    }

    #[test]
    fn from_algo_and_mode_and_digest_failure() {
        assert!(OutputHash::from_mode_algo_and_digest("r:sha256", []).is_err());
        assert!(OutputHash::from_mode_algo_and_digest("ha256", DIGEST_SHA256).is_err());
    }
}
