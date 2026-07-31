use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::derivation::{DerivationBuilder, DerivationError, Outputs, UnverifiedDerivation};
use crate::derived_path::DerivedPath;
use crate::nixbase32;
use crate::realisation::{DrvOutput, Realisation};
use crate::wire::de::Error;
use crate::{
    narinfo::Signature,
    nixhash::CAHash,
    store_path::StorePath,
    wire::{
        de::{NixDeserialize, NixRead},
        ser::{NixSerialize, NixWrite},
    },
};
use bstr::BString;
use bytes::Bytes;
use nix_compat_derive::{NixDeserialize, NixSerialize};

/// Marker type that consumes/sends and ignores a u64.
#[derive(Clone, Debug, NixDeserialize, NixSerialize)]
#[nix(from = "u64", into = "u64")]
pub struct IgnoredZero;
impl From<u64> for IgnoredZero {
    fn from(_: u64) -> Self {
        IgnoredZero
    }
}

impl From<IgnoredZero> for u64 {
    fn from(_: IgnoredZero) -> Self {
        0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    num_enum::TryFromPrimitive,
    num_enum::IntoPrimitive,
    NixDeserialize,
    NixSerialize,
)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[nix(try_from = "u16", into = "u16")]
#[repr(u16)]
pub enum BuildMode {
    Normal = 0,
    Repair = 1,
    Check = 2,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    num_enum::TryFromPrimitive,
    num_enum::IntoPrimitive,
    NixDeserialize,
    NixSerialize,
)]
#[nix(try_from = "u16", into = "u16")]
#[repr(u16)]
pub enum BuildStatus {
    Built = 0,
    Substituted = 1,
    AlreadyValid = 2,
    PermanentFailure = 3,
    InputRejected = 4,
    OutputRejected = 5,
    TransientFailure = 6,
    CachedFailure = 7,
    TimedOut = 8,
    MiscFailure = 9,
    DependencyFailed = 10,
    LogLimitExceeded = 11,
    NotDeterministic = 12,
    ResolvesToAlreadyValid = 13,
    NoSubstituters = 14,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NixDeserialize, NixSerialize)]
#[repr(transparent)]
pub struct Microseconds(i64);

impl From<i64> for Microseconds {
    fn from(value: i64) -> Self {
        Microseconds(value)
    }
}

impl From<Microseconds> for Duration {
    fn from(value: Microseconds) -> Self {
        Duration::from_micros(value.0.unsigned_abs())
    }
}

impl TryFrom<Duration> for Microseconds {
    type Error = std::num::TryFromIntError;
    fn try_from(value: Duration) -> Result<Self, Self::Error> {
        Ok(Microseconds(value.as_micros().try_into()?))
    }
}

impl From<Microseconds> for i64 {
    fn from(value: Microseconds) -> Self {
        value.0
    }
}

impl NixDeserialize for Option<Microseconds> {
    async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
    where
        R: ?Sized + NixRead + Send,
    {
        if let Some(tag) = reader.try_read_value::<u8>().await? {
            match tag {
                0 => Ok(Some(None)),
                1 => Ok(Some(Some(reader.read_value::<Microseconds>().await?))),
                _ => Err(R::Error::invalid_data("invalid optional tag from remote")),
            }
        } else {
            Ok(None)
        }
    }
}

impl NixSerialize for Option<Microseconds> {
    async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
    where
        W: NixWrite,
    {
        if let Some(value) = self.as_ref() {
            writer.write_number(1).await?;
            writer.write_value(value).await
        } else {
            writer.write_number(0).await
        }
    }
}

#[derive(Debug, NixSerialize)]
pub struct TraceLine {
    have_pos: IgnoredZero,
    hint: String,
}

/// Represents an error returned by the nix-daemon to its client.
///
/// Adheres to the format described in serialization.md
#[derive(NixSerialize)]
pub struct NixError {
    #[nix(version = "26..")]
    type_: &'static str,

    #[nix(version = "26..")]
    level: u64,

    #[nix(version = "26..")]
    name: &'static str,

    msg: String,
    #[nix(version = "26..")]
    have_pos: IgnoredZero,

    #[nix(version = "26..")]
    traces: Vec<TraceLine>,

    #[nix(version = "..=25")]
    exit_status: u64,
}

impl NixError {
    pub fn new(msg: String) -> Self {
        Self {
            type_: "Error",
            level: 0, // error
            name: "Error",
            msg,
            have_pos: IgnoredZero {},
            traces: vec![],
            exit_status: 1,
        }
    }
}

