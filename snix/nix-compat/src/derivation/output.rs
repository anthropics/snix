use std::str::FromStr;

use crate::nixhash;
use crate::nixhash::HashAlgo;
use crate::nixhash::NixHash;
use crate::store_path::StorePath;

/// References the derivation output in its serialized form.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub(super) struct Output {
    /// Store path of build result.
    pub path: StorePath,

    #[cfg_attr(feature = "serde", serde(flatten))]
    pub output_hash: Option<OutputHash>,
}

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
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
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

#[cfg(feature = "serde")]
impl serde::Serialize for OutputHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(3))?;

        map.serialize_entry(
            "hash",
            &data_encoding::HEXLOWER.encode(self.hash.digest_as_bytes()),
        )?;

        map.serialize_entry("hashAlgo", self.as_mode_and_algo_str())?;

        map.end()
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Output {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde_json::Map;
        let fields = Map::deserialize(deserializer)?;
        let path: &str = fields
            .get("path")
            .ok_or(serde::de::Error::missing_field(
                "`path` is missing but required for outputs",
            ))?
            .as_str()
            .ok_or(serde::de::Error::invalid_type(
                serde::de::Unexpected::Other("certainly not a string"),
                &"a string",
            ))?;

        let path = StorePath::from_absolute_path(path.as_bytes()).map_err(|_| {
            serde::de::Error::invalid_value(serde::de::Unexpected::Str(path), &"StorePath")
        })?;

        Ok(Self {
            path,
            // deserialize Option<OutputHash>. we don't do this in a `impl Deserialize for OutputHash`,
            // as this is flattened and we don't want to silently swallow errors.
            output_hash: match (fields.get("hash"), fields.get("hashAlgo")) {
                // If hash is not provided, do nothing.
                (None, None) => None,
                (Some(hash_f), Some(mode_and_algo)) => {
                    let hash_str = hash_f.as_str().ok_or(serde::de::Error::invalid_type(
                        serde::de::Unexpected::Other("certainly not a string"),
                        &"a string",
                    ))?;
                    let mode_and_algo =
                        mode_and_algo
                            .as_str()
                            .ok_or(serde::de::Error::invalid_type(
                                serde::de::Unexpected::Other("certainly not a string"),
                                &"a mode:algo string",
                            ))?;

                    let digest = data_encoding::HEXLOWER
                        .decode(hash_str.as_bytes())
                        .map_err(serde::de::Error::custom)?;

                    let output_hash = OutputHash::from_mode_algo_and_digest(mode_and_algo, digest)
                        .map_err(serde::de::Error::custom)?;

                    Some(output_hash)
                }
                _ => {
                    return Err(serde::de::Error::invalid_value(
                        serde::de::Unexpected::Other("Exactly one of `hash` and `hashAlgo`"),
                        &"none or both fields",
                    ));
                }
            },
        })
    }
}

/// Errors that can occur during the validation of a specific
// [crate::derivation::Output] of a [crate::derivation::Derivation].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseOutputHashModeError {
    #[error("Invalid hash mode: {0}")]
    InvalidHashMode(String),
}

impl Output {
    pub fn is_fixed(&self) -> bool {
        self.output_hash.is_some()
    }
}

/// This ensures that a potentially valid input addressed
/// output is deserialized as a non-fixed output.
#[cfg(feature = "serde")]
#[test]
fn deserialize_valid_input_addressed_output() {
    let json_bytes = r#"
    {
      "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432"
    }"#;
    let output: Output = serde_json::from_str(json_bytes).expect("must parse");

    assert!(!output.is_fixed());
}

/// This ensures that a potentially valid fixed output
/// output deserializes fine as a fixed output.
#[cfg(feature = "serde")]
#[test]
fn deserialize_valid_fixed_output() {
    let json_bytes = r#"
    {
        "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
        "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
        "hashAlgo": "r:sha256"
    }"#;
    let output: Output = serde_json::from_str(json_bytes).expect("must parse");

    assert!(output.is_fixed());
}

/// This ensures that parsing an input with the invalid hash encoding
/// will result in a parsing failure.
#[cfg(feature = "serde")]
#[test]
fn deserialize_with_error_invalid_hash_encoding_fixed_output() {
    let json_bytes = r#"
    {
        "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
        "hash": "IAMNOTVALIDNIXBASE32",
        "hashAlgo": "r:sha256"
    }"#;
    let output: Result<Output, _> = serde_json::from_str(json_bytes);

    assert!(output.is_err());
}

/// This ensures that parsing an input with the wrong hash algo
/// will result in a parsing failure.
#[cfg(feature = "serde")]
#[test]
fn deserialize_with_error_invalid_hash_algo_fixed_output() {
    let json_bytes = r#"
    {
        "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
        "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
        "hashAlgo": "r:sha1024"
    }"#;
    let output: Result<Output, _> = serde_json::from_str(json_bytes);

    assert!(output.is_err());
}

/// This ensures that parsing an input with the missing hash algo but present hash will result in a
/// parsing failure.
#[cfg(feature = "serde")]
#[test]
fn deserialize_with_error_missing_hash_algo_fixed_output() {
    let json_bytes = r#"
    {
        "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
        "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
    }"#;
    let output: Result<Output, _> = serde_json::from_str(json_bytes);

    assert!(output.is_err());
}

/// This ensures that parsing an input with the missing hash but present hash algo will result in a
/// parsing failure.
#[cfg(feature = "serde")]
#[test]
fn deserialize_with_error_missing_hash_fixed_output() {
    let json_bytes = r#"
    {
        "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
        "hashAlgo": "r:sha1024"
    }"#;
    let output: Result<Output, _> = serde_json::from_str(json_bytes);

    assert!(output.is_err());
}

#[cfg(feature = "serde")]
#[test]
fn serialize_deserialize() {
    let json_bytes = r#"
    {
      "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432"
    }"#;
    let output: Output = serde_json::from_str(json_bytes).expect("must parse");

    let s = serde_json::to_string(&output).expect("Serialize");
    let output2: Output = serde_json::from_str(&s).expect("must parse again");

    assert_eq!(output, output2);
}

#[cfg(feature = "serde")]
#[test]
fn serialize_deserialize_fixed() {
    let json_bytes = r#"
    {
        "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
        "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
        "hashAlgo": "r:sha256"
    }"#;
    let output: Output = serde_json::from_str(json_bytes).expect("must parse");

    let s = serde_json::to_string_pretty(&output).expect("Serialize");
    let output2: Output = serde_json::from_str(&s).expect("must parse again");

    assert_eq!(output, output2);
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
