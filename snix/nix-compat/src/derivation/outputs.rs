//! Outputs for a derivation.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::{
    derivation::{OutputHash, OutputName},
    store_path::{self, StorePath},
};

/// Derivation outputs with filled out [`StorePath`].
///
/// Each output has an [`OutputName`] and a [`StorePath`] and `Outputs`
/// provides a map of [`OutputName`] to [`StorePath`]  to provide easy
/// access while still maintaining invariants.
///
/// In general there are two types of outputs: fixed or input addressed.
/// And while the fixed output also has a name and a [`StorePath`] it
/// addtionally also has a [`OutputHash`].
///
/// # Invariants
/// - Only a single FOD output is allowed and it must be named `out`
/// - Duplicately named outputs is an error
/// - Outputs can not be empty
///
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outputs(OutputsInner);

impl Outputs {
    /// Create `Outputs` with a fixed output with provided `output_hash` and `store_path`.
    ///
    /// This single fixed output is always named `out`.
    pub fn fixed_output(output_hash: OutputHash, store_path: StorePath) -> Self {
        Outputs(OutputsInner::Fixed {
            output_hash,
            store_path,
        })
    }

    /// Try to create input addressed `Outputs` from the provided iterator.
    ///
    /// This will return a [`OutputsError`] if the [`OutputName`], [`StorePath`] pairs
    /// in the iterator don't follow the [invariants].
    ///
    /// [invariants]: #invariants
    pub fn input_addressed_from_iter<I>(it: I) -> Result<Self, OutputsError>
    where
        I: IntoIterator<Item = (OutputName, StorePath)>,
    {
        Self::try_from_iter(
            it.into_iter()
                .map(|(output_name, store_path)| (output_name, store_path, None)),
        )
    }

    /// Try to make `Outputs` from the provided iterator.
    ///
    /// This will return a [`OutputsError`] if the [`OutputName`], [`StorePath`], [`OutputHash`] tuple
    /// in the iterator don't follow the [invariants].
    ///
    /// [invariants]: #invariants
    pub fn try_from_iter<I>(it: I) -> Result<Self, OutputsError>
    where
        I: IntoIterator<Item = (OutputName, StorePath, Option<OutputHash>)>,
    {
        let mut builder = UnverifiedOutputsBuilder::new();
        for (output_name, store_path, output_hash) in it {
            builder.try_insert(output_name, store_path, output_hash)?;
        }
        builder.try_build()
    }

    /// Return output hash if this `Outputs` is fixed.
    pub fn as_fixed_output_hash(&self) -> Option<&OutputHash> {
        if let OutputsInner::Fixed { output_hash, .. } = &self.0 {
            Some(output_hash)
        } else {
            None
        }
    }

    /// Return output hash and store path if this `Outputs` is fixed.
    pub fn as_fixed_output(&self) -> Option<(&OutputHash, &StorePath)> {
        if let OutputsInner::Fixed {
            output_hash,
            store_path,
        } = &self.0
        {
            Some((output_hash, store_path))
        } else {
            None
        }
    }

    /// Convert this `Outputs` into an `OutputsBuilder` with the same type and output names.
    pub fn into_builder(self) -> OutputsBuilder {
        match self.0 {
            OutputsInner::Fixed { output_hash, .. } => OutputsBuilder::Fixed(output_hash),
            OutputsInner::InputAddressed(outputs) => {
                let output_names = outputs.into_keys().collect();
                OutputsBuilder::InputAddressed(output_names)
            }
        }
    }

    /// Convert this `Outputs` into an `OutputsBuilder` with the same type and output names.
    pub fn into_unverified_builder(self) -> UnverifiedOutputsBuilder {
        UnverifiedOutputsBuilder(self.0)
    }

    /// Returns `true` if the outputs contains an output with the specified `name`.
    #[must_use]
    pub fn contains_key(&self, name: &OutputName) -> bool {
        self.0.contains_key(name)
    }

    /// Returns a reference to the [`StorePath`] corresponding to the provided `name`.
    pub fn get(&self, name: &OutputName) -> Option<&StorePath> {
        self.0.get(name)
    }

