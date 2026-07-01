#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

//! Nix derivations
//!
//! This module allows you to construct, store, parse, … Nix derivations.
//!
//! There's two major types representing Derivations,
//! [UnverifiedDerivation] and [Derivation].
//!
//! They can be constructed either from parsing ATerm
//! (using [Derivation::from_aterm_bytes] or [UnverifiedDerivation::from_aterm_bytes] respectively),
//! deserializing through serde, or using [DerivationBuilder].
//!
//! Contrary to [UnverifiedDerivation], [Derivation] can only be constructed when it's certain
//! that the output path calculation is done correctly.
//! So the name and a lookup function for the [hash derivation modulos] needs to
//! be passed in whenever this type is constructed.
//!
//! [hash derivation modulos]: HashDerivationModuloLookup
//!
//!
//! # Hash Derivation Modulo
//!
//! In order to ensure that the output paths of input addressed derivations
//! are derived from the inputs of the derivation, a SHA256, calculated based
//! on the type of the input derivation, is used. This digest is called the
//! Hash Derivation Modulo (or HDM for short) and is used recursively
//! to capture the entire input closure in a kind of merkle tree.
//!
//! Generally HDM comes in three flavors: [Fixed], [Derivation Input] and [Derivation Output].
//! These are described in more detail below.
//!
//! ## Fixed
//!
//! The Fixed HDM (also called the FOD digest) is used for Fixed Output Derivations and
//! form the leafs of the derivation merkle tree.
//!
//! Unlike the flavors of HDM described later, this one is not based on the ATerm
//! representation of the derivation. It is instead calculated based on the [`OutputHash`]
//! and [`StorePath`] of the derivation output.
//!
//! The following code illustrates the format:
//!
//! ```
//! # use nix_compat::nixhash::NixHash;
//! # use nix_compat::format_sha256;
//! # let is_recursive = true;
//! # let nix_hash = NixHash::Sha1(hex_literal::hex!("0beec7b5ea3f0fdbc95d0dd47f3c5bc275da8a33"));
//! let rec = if is_recursive { "r:" } else { "" };
//! let algo = nix_hash.algo();
//! let digest = data_encoding::HEXLOWER.encode(nix_hash.digest_as_bytes());
//! // The output path of the derivation
//! let fod_output_path = "/nix/store/mp57d33657rf34lzvlbpfa1gjfv5gmpg-bar";
//! let hdm = format_sha256!("fixed:out:{rec}{algo}:{digest}:{fod_output_path}");
//! ```
//!
//! This is the hash returned by [`UnverifiedDerivation::fod_digest`].
//!
//!
//! ## Derivation Input
//!
//! The Derivation Input HDM is calculated for Input Addressed derivations and used by
//! dependent derivations in their own HDM calculations to form a merkle tree.
//!
//! This HDM is a SHA256 digest of a special version of the ATerm serialized output of the
//! derivation. In this version of the ATerm serialization, where normally the [`StorePath`]
//! of input derivations would be written, it has instead been replaced with the
//! [Derivation Input] HDM or the [Fixed] HDM (depending on what type of derivation it is).
//!
//! This is the hash used returned by [`UnverifiedDerivation::hash_derivation_modulo`].
//!
//!
//! ## Derivation Output
//!
//! When calculating the HDM we want to use in the creation of output paths we
//! don't have the output paths yet. So instead of SHA256 digesting the same ATerm format as
//! used by [Derivation Input], we additionally replace the output paths, as well as their
//! corresponding environment variables, in the ATerm serialization with empty strings.
//!
//! This is the hash used internally by [`DerivationBuilder::calculate_outputs`].
//!
//! [Fixed]: #fixed
//! [Derivation Input]: #derivation-input
//! [Derivation Output]: #derivation-output
use crate::nixhash::Sha256;
use crate::store_path::{self, StorePath, StorePathRef};
use bstr::BString;
use std::collections::{BTreeMap, BTreeSet};
use std::io;

mod errors;
mod output;
mod output_name;
pub mod outputs;
mod parse_error;
mod parser;
mod write;

mod builder;
mod hdm_lookup;
#[cfg(test)]
mod tests;

// Public API of the crate.
pub use crate::nixhash::{CAHash, NixHash};
pub use builder::DerivationBuilder;
pub use errors::DerivationError;
pub use hdm_lookup::{HashDerivationModuloLookup, lookup_fn};
pub use output::{OutputHash, OutputHashMode};
pub use output_name::{OutputName, ParseOutputNameError};
#[doc(inline)]
pub use outputs::{Outputs, OutputsBuilder};
pub use parser::Error as ParserError;