impl NixSerialize for Option<UnkeyedValidPathInfo> {
    async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
    where
        W: NixWrite,
    {
        match self {
            Some(value) => {
                writer.write_value(&true).await?;
                writer.write_value(value).await
            }
            None => writer.write_value(&false).await,
        }
    }
}

#[derive(NixSerialize, NixDeserialize, Debug, Clone, PartialEq)]
pub struct UnkeyedValidPathInfo {
    pub deriver: Option<StorePath>,
    pub nar_hash: NarHash,
    pub references: Vec<StorePath>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub signatures: Vec<Signature<String>>,
    pub ca: Option<CAHash>,
}

/// Request tuple for [super::worker_protocol::Operation::QueryValidPaths]
#[derive(NixDeserialize)]
pub struct QueryValidPaths {
    // Paths to query
    pub paths: Vec<StorePath>,

    // Whether to try and substitute the paths.
    #[nix(version = "27..")]
    pub substitute: bool,
}

/// Request tuple for [super::worker_protocol::Operation::BuildPaths]
#[derive(NixDeserialize)]
pub struct BuildPaths {
    // Paths to build
    pub paths: Vec<DerivedPath>,

    // How to build the paths
    pub mode: BuildMode,
}

/// newtype wrapper for the byte array that correctly implements NixSerialize, NixDeserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NarHash([u8; 32]);

impl NarHash {
    pub fn from_digest(digest: [u8; 32]) -> Self {
        NarHash(digest)
    }
}

impl std::ops::Deref for NarHash {
    type Target = [u8; 32];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl NixDeserialize for NarHash {
    async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
    where
        R: ?Sized + NixRead + Send,
    {
        if let Some(bytes) = reader.try_read_bytes().await? {
            let result = data_encoding::HEXLOWER
                .decode(bytes.as_ref())
                .map_err(R::Error::invalid_data)?;
            Ok(Some(NarHash(result.try_into().map_err(|_| {
                R::Error::invalid_data("incorrect length")
            })?)))
        } else {
            Ok(None)
        }
    }
}

impl NixSerialize for NarHash {
    async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
    where
        W: NixWrite,
    {
        nixbase32::encode(&self.0).serialize(writer).await
    }
}

/// Info type used by [super::worker_protocol::Operation::AddToStoreNar] and [super::worker_protocol::Operation::AddMultipleToStore]
///
/// See: [ValidPathInfo reference](https://snix.dev/docs/reference/nix-daemon-protocol/types/#validpathinfo)
#[derive(NixDeserialize, Debug)]
pub struct ValidPathInfo {
    // - path :: [StorePath][se-StorePath]
    pub path: StorePath,
    pub info: UnkeyedValidPathInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, NixDeserialize, NixSerialize)]
pub struct BuildResult {
    pub status: BuildStatus,
    pub error_msg: Bytes,
    #[nix(version = "29..")]
    pub times_built: u32,
    #[nix(version = "29..")]
    pub is_non_deterministic: bool,
    #[nix(version = "29..")]
    pub start_time: u64,
    #[nix(version = "29..")]
    pub stop_time: u64,
    #[nix(version = "37..")]
    pub cpu_user: Option<Microseconds>,
    #[nix(version = "37..")]
    pub cpu_system: Option<Microseconds>,
    #[nix(version = "28..")]
    pub built_outputs: BTreeMap<DrvOutput, Realisation>,
}

pub type KeyedBuildResults = Vec<KeyedBuildResult>;
#[derive(Debug, Clone, PartialEq, Eq, NixDeserialize, NixSerialize)]
pub struct KeyedBuildResult {
    pub path: DerivedPath,
    pub result: BuildResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, NixDeserialize, NixSerialize)]
pub struct QueryMissingResult {
    pub will_build: Vec<StorePath>,
    pub will_substitute: Vec<StorePath>,
    pub unknown: Vec<StorePath>,
    pub download_size: u64,
    pub nar_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, NixDeserialize, NixSerialize)]
pub struct BasicDerivation {
    pub outputs: Outputs,
    pub input_sources: BTreeSet<StorePath>,
    pub system: String,
    pub command: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, BString>,
}

impl TryFrom<BasicDerivation> for UnverifiedDerivation {
    type Error = DerivationError;

    fn try_from(value: BasicDerivation) -> Result<Self, Self::Error> {
        DerivationBuilder {
            input_sources: value.input_sources,
            system: value.system,
            command: value.command,
            arguments: value.arguments,
            environment: value.environment,
            ..Default::default()
        }
        .build_unverified(value.outputs)
    }
}