    /// Gets an iterator over the entries of the outputs, sorted by name.
    pub fn iter(&self) -> Iter<'_> {
        self.0.iter()
    }

    /// Gets an iterator over the names of the outputs, in sorted order.
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// use nix_compat::store_path::StorePath;
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// let bin_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyib-has-multi-out-bin".parse().unwrap();
    /// let a = Outputs::input_addressed_from_iter([
    ///     (OutputName::out(), out_sp),
    ///     (OutputName::from_static("bin").unwrap(), bin_sp),
    /// ]).expect("multiple outputs");
    ///
    /// let names: Vec<OutputName> = a.names().cloned().collect();
    /// assert_eq!(names, [
    ///     OutputName::from_static("bin").unwrap(),
    ///     OutputName::out(),
    /// ]);
    /// ```
    pub fn names(&self) -> OutputNames<'_> {
        self.0.names()
    }

    /// Convert this `Outputs` to an iterator over the names of the outputs, in sorted order.
    pub fn into_names(self) -> IntoOutputNames {
        self.0.into_names()
    }

    /// Gets an iterator over the [`StorePath`] values of the outputs, in order by name.
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// use nix_compat::store_path::StorePath;
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// let a = Outputs::input_addressed_from_iter([(OutputName::out(), out_sp.clone())]).unwrap();
    ///
    /// let values: Vec<StorePath> = a.store_paths().cloned().collect();
    /// assert_eq!(values, [out_sp]);
    /// ```
    pub fn store_paths(&self) -> impl Iterator<Item = &StorePath> {
        self.iter().map(|(_, path)| path)
    }

    /// Returns the number of outputs
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// use nix_compat::store_path::StorePath;
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// let a = Outputs::input_addressed_from_iter([(OutputName::out(), out_sp.clone())]).unwrap();
    /// assert_eq!(a.len(), 1);
    ///
    /// let bin_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyib-has-multi-out-bin".parse().unwrap();
    /// let b = Outputs::input_addressed_from_iter([
    ///     (OutputName::out(), out_sp),
    ///     (OutputName::from_static("bin").unwrap(), bin_sp),
    /// ]).expect("multiple outputs");
    /// assert_eq!(b.len(), 2);
    /// ```
    #[expect(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if this contains only a single output.
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    /// use nix_compat::store_path::StorePath;
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    ///
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// let a = Outputs::fixed_output(output_hash, out_sp.clone());
    /// assert!(a.is_single());
    ///
    /// let b = Outputs::input_addressed_from_iter([(OutputName::out(), out_sp.clone())]).unwrap();
    /// assert!(b.is_single());
    ///
    /// let bin_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyib-has-multi-out-bin".parse().unwrap();
    /// let c = Outputs::input_addressed_from_iter([
    ///     (OutputName::out(), out_sp),
    ///     (OutputName::from_static("bin").unwrap(), bin_sp),
    /// ]).unwrap();
    /// assert!(!c.is_single());
    /// ```
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.0.is_single()
    }

    /// Returns `true` if this is a single fixed-output.
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    /// use nix_compat::store_path::StorePath;
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// let a = Outputs::fixed_output(output_hash, out_sp.clone());
    /// assert!(a.is_fixed());
    ///
    /// let b = Outputs::input_addressed_from_iter([(OutputName::out(), out_sp.clone())]).unwrap();
    /// assert!(!b.is_fixed());
    ///
    /// let bin_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyib-has-multi-out-bin".parse().unwrap();
    /// let c = Outputs::input_addressed_from_iter([
    ///     (OutputName::out(), out_sp),
    ///     (OutputName::from_static("bin").unwrap(), bin_sp),
    /// ]).unwrap();
    /// assert!(!c.is_fixed());
    /// ```
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.0.is_fixed()
    }

    /// Returns `true` if this is a set of input addressed outputs.
    #[must_use]
    pub fn is_input_addressed(&self) -> bool {
        self.0.is_input_addressed()
    }
}

impl From<Outputs> for OutputsBuilder {
    fn from(value: Outputs) -> Self {
        value.into_builder()
    }
}

impl<'a> IntoIterator for &'a Outputs {
    type Item = (&'a OutputName, &'a StorePath);

    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for Outputs {
    type Item = (OutputName, StorePath);

    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter_internal()
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Outputs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OutputsVisitor;
        impl<'de> serde::de::Visitor<'de> for OutputsVisitor {
            type Value = Outputs;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("derivation outputs")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                use data_encoding::HEXLOWER;
                use serde::de::Error;
                #[derive(serde::Deserialize)]
                struct Output<'o> {
                    path: StorePath,
                    #[serde(rename = "hashAlgo")]
                    #[serde(default)]
                    hash_algo: &'o str,
                    #[serde(default)]
                    hash: &'o str,
                }
                fn extract<'o, E: serde::de::Error>(
                    output: Output<'o>,
                ) -> Result<(StorePath, Option<OutputHash>), E> {
                    if output.hash.is_empty() && output.hash_algo.is_empty() {
                        Ok((output.path, None))
                    } else {
                        let digest = HEXLOWER.decode(output.hash.as_bytes()).map_err(E::custom)?;
                        let output_hash =
                            OutputHash::from_mode_algo_and_digest(output.hash_algo, digest)
                                .map_err(E::custom)?;
                        Ok((output.path, Some(output_hash)))
                    }
                }

                let Some((output_name, output)) = map.next_entry::<OutputName, Output>()? else {
                    return Err(A::Error::invalid_length(0, &"non-empty derivation outputs"));
                };
                let (store_path, output_hash) = extract(output)?;
                let mut builder = UnverifiedOutputsBuilder::new();
                builder
                    .try_insert(output_name, store_path, output_hash)
                    .map_err(A::Error::custom)?;

                while let Some((output_name, output)) = map.next_entry::<OutputName, Output>()? {
                    let (store_path, output_hash) = extract(output)?;
                    builder
                        .try_insert(output_name, store_path, output_hash)
                        .map_err(A::Error::custom)?;
                }

                builder.try_build().map_err(A::Error::custom)
            }
        }

        deserializer.deserialize_map(OutputsVisitor)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Outputs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        #[derive(serde::Serialize)]
        struct OutputRef<'b> {
            path: &'b StorePath,
            #[serde(rename = "hashAlgo")]
            #[serde(skip_serializing_if = "str::is_empty")]
            hash_algo: &'b str,
            #[serde(skip_serializing_if = "String::is_empty")]
            hash: String,
        }
        let mut map = serializer.serialize_map(Some(self.len()))?;
        if let Some(output_hash) = self.as_fixed_output_hash() {
            use data_encoding::HEXLOWER;
            let digest = HEXLOWER.encode(output_hash.hash.digest_as_bytes());
            for (output_name, path) in self {
                map.serialize_entry(
                    output_name,
                    &OutputRef {
                        path,
                        hash_algo: output_hash.as_mode_and_algo_str(),
                        hash: digest.clone(),
                    },
                )?;
            }
        } else {
            for (output_name, path) in self {
                map.serialize_entry(
                    output_name,
                    &OutputRef {
                        path,
                        hash_algo: "",
                        hash: String::new(),
                    },
                )?;
            }
        }
        map.end()
    }
}

/// Builder for making unverified [`Outputs`].
///
/// Even when not verifying the output path of a set of outputs, there are still [invariants] that
/// must be upheld. But rather than checking all of them in one go, it is useful during construction
/// to check each output as they are added and then do a final check when building the `Outputs`.
///
/// # Examples
///
/// ```
/// use nix_compat::derivation::{OutputName, UnverifiedOutputsBuilder};
/// use nix_compat::store_path::StorePath;
///
/// let mut builder = UnverifiedOutputsBuilder::new();
/// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
/// builder.try_insert(OutputName::out(), out_sp, None).unwrap();
/// let outputs = builder.try_build().unwrap();
/// ```
///
/// [invariants]: Outputs#invariants
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnverifiedOutputsBuilder(OutputsInner);

