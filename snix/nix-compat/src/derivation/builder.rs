use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use bstr::BString;
use tracing::warn;

use crate::derivation::outputs::OutputsBuilder;
use crate::derivation::write::{AtermWriteable, shadow};
use crate::derivation::{Derivation, UnverifiedDerivation};
use crate::derivation::{
    DerivationError, HashDerivationModuloLookup, OutputHashMode, OutputName, Outputs,
    outputs::OutputsError,
};
use crate::nixhash::{Sha256, Sha256Digester};
use crate::store_path::{self, StorePath};

/// Builder for [`Derivation`] and [`UnverifiedDerivation`].
///
/// This helps to build either a `Derivation` or a `UnverifiedDerivation` from
/// its component fields.
///
/// Since both `Derivation` and `UnverifiedDerivation` are immutable
/// this builder is needed to construct them from scratch.
///
/// The fields are public so that they can be mutated without any getters and setters.
/// The fields don't know about output paths for a derivation, only output names
/// and whether it's a FOD.
///
/// Once the fields are populated:
///  - [`build`] returns a fully validated [`Derivation`].
///    It takes the derivation name and the HDM lookup function, calculating output paths on its own.
///  - [`build_unverified`] returns a [`UnverifiedDerivation`]
///    It takes [Outputs], which can be constructed using the constructors there.
///
/// ## Validation
///
/// When building a [`Derivation`] or an [`UnverifiedDerivation`] the following things are checked:
/// - [`StorePath`] of input derivations end in `.drv`.
/// - `system` is not empty.
/// - `builder` is not empty.
/// - no environment variable name is empty.
///
/// In addition, if producing a [Derivation], all of its invariants are checked as well.
///
/// [`StorePath`]: nix_compat::store_path::StorePath
/// [`build`]: Self::build
/// [`build_unverified`]: Self::build_unverified
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct DerivationBuilder {
    /// Command line arguments to builder
    #[cfg_attr(feature = "serde", serde(rename = "args"))]
    pub arguments: Vec<String>,

    /// Command to execute. This is usually a path to `bash`.
    ///
    /// **NOTE:** This is called `builder` in cppnix and in Nix code.
    pub command: String,

    /// Environment variables to include in build environmennt.
    #[cfg_attr(feature = "serde", serde(rename = "env"))]
    pub environment: BTreeMap<String, BString>,

    /// Map from drv path to output names used from this derivation.
    #[cfg_attr(feature = "serde", serde(rename = "inputDrvs"))]
    pub input_derivations: BTreeMap<StorePath, BTreeSet<OutputName>>,

    /// Plain store paths of additional inputs.
    #[cfg_attr(feature = "serde", serde(rename = "inputSrcs"))]
    pub input_sources: BTreeSet<StorePath>,

    /// Builder for the outputs of this derivation.
    pub outputs: OutputsBuilder,

    /// System used to build this derivation.
    pub system: String,
}

impl DerivationBuilder {
    /// Calculate HDM with provided environment variable and output replacements.
    pub(super) fn hash_derivation_modulo_with_outputs<O, L, E, EK, EV>(
        &self,
        environment: E,
        outputs: &O,
        lookup: L,
    ) -> Result<Sha256, DerivationError>
    where
        E: IntoIterator<Item = (EK, EV)>,
        EK: AsRef<[u8]>,
        EV: AsRef<[u8]>,
        O: AtermWriteable,
        L: HashDerivationModuloLookup,
    {
        // For each input_derivation, look up the hash derivation modulo,
        // and replace the derivation path with the hash_derivation_modulo.
        let mut replacements = BTreeMap::<Sha256, BTreeSet<OutputName>>::new();
        for (drv_path, output_names) in &self.input_derivations {
            let hdm = lookup
                .lookup_hdm(&drv_path.as_ref())
                .ok_or_else(|| DerivationError::MissingInputDerivation(drv_path.clone()))?;

            replacements
                .entry(hdm)
                .or_default()
                .extend(output_names.iter().cloned());
        }

        let mut hasher = Sha256Digester::new();
        let _ = self.serialize_with_replacements(
            &mut hasher,
            environment,
            outputs,
            replacements.iter(),
        );

        Ok(hasher.finalize())
    }