/// A verified derivation with its [`StorePath`].
///
/// A [`Derivation`] can only be created by also verifying that all its data
/// is valid and that its output paths and [`StorePath`] matches that data
/// and the [hash derivation modulo] of its dependencies.
///
/// [hash derivation modulo]: nix_compat::derivation#hash-derivation-modulo
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Derivation {
    drv_path: StorePath,
    inner: UnverifiedDerivation,
}

impl Derivation {
    /// Parse a `Derivation` in ATerm serialization, and verify that it is valid
    /// and that its output paths have been calculated correctly.
    ///
    /// Use [UnverifiedDerivation::from_aterm_bytes] to parse without verifying this.
    pub fn from_aterm_bytes<'i, L>(
        name: &str,
        lookup_fn: L,
        b: &'i [u8],
    ) -> Result<Derivation, parser::Error<&'i [u8]>>
    where
        L: HashDerivationModuloLookup,
    {
        let drv = UnverifiedDerivation::from_aterm_bytes(b)?;
        drv.verify(name, lookup_fn)
            .map_err(parser::Error::Validation)
    }

    /// Split this `Derivation` into its constituent parts.
    pub fn into_parts(self) -> (StorePath, DerivationBuilder, Outputs) {
        let (builder, outputs) = self.inner.into_parts();
        (self.drv_path, builder, outputs)
    }

    /// Return this [`Derivation`] as a [`UnverifiedDerivation`].
    pub fn as_unverified(&self) -> &UnverifiedDerivation {
        &self.inner
    }

    /// Convert this [`Derivation`] into a [`UnverifiedDerivation`].
    pub fn into_unverified(self) -> UnverifiedDerivation {
        self.inner
    }

    /// Return name of this derivation.
    ///
    /// This is the name of the store path with the suffix `.drv` stripped.
    pub fn name(&self) -> &str {
        // drv_path MUST end in .drv so this cannot panic
        self.drv_path.name().strip_suffix(".drv").unwrap()
    }

    /// Return [`StorePath`] of this derivation.
    pub fn drv_path(&self) -> &StorePath {
        &self.drv_path
    }

    /// Recalculate and return the derivation [`StorePath`].
    pub fn recalculate_derivation_path(&self) -> Result<StorePath, DerivationError> {
        self.inner.calculate_derivation_path(self.name())
    }

    /// Recalculate and return the [`Outputs`] of this `Derivation`.
    pub fn recalculate_outputs<L>(&self, lookup: L) -> Result<Outputs, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        self.inner.recalculate_outputs(self.name(), lookup)
    }
}

impl std::ops::Deref for Derivation {
    type Target = UnverifiedDerivation;

    fn deref(&self) -> &Self::Target {
        self.as_unverified()
    }
}

impl AsRef<UnverifiedDerivation> for Derivation {
    fn as_ref(&self) -> &UnverifiedDerivation {
        self.as_unverified()
    }
}

impl From<Derivation> for UnverifiedDerivation {
    fn from(value: Derivation) -> Self {
        value.into_unverified()
    }
}

/// A derivation with its output paths filled out but not verified.
///
/// This is used for working with serialized versions of the derivation
/// without having to go through the [hash derivation modulo] verification
/// required by [`Derivation`].
///
/// [`UnverifiedDerivation`] has getters for values, is not mutable and does
/// have output paths defined using [`Outputs`]. Those output paths are
/// possibly loaded from somewhere else and are not guaranteed to have been
/// verified to match the content of the derivation. When a derivation is
/// deserialized from either ATerm format or via serde this is the returned type.
///
/// [hash derivation modulo]: nix_compat::derivation#hash-derivation-modulo
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnverifiedDerivation {
    builder: DerivationBuilder,
    outputs: Outputs,
}