impl UnverifiedOutputsBuilder {
    /// Return an empty `UnverifiedOutputsBuilder`.
    pub const fn new() -> Self {
        Self(OutputsInner::new())
    }

    /// Returns the number of outputs
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{OutputName, UnverifiedOutputsBuilder};
    /// use nix_compat::store_path::StorePath;
    ///
    /// let mut builder = UnverifiedOutputsBuilder::new();
    /// assert_eq!(builder.len(), 0);
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// builder.try_insert(OutputName::out(), out_sp, None).unwrap();
    /// assert_eq!(builder.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if there are not outputs defined in this builder.
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{OutputName, UnverifiedOutputsBuilder};
    /// use nix_compat::store_path::StorePath;
    ///
    /// let mut builder = UnverifiedOutputsBuilder::new();
    /// assert!(builder.is_empty());
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// builder.try_insert(OutputName::out(), out_sp, None).unwrap();
    /// assert!(!builder.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }

    /// Returns `true` if this is a set of input addressed outputs.
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{OutputName, UnverifiedOutputsBuilder};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    /// use nix_compat::store_path::StorePath;
    ///
    /// let mut a = UnverifiedOutputsBuilder::new();
    /// assert!(!a.is_input_addressed());
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// a.try_insert(OutputName::out(), out_sp, None).unwrap();
    /// assert!(a.is_input_addressed());
    ///
    /// let bin_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyib-has-multi-out-bin".parse().unwrap();
    /// a.try_insert(OutputName::from_static("bin").unwrap(), bin_sp, None).unwrap();
    /// assert!(a.is_input_addressed());
    ///
    /// let mut b = UnverifiedOutputsBuilder::new();
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// b.try_insert(OutputName::out(), out_sp, Some(output_hash)).unwrap();
    /// assert!(!b.is_input_addressed());
    /// ```
    #[must_use]
    pub fn is_input_addressed(&self) -> bool {
        self.0.is_input_addressed()
    }

    /// Returns `true` if this builder contains a single output called "out"
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{OutputName, UnverifiedOutputsBuilder};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    /// use nix_compat::store_path::StorePath;
    ///
    /// let mut a = UnverifiedOutputsBuilder::new();
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// a.try_insert(OutputName::out(), out_sp, None).unwrap();
    /// assert!(a.is_single());
    ///
    /// let bin_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyib-has-multi-out-bin".parse().unwrap();
    /// a.try_insert(OutputName::from_static("bin").unwrap(), bin_sp, None).unwrap();
    /// assert!(!a.is_single());
    ///
    /// let mut b = UnverifiedOutputsBuilder::new();
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// b.try_insert(OutputName::out(), out_sp, Some(output_hash)).unwrap();
    /// assert!(b.is_single());
    /// ```
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.0.is_single()
    }

    /// Returns `true` if this is a set of input addressed outputs.
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{OutputName, UnverifiedOutputsBuilder};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    /// use nix_compat::store_path::StorePath;
    ///
    /// let mut a = UnverifiedOutputsBuilder::new();
    /// assert!(!a.is_fixed());
    ///
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// a.try_insert(OutputName::out(), out_sp, None).unwrap();
    /// assert!(!a.is_fixed());
    ///
    /// let mut b = UnverifiedOutputsBuilder::new();
    /// let out_sp : StorePath = "2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out".parse().unwrap();
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// b.try_insert(OutputName::out(), out_sp, Some(output_hash)).unwrap();
    /// assert!(b.is_fixed());
    /// ```
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.0.is_fixed()
    }

    /// Returns `true` if the outputs contains an output with the specified `name`.
    #[must_use]
    pub fn contains_key(&self, name: &OutputName) -> bool {
        self.0.contains_key(name)
    }

    /// Returns a reference to the [`StorePath`] corresponding to the provided `name`.
    pub fn get(&self, name: &OutputName) -> Option<&StorePath> {
        self.0.get(name)
    }

    /// Gets an iterator over the names of the outputs, in sorted order.
    pub fn names(&self) -> OutputNames<'_> {
        self.0.names()
    }

    /// Consume this builder and return an iterator over the names of the outputs, in sorted order.
    pub fn into_names(self) -> IntoOutputNames {
        self.0.into_names()
    }

    /// Gets an iterator over the entries of the outputs, sorted by name.
    pub fn iter(&self) -> Iter<'_> {
        self.0.iter()
    }

    /// Gets an iterator over the [`StorePath`] values of the outputs, in order by name.
    pub fn store_paths(&self) -> impl Iterator<Item = &StorePath> {
        self.iter().map(|(_, path)| path)
    }

    /// Try to insert a new output with the provided values and an error
    /// if this violates the [invariants].
    ///
    /// [invariants]: #invariants
    pub fn try_insert(
        &mut self,
        output_name: OutputName,
        output_path: StorePath,
        output_hash: Option<OutputHash>,
    ) -> Result<(), OutputsError> {
        if self.is_empty() {
            if let Some(output_hash) = output_hash {
                self.0 = OutputsInner::Fixed {
                    output_hash,
                    store_path: output_path,
                };
            } else {
                self.0 =
                    OutputsInner::InputAddressed(BTreeMap::from_iter([(output_name, output_path)]));
            }
            return Ok(());
        }

        if self.is_fixed() || output_hash.is_some() {
            return Err(OutputsError::MoreThanOneOutputButFixed());
        }

        match std::mem::take(&mut self.0) {
            OutputsInner::Fixed { .. } => {
                return Err(OutputsError::MoreThanOneOutputButFixed());
            }
            OutputsInner::InputAddressed(mut outputs) => {
                if outputs.insert(output_name.clone(), output_path).is_some() {
                    return Err(OutputsError::DuplicateOutputName(output_name));
                }
                self.0 = OutputsInner::InputAddressed(outputs);
            }
        }
        Ok(())
    }

    /// Consume this builder and return the built [`Outputs`] or an error if not possible.
    pub fn try_build(self) -> Result<Outputs, OutputsError> {
        if self.is_empty() {
            return Err(OutputsError::NoOutputs());
        }

        if self.len() == 1 && !self.is_single() {
            return Err(OutputsError::InvalidOutputName(
                self.names().next().unwrap().to_string(),
            ));
        }

        Ok(Outputs(self.0))
    }
}