    fn hash_derivation_modulo<L>(&self, lookup: L) -> Result<Sha256, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        // In order to generate the correct ATerm, the environment variables that
        // correspond to each output need to be blank, but since `environment` is
        // a public field we can't gurantee this. So instead make a special
        // iterator where the blank entries hide the actual contents in
        // `environment`.
        let environment = shadow(
            self.environment
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice())),
            self.outputs.names().map(|name| (name.as_str(), &b""[..])),
        );
        self.hash_derivation_modulo_with_outputs(environment, &self.outputs, lookup)
    }

    /// Calculate and return [`Outputs`] for this `DerivationBuilder`.
    ///
    /// This will also ensure that the [`OutputsBuilder`] is valid, and that
    /// the provided `name` can be combined with the name of the output to
    /// produce a valid [`StorePath`].
    ///
    /// To calculate the outputs paths for input addressed outputs we
    /// need to lookup the [hash derivation modulo] of all input
    /// derivations, which is done using the provided
    /// [`HashDerivationModuloLookup`].
    ///
    /// Internally this calls [`store_path::build_ca_path`] or
    /// [`store_path::build_output_path`], depending on output type, to actually
    /// make the [`StorePath`].
    ///
    /// [hash derivation modulo]: nix_compat::derivation#hash-derivation-modulo
    pub fn calculate_outputs<L>(&self, name: &str, lookup: L) -> Result<Outputs, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        match &self.outputs {
            OutputsBuilder::Fixed(output_hash) => {
                // For fixed output derivation we use [build_ca_path], otherwise we
                // use [build_output_path] with [hash_derivation_modulo].
                let store_path = store_path::build_ca_path(
                    name,
                    output_hash.mode == OutputHashMode::Recursive,
                    &output_hash.hash,
                    [],
                    false,
                )
                .map_err(|e| OutputsError::InvalidOutputDerivationPath(name.to_string(), e))?;
                Ok(Outputs::fixed_output(
                    output_hash.clone(),
                    store_path.to_owned(),
                ))
            }
            OutputsBuilder::InputAddressed(output_names) if output_names.is_empty() => {
                Err(OutputsError::NoOutputs().into())
            }
            OutputsBuilder::InputAddressed(output_names) => {
                let hash_derivation_modulo = self.hash_derivation_modulo(lookup)?;
                let mut outputs = BTreeMap::new();
                for output_name in output_names.iter() {
                    // Assemble the name, which is either the drv-name suffixed `-{output_name}`,
                    // except in the `out` case, where it's omitted.
                    let name = {
                        let mut name = name.to_string();
                        if output_name != &OutputName::out() {
                            write!(name, "-{output_name}").unwrap();
                        }
                        name
                    };

                    // use [build_output_path] with [hash_derivation_modulo].
                    let store_path =
                        store_path::build_output_path(&name, &hash_derivation_modulo, output_name)
                            .map_err(|e| {
                                OutputsError::InvalidOutputDerivationPath(name.to_string(), e)
                            })?;
                    if outputs
                        .insert(output_name.clone(), store_path.to_owned())
                        .is_some()
                    {
                        return Err(OutputsError::DuplicateOutputName(output_name.clone()).into());
                    }
                }
                Outputs::input_addressed_from_iter(outputs).map_err(From::from)
            }
        }
    }

    /// Consume this builder and return the built [`Derivation`] if possible.
    ///
    /// This will build the [validated] and full [`Derivation`] using the data from
    /// this `DerivationBuilder`, the provided `name` and provided
    /// [`HashDerivationModuloLookup`].
    ///
    /// [validated]: #validation
    pub fn build<L>(self, name: &str, lookup: L) -> Result<Derivation, DerivationError>
    where
        L: HashDerivationModuloLookup,
    {
        let outputs = self.calculate_outputs(name, lookup)?;

        let inner = self.build_unverified(outputs)?;
        let drv_path = inner.calculate_derivation_path(name)?;
        Ok(Derivation { drv_path, inner })
    }

    /// Consume this builder and return a [`UnverifiedDerivation`] with the provided `outputs`.
    ///
    /// This will [validate] and update environment variables for outputs to their store paths.
    ///
    /// [validate]: #validation
    pub fn build_unverified(
        mut self,
        outputs: Outputs,
    ) -> Result<UnverifiedDerivation, DerivationError> {
        // Set environment variables corresponding to outputs to the store path of the outputs.
        for (output_name, store_path) in outputs.iter() {
            if self
                .environment
                .insert(
                    output_name.to_string(),
                    store_path.to_absolute_path().into(),
                )
                .is_some()
            {
                warn!(output.name = %output_name, "this derivation's environment shadows the output name {output_name}");
            }
        }
        self.validate()?;

        // Replace outputs builder if it doesn't match the provided outputs
        if self.outputs != outputs {
            self.outputs = outputs.clone().into_builder();
        }
        Ok(UnverifiedDerivation {
            builder: self,
            outputs,
        })
    }

    /// [Validate] the fields of this builder.
    ///
    /// [Validate]: #validation
    fn validate(&self) -> Result<(), DerivationError> {
        // Validate all input_derivation paths to end with .drv.
        // The output names are already validated as we're using the OutputName type.
        for input_derivation_path in self.input_derivations.keys() {
            if !input_derivation_path.name().ends_with(".drv") {
                return Err(DerivationError::InvalidInputDerivationPrefix(
                    input_derivation_path.to_string(),
                ));
            }
        }

        // validate platform
        if self.system.is_empty() {
            return Err(DerivationError::InvalidPlatform(self.system.to_string()));
        }

        // validate command
        if self.command.is_empty() {
            return Err(DerivationError::InvalidCommand(self.command.to_string()));
        }

        // validate env, none of the keys may be empty.
        // We skip the `name` validation seen in go-nix.
        for k in self.environment.keys() {
            if k.is_empty() {
                return Err(DerivationError::InvalidEnvironmentKey(k.to_string()));
            }
        }

        Ok(())
    }
}