impl UnverifiedDerivation {
    /// Verify and consume this `UnverifiedDerivation` and return the [`Derivation`].
    ///
    /// It requires the name and lookup function to be passed in,
    /// as it verifies the outputs to have been [calculated correctly]
    /// and also [calculates the drvPath].
    ///
    /// [calculated correctly]: Self::recalculate_outputs
    /// [calculates the drvPath]: Self::calculate_derivation_path
    pub fn verify<L>(self, name: &str, lookup_fn: L) -> Result<Derivation, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        let drv_path = self.calculate_derivation_path(name)?;
        let drv = Derivation {
            drv_path,
            inner: self,
        };
        let outputs = drv.recalculate_outputs(lookup_fn)?;
        if drv.outputs != outputs {
            return Err(DerivationError::InvalidOutputs(
                outputs::OutputsError::VerificationError(),
            ));
        }
        Ok(drv)
    }

    /// write the Derivation to the given [std::io::Write], in ATerm format.
    ///
    /// The only errors returned are from the passed in writer.
    pub fn serialize(&self, writer: &mut impl std::io::Write) -> Result<(), io::Error> {
        self.builder.serialize_with_replacements(
            writer,
            &self.builder.environment,
            &self.outputs,
            self.builder.input_derivations.iter(),
        )
    }

    /// return the ATerm serialization.
    pub fn to_aterm_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.serialize(&mut buf).unwrap();
        buf
    }

    /// Parse an Derivation in ATerm serialization, and validate it passes our
    /// set of [validations].
    /// This one does not validate output store paths to be calculated correctly.
    ///
    /// [validations]: DerivationBuilder#validation
    pub fn from_aterm_bytes(b: &[u8]) -> Result<Self, parser::Error<&[u8]>> {
        parser::parse(b)
    }

    /// Calculate and return the drv path of this `UnverifiedDerivation`.
    ///
    /// The drv path is calculated by invoking [`store_path::build_text_path`], using
    /// the provided `name` with a `.drv` suffix added. All [`input_sources`] and
    /// keys of [`input_derivations`] are used as references. And the ATerm string of
    /// the `UnverifiedDerivation` is used as content.
    ///
    /// [`input_sources`]: UnverifiedDerivation::input_sources
    /// [`input_derivations`]: UnverifiedDerivation::input_derivations
    pub fn calculate_derivation_path(&self, name: &str) -> Result<StorePath, DerivationError> {
        // collect the list of paths from input_sources AND input_derivations
        // into a sorted list of references.
        let mut references: BTreeSet<StorePathRef> = self
            .builder
            .input_derivations
            .keys()
            .map(StorePath::as_ref)
            .collect();
        references.extend(self.builder.input_sources.iter().map(StorePath::as_ref));

        let drv_name = format!("{}.drv", name);
        store_path::build_text_path(
            // append .drv to the name
            &drv_name,
            self.to_aterm_bytes(),
            references,
        )
        .map_err(|err| DerivationError::InvalidDerivationName(drv_name.to_string(), err))
        .map(|sp| sp.to_owned())
    }

    /// Returns the FOD digest, if the derivation is fixed-output, or None if
    /// it's not.
    pub fn fod_digest(&self) -> Option<Sha256> {
        let (out_output_hash, out_output_path) = self.outputs.as_fixed_output()?;

        Some(store_path::fod_digest(
            out_output_hash.mode == OutputHashMode::Recursive,
            &out_output_hash.hash,
            Some(out_output_path.as_ref()),
        ))
    }

    /// Calculates the [hash derivation module] of this `UnverifiedDerivation`.
    ///
    /// [hash derivation module]: nix_compat::derivation#hash-derivation-module
    pub fn hash_derivation_modulo<L>(&self, lookup: L) -> Result<Sha256, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        // Fixed-output derivations return a fixed hash.
        if let Some(hdm) = self.fod_digest() {
            return Ok(hdm);
        }

        // Non-Fixed-output derivations return the sha256 digest of the ATerm
        // notation, but with all input_derivation paths replaced by a recursive
        // call to this function.
        // We call [hdm_lookup] rather than recursing
        // ourselves, so callers can precompute this.
        self.builder.hash_derivation_modulo_with_outputs(
            &self.builder.environment,
            &self.outputs,
            lookup,
        )
    }

    /// Recalculate and return the [`Outputs`] of this `UnverifiedDerivation`.
    pub fn recalculate_outputs<L>(&self, name: &str, lookup: L) -> Result<Outputs, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        self.builder.calculate_outputs(name, lookup)
    }

    /// Split this `UnverifiedDerivation` into its constituent parts.
    pub fn into_parts(mut self) -> (DerivationBuilder, Outputs) {
        for output_name in self.outputs.names() {
            self.builder.environment.remove(output_name.as_str());
        }
        (self.builder, self.outputs)
    }

    /// Return command line arguments to builder
    pub fn arguments(&self) -> &[String] {
        &self.builder.arguments
    }

    /// Return builder to execute.
    ///
    /// This is usually a path to `bash`.
    ///
    /// **NOTE:** This is called `builder` in cppnix and in Nix code.
    pub fn command(&self) -> &str {
        &self.builder.command
    }

    /// Return environment variables to include in build environmennt.
    pub fn environment(&self) -> &BTreeMap<String, BString> {
        &self.builder.environment
    }

    /// Map from drv path to output names used from this derivation.
    pub fn input_derivations(&self) -> &BTreeMap<StorePath, BTreeSet<OutputName>> {
        &self.builder.input_derivations
    }

    /// Return plain store paths of additional inputs.
    pub fn input_sources(&self) -> &BTreeSet<StorePath> {
        &self.builder.input_sources
    }

    /// Return the outputs of this `UnverifiedDerivation`.
    pub fn outputs(&self) -> &Outputs {
        &self.outputs
    }

    /// Return system used to build this derivation.
    pub fn system(&self) -> &str {
        &self.builder.system
    }
}