impl From<Outputs> for UnverifiedOutputsBuilder {
    fn from(value: Outputs) -> Self {
        value.into_unverified_builder()
    }
}

impl<'a> IntoIterator for &'a UnverifiedOutputsBuilder {
    type Item = (&'a OutputName, &'a StorePath);

    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for UnverifiedOutputsBuilder {
    type Item = (OutputName, StorePath);

    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter_internal()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OutputsInner {
    Fixed {
        output_hash: OutputHash,
        store_path: StorePath,
    },
    InputAddressed(BTreeMap<OutputName, StorePath>),
}

impl OutputsInner {
    pub const fn new() -> Self {
        OutputsInner::InputAddressed(BTreeMap::new())
    }

    pub fn len(&self) -> usize {
        match self {
            OutputsInner::Fixed { .. } => 1,
            OutputsInner::InputAddressed(outputs) => outputs.len(),
        }
    }

    pub fn is_fixed(&self) -> bool {
        matches!(self, OutputsInner::Fixed { .. })
    }

    pub fn is_input_addressed(&self) -> bool {
        matches!(self, OutputsInner::InputAddressed(outputs) if !outputs.is_empty())
    }

    pub fn is_single(&self) -> bool {
        self.len() == 1
    }

    pub fn contains_key(&self, name: &OutputName) -> bool {
        match self {
            OutputsInner::Fixed { .. } => *name == OutputName::out(),
            OutputsInner::InputAddressed(outputs) => outputs.contains_key(name),
        }
    }

    pub fn get(&self, name: &OutputName) -> Option<&StorePath> {
        match self {
            OutputsInner::Fixed { store_path, .. } if *name == OutputName::out() => {
                Some(store_path)
            }
            OutputsInner::InputAddressed(outputs) => outputs.get(name),
            _ => None,
        }
    }

    pub fn names(&self) -> OutputNames<'_> {
        match self {
            OutputsInner::Fixed { .. } => {
                const OUT: &OutputName = &OutputName::out();
                OutputNames(OutputNameInner::Single(std::iter::once(OUT)))
            }
            OutputsInner::InputAddressed(outputs) => {
                OutputNames(OutputNameInner::BTreeMap(outputs.keys()))
            }
        }
    }

    pub fn into_names(self) -> IntoOutputNames {
        match self {
            OutputsInner::Fixed { .. } => IntoOutputNames(IntoOutputNameInner::Single(
                std::iter::once(OutputName::out()),
            )),
            OutputsInner::InputAddressed(outputs) => {
                IntoOutputNames(IntoOutputNameInner::BTreeMap(outputs.into_keys()))
            }
        }
    }

    /// Gets an iterator over the entries of the outputs, sorted by name.
    pub fn iter(&self) -> Iter<'_> {
        match self {
            OutputsInner::Fixed { store_path, .. } => {
                const OUT: &OutputName = &OutputName::out();
                Iter(IterI::Fixed(std::iter::once((OUT, store_path))))
            }
            OutputsInner::InputAddressed(outputs) => Iter(IterI::InputAddressed(outputs.iter())),
        }
    }

    fn into_iter_internal(self) -> IntoIter {
        match self {
            OutputsInner::Fixed { store_path, .. } => IntoIter(IntoIterI::Fixed(std::iter::once(
                (OutputName::out(), store_path),
            ))),
            OutputsInner::InputAddressed(outputs) => {
                IntoIter(IntoIterI::InputAddressed(outputs.into_iter()))
            }
        }
    }
}

impl Default for OutputsInner {
    fn default() -> Self {
        Self::new()
    }
}

/// A builder for outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize), serde(from = "Outputs"))]
pub enum OutputsBuilder {
    /// A fixed ouput
    Fixed(OutputHash),
    /// Input addressed outputs
    InputAddressed(BTreeSet<OutputName>),
}

impl OutputsBuilder {
    /// Gets an iterator over the names of the outputs, in sorted order.
    pub fn names(&self) -> OutputNames<'_> {
        const OUT: &OutputName = &OutputName::out();
        match self {
            OutputsBuilder::Fixed(_) => OutputNames(OutputNameInner::Single(std::iter::once(OUT))),
            OutputsBuilder::InputAddressed(outputs) => {
                OutputNames(OutputNameInner::BTreeSet(outputs.iter()))
            }
        }
    }

    /// Convert this `OutputsBuilder` to an iterator over the names of the outputs, in sorted order.
    pub fn into_names(self) -> IntoOutputNames {
        match self {
            OutputsBuilder::Fixed(_) => IntoOutputNames(IntoOutputNameInner::Single(
                std::iter::once(OutputName::out()),
            )),
            OutputsBuilder::InputAddressed(outputs) => {
                IntoOutputNames(IntoOutputNameInner::BTreeSet(outputs.into_iter()))
            }
        }
    }

    /// Return count of names in this `OutputsBuilder`.
    pub fn len(&self) -> usize {
        match self {
            OutputsBuilder::Fixed(_) => 1,
            OutputsBuilder::InputAddressed(outputs) => outputs.len(),
        }
    }

    /// Returns `true` if the builder contains no outputs.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if the builder contains a single output.
    pub fn is_single(&self) -> bool {
        self.len() == 1
    }

    /// Returns `true` if the builder contains a fixed output.
    pub fn is_fixed(&self) -> bool {
        matches!(self, Self::Fixed(_))
    }

    /// Returns `true` if the builder contains an output with the provided `name`.
    pub fn contains(&self, name: &OutputName) -> bool {
        match self {
            OutputsBuilder::Fixed(_) => *name == OutputName::out(),
            OutputsBuilder::InputAddressed(output_names) => output_names.contains(name),
        }
    }
}

impl Default for OutputsBuilder {
    fn default() -> Self {
        Self::InputAddressed(BTreeSet::from_iter([OutputName::out()]))
    }
}

impl PartialEq<Outputs> for OutputsBuilder {
    fn eq(&self, other: &Outputs) -> bool {
        self.is_fixed() == other.is_fixed() && self.names().eq(other.names())
    }
}

impl PartialEq<OutputsBuilder> for Outputs {
    fn eq(&self, other: &OutputsBuilder) -> bool {
        self.is_fixed() == other.is_fixed() && self.names().eq(other.names())
    }
}

impl<'a> IntoIterator for &'a OutputsBuilder {
    type Item = &'a OutputName;

    type IntoIter = OutputNames<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.names()
    }
}

impl IntoIterator for OutputsBuilder {
    type Item = OutputName;

    type IntoIter = IntoOutputNames;

    fn into_iter(self) -> Self::IntoIter {
        self.into_names()
    }
}

impl FromIterator<OutputName> for OutputsBuilder {
    fn from_iter<T: IntoIterator<Item = OutputName>>(iter: T) -> Self {
        OutputsBuilder::InputAddressed(iter.into_iter().collect())
    }
}

/// Errors that can occur during the creation and validation of [`Outputs`].
#[derive(Debug, PartialEq, thiserror::Error)]
#[allow(missing_docs)]
pub enum OutputsError {
    #[error("no outputs defined")]
    NoOutputs(),
    #[error("outputs failed verification")]
    VerificationError(),
    #[error("invalid output name: {0}")]
    InvalidOutputName(String),
    #[error("duplicate output name: {0}")]
    DuplicateOutputName(OutputName),
    #[error("encountered fixed-output derivation, but more than 1 output in total")]
    MoreThanOneOutputButFixed(),
    #[error("invalid output name for fixed-output derivation: {0}")]
    InvalidOutputNameForFixed(String),
    #[error("invalid calculated output derivation path name: {0}")]
    InvalidOutputDerivationPath(String, #[source] store_path::ParseStorePathError),
}

/// An iterator over the entries of `Outputs`.
///
/// This `struct` is created by the [`iter`] method on [`Outputs`]. See its
/// documentation for more.
///
/// [`iter`]: Outputs::iter
pub struct Iter<'a>(IterI<'a>);
impl fmt::Debug for Iter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Iter")
    }
}

enum IterI<'a> {
    Fixed(std::iter::Once<(&'a OutputName, &'a StorePath)>),
    InputAddressed(std::collections::btree_map::Iter<'a, OutputName, StorePath>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a OutputName, &'a StorePath);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterI::Fixed(it) => it.next(),
            IterI::InputAddressed(it) => it.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterI::Fixed(it) => it.size_hint(),
            IterI::InputAddressed(it) => it.size_hint(),
        }
    }
}

impl<'a> ExactSizeIterator for Iter<'a> {}

/// An iterator over the entries of `Outputs`.
///
/// This `struct` is created by the [`into_iter`] method on [`Outputs`]. See its
/// documentation for more.
///
/// [`into_iter`]: Outputs::into_iter
pub struct IntoIter(IntoIterI);

impl fmt::Debug for IntoIter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IntoIter")
    }
}

enum IntoIterI {
    Fixed(std::iter::Once<(OutputName, StorePath)>),
    InputAddressed(std::collections::btree_map::IntoIter<OutputName, StorePath>),
}

impl Iterator for IntoIter {
    type Item = (OutputName, StorePath);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IntoIterI::Fixed(it) => it.next(),
            IntoIterI::InputAddressed(it) => it.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IntoIterI::Fixed(it) => it.size_hint(),
            IntoIterI::InputAddressed(it) => it.size_hint(),
        }
    }
}

impl ExactSizeIterator for IntoIter {}

/// An iterator over the names of outputs.
///
/// This `struct` is created by [`OutputsBuilder::names`] and [`Outputs::names`].
/// See their documentation for more.
pub struct OutputNames<'a>(OutputNameInner<'a>);

impl fmt::Debug for OutputNames<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutputNames")
    }
}

impl<'a> Iterator for OutputNames<'a> {
    type Item = &'a OutputName;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            OutputNameInner::Single(it) => it.next(),
            OutputNameInner::BTreeSet(it) => it.next(),
            OutputNameInner::BTreeMap(it) => it.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            OutputNameInner::Single(it) => it.size_hint(),
            OutputNameInner::BTreeSet(it) => it.size_hint(),
            OutputNameInner::BTreeMap(it) => it.size_hint(),
        }
    }
}
impl<'a> ExactSizeIterator for OutputNames<'a> {}

enum OutputNameInner<'a> {
    Single(std::iter::Once<&'a OutputName>),
    BTreeSet(std::collections::btree_set::Iter<'a, OutputName>),
    BTreeMap(std::collections::btree_map::Keys<'a, OutputName, StorePath>),
}

/// An iterator over the owned names of outputs.
///
/// This `struct` is created by [`OutputsBuilder::into_names`] and [`Outputs::into_names`].
/// See their documentation for more.
pub struct IntoOutputNames(IntoOutputNameInner);
impl fmt::Debug for IntoOutputNames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IntoOutputNames")
    }
}