impl PartialEq<UnverifiedDerivation> for Derivation {
    fn eq(&self, other: &UnverifiedDerivation) -> bool {
        self.as_unverified() == other
    }
}

impl PartialEq<&UnverifiedDerivation> for Derivation {
    fn eq(&self, other: &&UnverifiedDerivation) -> bool {
        self.as_unverified() == *other
    }
}

impl PartialEq<&Derivation> for UnverifiedDerivation {
    fn eq(&self, other: &&Derivation) -> bool {
        other.as_unverified() == self
    }
}

impl PartialEq<Derivation> for UnverifiedDerivation {
    fn eq(&self, other: &Derivation) -> bool {
        other.as_unverified() == self
    }
}

#[cfg(feature = "serde")]
mod serde_impl {
    use std::collections::{BTreeMap, BTreeSet};

    use bstr::BString;

    use crate::derivation::{
        Derivation, DerivationBuilder, OutputName, Outputs, UnverifiedDerivation,
    };
    use crate::store_path::StorePath;

    impl serde::Serialize for Derivation {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serde::Serialize::serialize(self.as_unverified(), serializer)
        }
    }

    impl serde::Serialize for UnverifiedDerivation {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;

            let mut map = serializer.serialize_map(Some(7))?;
            map.serialize_entry("args", self.arguments())?;
            map.serialize_entry("builder", self.command())?;
            map.serialize_entry("env", self.environment())?;
            map.serialize_entry("inputDrvs", self.input_derivations())?;
            map.serialize_entry("inputSrcs", self.input_sources())?;
            map.serialize_entry("outputs", self.outputs())?;
            map.serialize_entry("system", self.system())?;
            map.end()
        }
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Helper {
        args: Vec<String>,
        builder: String,
        env: BTreeMap<String, BString>,
        input_drvs: BTreeMap<StorePath, BTreeSet<OutputName>>,
        input_srcs: BTreeSet<StorePath>,
        outputs: Outputs,
        system: String,
    }

    impl<'d> serde::Deserialize<'d> for UnverifiedDerivation {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'d>,
        {
            let h = Helper::deserialize(deserializer)?;
            let outputs = h.outputs;
            let builder = DerivationBuilder {
                arguments: h.args,
                command: h.builder,
                environment: h.env,
                input_derivations: h.input_drvs,
                input_sources: h.input_srcs,
                outputs: outputs.clone().into_builder(),
                system: h.system,
            };
            Ok(UnverifiedDerivation { builder, outputs })
        }
    }
}

#[cfg(feature = "async")]
#[allow(dead_code)]
trait DerivationAsyncExt: Sized {
    /// Parse an Derivation in ATerm serialization, and validate it passes
    /// our set of validations, from a asynchronous buffered reader.
    /// This is a streaming variant of [Derivation::from_aterm_bytes].
    async fn from_streaming_aterm_bytes<R>(reader: R) -> Result<Self, parser::Error<Vec<u8>>>
    where
        R: tokio::io::AsyncBufRead + Unpin + Send;
}

#[cfg(feature = "async")]
impl DerivationAsyncExt for UnverifiedDerivation {
    async fn from_streaming_aterm_bytes<R>(
        mut reader: R,
    ) -> Result<UnverifiedDerivation, parser::Error<Vec<u8>>>
    where
        R: tokio::io::AsyncBufRead + Unpin + Send,
    {
        use tokio::io::AsyncBufReadExt;
        let mut buffer = Vec::new();
        loop {
            let rest = reader.fill_buf().await.unwrap();
            let length = rest.len();

            // We reached EOF, we can stop and return incompleteness.
            if length == 0 {
                return Err(ParserError::Incomplete);
            }

            buffer.extend_from_slice(rest);

            // Parse the so-far internal buffer of reader.
            match parser::parse_streaming(&buffer) {
                (Err(parser::Error::Incomplete), _) => {
                    reader.consume(length);
                    continue;
                }
                (Ok(derivation), leftover) => {
                    // We cannot inline it in the next call because `reader` is mutably borrowed
                    // and has a relationship with the lifetime of `leftover`.
                    let leftover_length = leftover.len();

                    // Well, if we already had consumed the leftovers of the past fetch
                    // while believing we were just parsing incomplete ATerm, there's nothing
                    // we can do about it. The protocol is made this way.
                    if length >= leftover_length {
                        // We still have leftover, let's not consume it.
                        // It's not for us.
                        reader.consume(length - leftover_length);
                    }
                    return Ok(derivation);
                }
                (Err(e), _) => {
                    return Err(e.into());
                }
            }
        }
    }
}