impl Iterator for IntoOutputNames {
    type Item = OutputName;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IntoOutputNameInner::Single(it) => it.next(),
            IntoOutputNameInner::BTreeSet(it) => it.next(),
            IntoOutputNameInner::BTreeMap(it) => it.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IntoOutputNameInner::Single(it) => it.size_hint(),
            IntoOutputNameInner::BTreeSet(it) => it.size_hint(),
            IntoOutputNameInner::BTreeMap(it) => it.size_hint(),
        }
    }
}
impl ExactSizeIterator for IntoOutputNames {}

enum IntoOutputNameInner {
    Single(std::iter::Once<OutputName>),
    BTreeSet(std::collections::btree_set::IntoIter<OutputName>),
    BTreeMap(std::collections::btree_map::IntoKeys<OutputName, StorePath>),
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use rstest::rstest;

    use crate::{
        derivation::{OutputHash, OutputHashMode, OutputName, Outputs, OutputsBuilder},
        nixhash::NixHash,
        store_path::StorePath,
    };

    const DIGEST_SHA256: [u8; 32] =
        hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    const OUTPUT_HASH: OutputHash = OutputHash {
        mode: OutputHashMode::Flat,
        hash: NixHash::Sha256(DIGEST_SHA256),
    };
    static STORE_PATH: LazyLock<StorePath> = LazyLock::new(|| {
        StorePath::from_bytes(b"2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib").unwrap()
    });

    const FOD_OUTPUTS_BUILDER: OutputsBuilder = OutputsBuilder::Fixed(OutputHash {
        mode: OutputHashMode::Flat,
        hash: NixHash::Sha256(DIGEST_SHA256),
    });
    static SINGLE_OUTPUTS_BUILDER: LazyLock<OutputsBuilder> =
        LazyLock::new(|| OutputsBuilder::from_iter([OutputName::out()]));
    static SINGLE_NON_OUT_OUTPUTS_BUILDER: LazyLock<OutputsBuilder> =
        LazyLock::new(|| OutputsBuilder::from_iter([OutputName::from_static("bin").unwrap()]));
    static MULTIPLE_OUTPUTS_BUILDER: LazyLock<OutputsBuilder> = LazyLock::new(|| {
        OutputsBuilder::from_iter([
            OutputName::from_static("dev").unwrap(),
            OutputName::from_static("bin").unwrap(),
        ])
    });

    static FOD_OUTPUTS: LazyLock<Outputs> =
        LazyLock::new(|| Outputs::fixed_output(OUTPUT_HASH.clone(), STORE_PATH.clone()));
    static TRY_SINGLE_OUTPUTS: LazyLock<Outputs> = LazyLock::new(|| {
        Outputs::input_addressed_from_iter([(OutputName::out(), STORE_PATH.clone())])
            .expect("single output")
    });
    static TRY_SINGLE_NON_OUT_OUTPUTS: LazyLock<Outputs> = LazyLock::new(|| {
        Outputs::input_addressed_from_iter([(
            OutputName::from_static("bin").unwrap(),
            STORE_PATH.clone(),
        )])
        .expect("single output")
    });
    static TRY_FOD_OUTPUTS: LazyLock<Outputs> = LazyLock::new(|| -> Outputs {
        Outputs::try_from_iter([(
            OutputName::out(),
            STORE_PATH.clone(),
            Some(OUTPUT_HASH.clone()),
        )])
        .expect("single fod")
    });
    static TRY_MULTIPLE_OUTPUTS: LazyLock<Outputs> = LazyLock::new(|| -> Outputs {
        Outputs::input_addressed_from_iter([
            (OutputName::from_static("dev").unwrap(), STORE_PATH.clone()),
            (OutputName::from_static("bin").unwrap(), STORE_PATH.clone()),
        ])
        .expect("multiple")
    });

    mod outputs_builder {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER)]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER)]
        #[case::fod(&FOD_OUTPUTS_BUILDER)]
        #[case::default(&Default::default())]
        fn is_single(#[case] value: &OutputsBuilder) {
            assert!(value.is_single())
        }

        #[rstest]
        #[case::multiple(&MULTIPLE_OUTPUTS_BUILDER)]
        fn is_not_single(#[case] value: &OutputsBuilder) {
            assert!(!value.is_single())
        }

        #[rstest]
        #[case::fod(&FOD_OUTPUTS_BUILDER)]
        fn is_fixed(#[case] value: &OutputsBuilder) {
            assert!(value.is_fixed())
        }

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER)]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER)]
        #[case::multiple(&MULTIPLE_OUTPUTS_BUILDER)]
        #[case::default(&Default::default())]
        fn is_not_fixed(#[case] value: &OutputsBuilder) {
            assert!(!value.is_fixed())
        }

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER, 1)]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER, 1)]
        #[case::fod(&FOD_OUTPUTS_BUILDER, 1)]
        #[case::multiple(&MULTIPLE_OUTPUTS_BUILDER, 2)]
        #[case::default(&Default::default(), 1)]
        fn len(#[case] value: &OutputsBuilder, #[case] expected: usize) {
            assert_eq!(value.len(), expected)
        }

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER, OutputName::out())]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER, "bin")]
        #[case::fod(&FOD_OUTPUTS_BUILDER, OutputName::out())]
        #[case::multiple_bin(&MULTIPLE_OUTPUTS_BUILDER, "bin")]
        #[case::default(&Default::default(), OutputName::out())]
        fn contains(#[case] value: &OutputsBuilder, #[case] name: OutputName) {
            assert!(value.contains(&name))
        }

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER, "bin")]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER, "out")]
        #[case::fod(&FOD_OUTPUTS_BUILDER, "bin")]
        #[case::multiple_out(&MULTIPLE_OUTPUTS_BUILDER, OutputName::out())]
        fn does_not_contain(#[case] value: &OutputsBuilder, #[case] name: OutputName) {
            assert!(!value.contains(&name))
        }

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER, vec![OutputName::out()])]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER, vec![OutputName::from_static("bin").unwrap()])]
        #[case::fod(&FOD_OUTPUTS_BUILDER, vec![OutputName::out()])]
        #[case::multiple(&MULTIPLE_OUTPUTS_BUILDER, vec![
            OutputName::from_static("bin").unwrap(),
            OutputName::from_static("dev").unwrap(),
        ])]
        #[case::default(&Default::default(), vec![OutputName::out()])]
        fn names(#[case] value: &OutputsBuilder, #[case] expected: Vec<OutputName>) {
            let actual: Vec<_> = value.names().cloned().collect();
            assert_eq!(actual, expected);
        }

        #[rstest]
        #[case::single(&SINGLE_OUTPUTS_BUILDER, vec![OutputName::out()])]
        #[case::single_non_out(&SINGLE_NON_OUT_OUTPUTS_BUILDER, vec![OutputName::from_static("bin").unwrap()])]
        #[case::fod(&FOD_OUTPUTS_BUILDER, vec![OutputName::out()])]
        #[case::multiple(&MULTIPLE_OUTPUTS_BUILDER, vec![
            OutputName::from_static("bin").unwrap(),
            OutputName::from_static("dev").unwrap(),
        ])]
        #[case::default(&Default::default(), vec![OutputName::out()])]
        fn into_names(#[case] value: &OutputsBuilder, #[case] expected: Vec<OutputName>) {
            let actual: Vec<_> = value.clone().into_names().collect();
            assert_eq!(actual, expected);
        }
    }

    #[rstest]
    #[should_panic(expected = "no outputs defined")]
    #[case::empty(&[])]
    #[should_panic(expected = "duplicate output name: bin")]
    #[case::duplicate(&[
        (OutputName::from_static("bin").unwrap(), STORE_PATH.clone(), None),
        (OutputName::from_static("bin").unwrap(), STORE_PATH.clone(), None),
    ])]
    #[should_panic(
        expected = "encountered fixed-output derivation, but more than 1 output in total"
    )]
    #[case::mixed(&[
        (OutputName::from_static("bin").unwrap(), STORE_PATH.clone(), Some(OUTPUT_HASH.clone())),
        (OutputName::from_static("dev").unwrap(), STORE_PATH.clone(), None),
    ])]
    fn try_from_iter_failure(#[case] it: &[(OutputName, StorePath, Option<OutputHash>)]) {
        panic!(
            "{}",
            Outputs::try_from_iter(it.iter().cloned()).expect_err("try_from_iter succeeded")
        );
    }

    #[rstest]
    #[should_panic(expected = "no outputs defined")]
    #[case::empty(&[])]
    #[should_panic(expected = "duplicate output name: bin")]
    #[case::duplicate(&[
        (OutputName::from_static("bin").unwrap(), STORE_PATH.clone()),
        (OutputName::from_static("bin").unwrap(), STORE_PATH.clone()),
    ])]
    fn input_addressed_from_iter_failure(#[case] it: &[(OutputName, StorePath)]) {
        panic!(
            "{}",
            Outputs::input_addressed_from_iter(it.iter().cloned())
                .expect_err("try_from_iter succeeded")
        );
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS)]
    #[case::try_single(&TRY_SINGLE_OUTPUTS)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS)]
    #[case::try_fod(&TRY_FOD_OUTPUTS)]
    fn is_single(#[case] value: &Outputs) {
        assert!(value.is_single())
    }

    #[rstest]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS)]
    fn is_not_single(#[case] value: &Outputs) {
        assert!(!value.is_single())
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS)]
    #[case::try_fod(&TRY_FOD_OUTPUTS)]
    fn is_fixed(#[case] value: &Outputs) {
        assert!(value.is_fixed())
    }

    #[rstest]
    #[case::try_single(&TRY_SINGLE_OUTPUTS)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS)]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS)]
    fn is_not_fixed(#[case] value: &Outputs) {
        assert!(!value.is_fixed())
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, 1)]
    #[case::try_fod(&TRY_FOD_OUTPUTS, 1)]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, 1)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, 1)]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS, 2)]
    fn len(#[case] value: &Outputs, #[case] expected: usize) {
        assert_eq!(value.len(), expected)
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, OutputName::out(), Some(&*STORE_PATH))]
    #[case::try_fod(&TRY_FOD_OUTPUTS, OutputName::out(), Some(&*STORE_PATH))]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, OutputName::out(), Some(&*STORE_PATH))]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, OutputName::from_static("bin").unwrap(), Some(&*STORE_PATH))]
    #[case::multiple_out(&TRY_MULTIPLE_OUTPUTS, OutputName::out(), None)]
    #[case::multiple_bin(&TRY_MULTIPLE_OUTPUTS, OutputName::from_static("bin").unwrap(), Some(&*STORE_PATH))]
    fn get(
        #[case] value: &Outputs,
        #[case] name: OutputName,
        #[case] expected: Option<&StorePath>,
    ) {
        assert_eq!(value.get(&name), expected)
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, OutputName::out())]
    #[case::try_fod(&TRY_FOD_OUTPUTS, OutputName::out())]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, OutputName::out())]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, OutputName::from_static("bin").unwrap())]
    #[case::multiple_bin(&TRY_MULTIPLE_OUTPUTS, OutputName::from_static("bin").unwrap())]
    fn contains_key(#[case] value: &Outputs, #[case] name: OutputName) {
        assert!(value.contains_key(&name))
    }

    #[rstest]
    #[case::multiple_out(&TRY_MULTIPLE_OUTPUTS, OutputName::out())]
    fn does_not_contain_key(#[case] value: &Outputs, #[case] name: OutputName) {
        assert!(!value.contains_key(&name))
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![(OutputName::out(), STORE_PATH.clone())])]
    #[case::try_fod(&TRY_FOD_OUTPUTS, vec![(OutputName::out(), STORE_PATH.clone())])]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, vec![(OutputName::out(), STORE_PATH.clone())])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, vec![(OutputName::from_static("bin").unwrap(), STORE_PATH.clone())])]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS, vec![
        (OutputName::from_static("bin").unwrap(), STORE_PATH.clone()),
        (OutputName::from_static("dev").unwrap(), STORE_PATH.clone()),
    ])]
    fn iter(#[case] value: &Outputs, #[case] expected: Vec<(OutputName, StorePath)>) {
        let actual: Vec<_> = value
            .iter()
            .map(|(name, output)| (name.clone(), output.clone()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![OutputName::out()])]
    #[case::try_fod(&TRY_FOD_OUTPUTS, vec![OutputName::out()])]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, vec![OutputName::out()])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, vec![OutputName::from_static("bin").unwrap()])]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS, vec![
        OutputName::from_static("bin").unwrap(),
        OutputName::from_static("dev").unwrap(),
    ])]
    fn names(#[case] value: &Outputs, #[case] expected: Vec<OutputName>) {
        let actual: Vec<_> = value.names().cloned().collect();
        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![OutputName::out()])]
    #[case::try_fod(&TRY_FOD_OUTPUTS, vec![OutputName::out()])]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, vec![OutputName::out()])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, vec![OutputName::from_static("bin").unwrap()])]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS, vec![
        OutputName::from_static("bin").unwrap(),
        OutputName::from_static("dev").unwrap(),
    ])]
    fn into_names(#[case] value: &Outputs, #[case] expected: Vec<OutputName>) {
        let actual: Vec<_> = value.clone().into_names().collect();
        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![STORE_PATH.clone()])]
    #[case::try_fod(&TRY_FOD_OUTPUTS, vec![STORE_PATH.clone()])]
    #[case::try_single(&TRY_SINGLE_OUTPUTS, vec![STORE_PATH.clone()])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS, vec![STORE_PATH.clone()])]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS, vec![STORE_PATH.clone(), STORE_PATH.clone()])]
    fn values(#[case] value: &Outputs, #[case] expected: Vec<StorePath>) {
        let actual: Vec<_> = value.store_paths().cloned().collect();
        assert_eq!(actual, expected);
    }

    #[cfg(feature = "serde")]
    #[rstest]
    #[case::fod(&FOD_OUTPUTS)]
    #[case::try_fod(&TRY_FOD_OUTPUTS)]
    #[case::try_single(&TRY_SINGLE_OUTPUTS)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT_OUTPUTS)]
    #[case::multiple(&TRY_MULTIPLE_OUTPUTS)]
    fn serde(#[case] value: &Outputs) {
        let serialize = serde_json::to_string_pretty(&value).unwrap();
        let actual: Outputs = serde_json::from_str(&serialize).unwrap();
        assert_eq!(&actual, value);
    }

    /// This ensures that a potentially valid input addressed
    /// output is deserialized as a non-fixed output.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialize_valid_input_addressed_output() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432"
            }
        }"#;
        let output: Outputs = serde_json::from_str(json_bytes).expect("must parse");

        assert!(!output.is_fixed());
    }

    /// This ensures that a potentially valid fixed output
    /// output deserializes fine as a fixed output.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialize_valid_fixed_output() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
                "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
                "hashAlgo": "r:sha256"
            }
        }"#;
        let output: Outputs = serde_json::from_str(json_bytes).expect("must parse");

        assert!(output.is_fixed());
    }

    /// This ensures that parsing an input with the invalid hash encoding
    /// will result in a parsing failure.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialize_with_error_invalid_hash_encoding_fixed_output() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
                "hash": "IAMNOTVALIDNIXBASE32",
                "hashAlgo": "r:sha256"
            }
        }"#;
        let output: Result<Outputs, _> = serde_json::from_str(json_bytes);

        assert!(output.is_err());
    }

    /// This ensures that parsing an input with the wrong hash algo
    /// will result in a parsing failure.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialize_with_error_invalid_hash_algo_fixed_output() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
                "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
                "hashAlgo": "r:sha1024"
            }
        }"#;
        let output: Result<Outputs, _> = serde_json::from_str(json_bytes);

        assert!(output.is_err());
    }

    /// This ensures that parsing an input with the missing hash algo but present hash will result in a
    /// parsing failure.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialize_with_error_missing_hash_algo_fixed_output() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
                "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
            }
        }"#;
        let output: Result<Outputs, _> = serde_json::from_str(json_bytes);

        assert!(output.is_err());
    }

    /// This ensures that parsing an input with the missing hash but present hash algo will result in a
    /// parsing failure.
    #[cfg(feature = "serde")]
    #[test]
    fn deserialize_with_error_missing_hash_fixed_output() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
                "hashAlgo": "r:sha1024"
            }
        }"#;
        let output: Result<Outputs, _> = serde_json::from_str(json_bytes);

        assert!(output.is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serialize_deserialize() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432"
            }
        }"#;
        let output: Outputs = serde_json::from_str(json_bytes).expect("must parse");

        let s = serde_json::to_string(&output).expect("Serialize");
        let output2: Outputs = serde_json::from_str(&s).expect("must parse again");

        assert_eq!(output, output2);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serialize_deserialize_fixed() {
        let json_bytes = r#"
        {
            "out": {
                "path": "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-net-tools-1.60_p20170221182432",
                "hash": "08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba",
                "hashAlgo": "r:sha256"
            }
        }"#;
        let output: Outputs = serde_json::from_str(json_bytes).expect("must parse");

        let s = serde_json::to_string_pretty(&output).expect("Serialize");
        let output2: Outputs = serde_json::from_str(&s).expect("must parse again");

        assert_eq!(output, output2);
    }
}
