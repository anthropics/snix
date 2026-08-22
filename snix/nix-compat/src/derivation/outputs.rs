//! Outputs for a derivation.
use std::{collections::BTreeMap, fmt::Write as _};

use crate::{
    derivation::{Output, OutputHash, OutputHashMode, OutputName, output::ParseOutputError},
    nixhash::Sha256,
    store_path,
};

/// Errors that can occur during the creation and validation of [`Outputs`].
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum OutputsError {
    #[error("no outputs defined")]
    NoOutputs(),
    #[error("invalid output name: {0}")]
    InvalidOutputName(String),
    #[error("duplicate output name: {0}")]
    DuplicateOutputName(OutputName),
    #[error("encountered fixed-output derivation, but more than 1 output in total")]
    MoreThanOneOutputButFixed(),
    #[error("invalid output name for fixed-output derivation: {0}")]
    InvalidOutputNameForFixed(String),
    #[error("unable to validate output {0}: {1}")]
    InvalidOutput(String, #[source] ParseOutputError),
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

enum IterI<'a> {
    Fixed(std::iter::Once<(&'a OutputName, &'a Output)>),
    InputAddressed(std::collections::btree_map::Iter<'a, OutputName, Output>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a OutputName, &'a Output);

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

#[derive(Clone, Debug, PartialEq, Eq)]
enum OutputsInner {
    Fixed(Output),
    InputAddressed(BTreeMap<OutputName, Output>),
}

/// Derivation outputs.
///
/// Outputs are generally mappings from [`OutputName`] to a [`Output`] but
/// with some extra invariants.
///
/// # Invariants
/// - Only a single FOD output is allowed and it must be named `out`
/// - Duplicately named outputs is an error
/// - Outputs can not be empty
///
/// # `StorePath` for an `Output`
/// When a derivation is being constructed the [`StorePath`] of each output
/// is not known yet and so `Outputs` does support this.
///
/// But generally you should call [`calculate_output_paths`] and after that
/// the [`StorePath`] of each output is available and will never change.
///
/// It is for this reason that the only method mutating `Outputs` after
/// construction is [`calculate_output_paths`].
///
/// [`StorePath`]: store_path::StorePath
/// [`calculate_output_paths`]: Outputs::calculate_output_paths
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outputs(OutputsInner);

impl Outputs {
    /// Return `Ouputs` with single fixed-output that matches `output_hash`.
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    ///
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// let b = Outputs::from_fod_hash(output_hash);
    /// assert!(b.is_single());
    /// assert!(b.is_fixed());
    /// assert!(b.contains_key(&OutputName::out()));
    /// ```
    pub const fn from_fod_hash(output_hash: OutputHash) -> Self {
        Outputs(OutputsInner::Fixed(Output {
            path: None,
            output_hash: Some(output_hash),
        }))
    }

    /// Try to make `Outputs` from the provided iterator.
    ///
    /// This will return a [`OutputsError`] if the [`OutputName`], [`Output`] pairs
    /// in the iterator don't follow the [invariants].
    ///
    /// [invariants]: #invariants
    pub fn try_from_iter<I>(it: I) -> Result<Self, OutputsError>
    where
        I: IntoIterator<Item = (OutputName, Output)>,
    {
        let mut it = it.into_iter().peekable();
        let Some((output_name, output)) = it.next() else {
            return Err(OutputsError::NoOutputs());
        };
        let mut outputs = BTreeMap::new();
        if output.is_fixed() {
            if it.peek().is_some() {
                return Err(OutputsError::MoreThanOneOutputButFixed());
            }
            if output_name != OutputName::out() {
                return Err(OutputsError::InvalidOutputName(output_name.to_string()));
            }
            Ok(Outputs(OutputsInner::Fixed(output)))
        } else {
            outputs.insert(output_name, output);
            for (output_name, output) in it {
                if output.is_fixed() {
                    return Err(OutputsError::MoreThanOneOutputButFixed());
                }
                if outputs.insert(output_name.clone(), output).is_some() {
                    return Err(OutputsError::DuplicateOutputName(output_name));
                }
            }
            Ok(Outputs(OutputsInner::InputAddressed(outputs)))
        }
    }

    /// Returns `true` if the outputs contains an output with the specified `name`.
    #[must_use]
    pub fn contains_key(&self, name: &OutputName) -> bool {
        match &self.0 {
            OutputsInner::Fixed(_) => *name == OutputName::out(),
            OutputsInner::InputAddressed(outputs) => outputs.contains_key(name),
        }
    }

    /// Returns a reference to the [`Output`] corresponding to the provided `name`.
    pub fn get(&self, name: &OutputName) -> Option<&Output> {
        match &self.0 {
            OutputsInner::Fixed(output) if *name == OutputName::out() => Some(output),
            OutputsInner::InputAddressed(outputs) => outputs.get(name),
            _ => None,
        }
    }

    /// Gets an iterator over the entries of the outputs, sorted by name.
    pub fn iter(&self) -> Iter<'_> {
        match &self.0 {
            OutputsInner::Fixed(output) => {
                const OUT: &OutputName = &OutputName::out();
                Iter(IterI::Fixed(std::iter::once((OUT, output))))
            }
            OutputsInner::InputAddressed(outputs) => Iter(IterI::InputAddressed(outputs.iter())),
        }
    }

    /// Gets an iterator over the names of the outputs, in sorted order.
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    ///
    /// let a = Outputs::try_from_iter([
    ///     (OutputName::out(), Default::default()),
    ///     (OutputName::from_static("bin").unwrap(), Default::default()),
    /// ]).expect("multiple outputs");
    ///
    /// let keys: Vec<OutputName> = a.keys().cloned().collect();
    /// assert_eq!(keys, [
    ///     OutputName::from_static("bin").unwrap(),
    ///     OutputName::out(),
    /// ]);
    /// ```
    pub fn keys(&self) -> impl Iterator<Item = &OutputName> {
        self.iter().map(|(name, _)| name)
    }

    /// Gets an iterator over the [`Output`] values of the outputs, in order by name.
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Output, Outputs, OutputName};
    ///
    /// let a = Outputs::default();
    ///
    /// let values: Vec<Output> = a.values().cloned().collect();
    /// assert_eq!(values, [Output::default()]);
    /// ```
    pub fn values(&self) -> impl Iterator<Item = &Output> {
        self.iter().map(|(_, output)| output)
    }

    /// Returns the number of outputs
    ///
    /// # Examples
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    ///
    /// let a = Outputs::default();
    /// assert_eq!(a.len(), 1);
    ///
    /// let b = Outputs::try_from_iter([
    ///     (OutputName::out(), Default::default()),
    ///     (OutputName::from_static("bin").unwrap(), Default::default()),
    /// ]).expect("multiple outputs");
    /// assert_eq!(b.len(), 2);
    /// ```
    #[expect(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        match &self.0 {
            OutputsInner::Fixed(_) => 1,
            OutputsInner::InputAddressed(outputs) => outputs.len(),
        }
    }

    /// Returns `true` if this contains only a single output.
    ///
    /// # Examples
    ///
    /// ```
    /// use nix_compat::derivation::{Outputs, OutputName};
    /// # use nix_compat::derivation::{OutputHash, OutputHashMode};
    /// # use nix_compat::nixhash::NixHash;
    ///
    /// let a = Outputs::default();
    /// assert!(a.is_single());
    ///
    /// # const DIGEST_SHA256: [u8; 32] =
    /// #     hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    /// # const NIXHASH_SHA256: NixHash = NixHash::Sha256(DIGEST_SHA256);
    /// # let output_hash = OutputHash { mode: OutputHashMode::Flat, hash: NIXHASH_SHA256.clone() };
    /// let b = Outputs::from_fod_hash(output_hash);
    /// assert!(b.is_single());
    ///
    /// let c = Outputs::try_from_iter([
    ///     (OutputName::out(), Default::default()),
    ///     (OutputName::from_static("bin").unwrap(), Default::default()),
    /// ]).unwrap();
    /// assert!(!c.is_single());
    /// ```
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.len() == 1
    }

    /// Returns `true` if this is a single fixed-output.
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        matches!(&self.0, OutputsInner::Fixed(_))
    }

    /// This calculates all output paths of a `Outputs` and updates the struct.
    ///
    /// It requires the struct to be initially without output paths.
    /// This means, [`Output::path`] needs to be `None` for each output.
    ///
    /// Output path calculation requires knowledge of the
    /// [`hash_derivation_modulo`], which (in case of non-fixed-output
    /// derivations) also requires knowledge of the
    /// [`hash_derivation_modulo`] of input derivations (recursively).
    ///
    /// To avoid recursing and doing unnecessary calculation, we simply
    /// ask the caller of this function to provide the result of the
    /// [`hash_derivation_modulo`] call of the current [`Derivation`],
    /// and leave it up to them to calculate it when needed.
    ///
    /// On completion, [`Output::path`] of each output is set to the calculated output path.
    ///
    /// [`hash_derivation_modulo`]: super::Derivation::hash_derivation_modulo
    /// [`Derivation`]: super::Derivation
    pub fn calculate_output_paths(
        &mut self,
        drv_name: &str,
        hash_derivation_modulo: &Sha256,
    ) -> Result<(), OutputsError> {
        match &mut self.0 {
            OutputsInner::Fixed(output) => {
                // Assert that outputs are not yet populated, to avoid using this function wrongly.
                // We don't also go over self.environment, but it's a sufficient
                // footgun prevention mechanism.
                assert!(output.path.is_none());

                // Assemble the name, which is either the drv-name suffixed `-{output_name}`,
                // except in the `out` case, where it's omitted.
                let name = drv_name.to_owned();

                // For fixed output derivation we use [build_ca_path], otherwise we
                // use [build_output_path] with [hash_derivation_modulo].
                let store_path = if let Some(output_hash) = &output.output_hash {
                    store_path::build_ca_path(
                        &name,
                        output_hash.mode == OutputHashMode::Recursive,
                        &output_hash.hash,
                        [],
                        false,
                    )
                } else {
                    store_path::build_output_path(&name, hash_derivation_modulo, &OutputName::out())
                }
                .map_err(|e| OutputsError::InvalidOutputDerivationPath(name.to_string(), e))?;

                output.path = Some(store_path.to_owned());
            }
            OutputsInner::InputAddressed(outputs) => {
                for (output_name, output) in outputs.iter_mut() {
                    debug_assert!(!output.is_fixed(), "Snix bug: multiple FOD");

                    // Assemble the name, which is either the drv-name suffixed `-{output_name}`,
                    // except in the `out` case, where it's omitted.
                    let name = {
                        let mut name = drv_name.to_owned();
                        if output_name != &OutputName::out() {
                            name.write_fmt(format_args!("-{output_name}")).unwrap();
                        }
                        name
                    };

                    // use [build_output_path] with [hash_derivation_modulo].
                    let store_path =
                        store_path::build_output_path(&name, hash_derivation_modulo, output_name)
                            .map_err(|e| {
                            OutputsError::InvalidOutputDerivationPath(name.to_string(), e)
                        })?;

                    output.path = Some(store_path.to_owned());
                }
            }
        }
        Ok(())
    }
}

impl<'a> IntoIterator for &'a Outputs {
    type Item = (&'a OutputName, &'a Output);

    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Default for Outputs {
    fn default() -> Self {
        Self(OutputsInner::InputAddressed(BTreeMap::from_iter([(
            OutputName::out(),
            Output::default(),
        )])))
    }
}

impl TryFrom<BTreeMap<OutputName, Output>> for Outputs {
    type Error = OutputsError;

    fn try_from(outputs: BTreeMap<OutputName, Output>) -> Result<Self, Self::Error> {
        Self::try_from_iter(outputs)
    }
}

#[cfg(test)]
impl Outputs {
    /// Clear any [`StorePath`] from each output.
    pub fn trim_store_paths(&mut self) {
        match &mut self.0 {
            OutputsInner::Fixed(output) => {
                output.path = None;
            }
            OutputsInner::InputAddressed(outputs) => {
                for output in outputs.values_mut() {
                    output.path = None;
                }
            }
        }
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
                use serde::de::{Error, Unexpected};
                let Some((output_name, output)) = map.next_entry::<OutputName, Output>()? else {
                    return Err(A::Error::invalid_length(0, &"non-empty derivation outputs"));
                };
                if let Some((second_name, second_output)) =
                    map.next_entry::<OutputName, Output>()?
                {
                    if output.is_fixed() || second_output.is_fixed() {
                        return Err(A::Error::invalid_value(
                            Unexpected::Other("FOD output"),
                            &"non-FOD outputs",
                        ));
                    }
                    let mut outputs = BTreeMap::new();
                    outputs.insert(output_name, output);
                    if outputs.insert(second_name, second_output).is_some() {
                        return Err(A::Error::custom(format_args!(
                            "duplicate derivation output"
                        )));
                    }
                    while let Some((output_name, output)) =
                        map.next_entry::<OutputName, Output>()?
                    {
                        if output.is_fixed() {
                            return Err(A::Error::invalid_value(
                                Unexpected::Other("FOD output"),
                                &"non-FOD outputs",
                            ));
                        }
                        if outputs.insert(output_name, output).is_some() {
                            return Err(A::Error::custom(format_args!(
                                "duplicate derivation output"
                            )));
                        }
                    }
                    Ok(Outputs(OutputsInner::InputAddressed(outputs)))
                } else if output.is_fixed() {
                    if output_name != OutputName::out() {
                        return Err(A::Error::invalid_value(
                            Unexpected::Other(output_name.as_str()),
                            &"single derivation output named 'out'",
                        ));
                    }
                    Ok(Outputs(OutputsInner::Fixed(output)))
                } else {
                    Ok(Outputs(OutputsInner::InputAddressed(BTreeMap::from_iter(
                        [(output_name, output)],
                    ))))
                }
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
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (k, v) in self {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::LazyLock};

    use rstest::rstest;

    use crate::{
        derivation::{Output, OutputHash, OutputHashMode, OutputName, Outputs},
        nixhash::NixHash,
    };

    const DIGEST_SHA256: [u8; 32] =
        hex_literal::hex!("a5ce9c155ed09397614646c9717fc7cd94b1023d7b76b618d409e4fefd6e9d39");
    const OUTPUT_HASH: OutputHash = OutputHash {
        mode: OutputHashMode::Flat,
        hash: NixHash::Sha256(DIGEST_SHA256),
    };
    const FOD_OUTPUT: Output = Output {
        output_hash: Some(OutputHash {
            mode: OutputHashMode::Flat,
            hash: NixHash::Sha256(DIGEST_SHA256),
        }),
        path: None,
    };

    const FOD_OUTPUTS: Outputs = Outputs::from_fod_hash(OutputHash {
        mode: OutputHashMode::Flat,
        hash: NixHash::Sha256(DIGEST_SHA256),
    });
    static TRY_SINGLE: LazyLock<Outputs> = LazyLock::new(|| {
        Outputs::try_from_iter([(OutputName::out(), Default::default())]).expect("single output")
    });
    static TRY_SINGLE_NON_OUT: LazyLock<Outputs> = LazyLock::new(|| {
        Outputs::try_from_iter([(OutputName::from_static("bin").unwrap(), Default::default())])
            .expect("single output")
    });
    static TRY_FOD: LazyLock<Outputs> = LazyLock::new(|| -> Outputs {
        Outputs::try_from_iter([(
            OutputName::out(),
            Output {
                path: None,
                output_hash: Some(OUTPUT_HASH.clone()),
            },
        )])
        .expect("single fod")
    });
    static TRY_MULTIPLE: LazyLock<Outputs> = LazyLock::new(|| -> Outputs {
        Outputs::try_from_iter([
            (OutputName::from_static("dev").unwrap(), Default::default()),
            (OutputName::from_static("bin").unwrap(), Default::default()),
        ])
        .expect("multiple")
    });

    #[rstest]
    #[should_panic(expected = "no outputs defined")]
    #[case::empty(&[])]
    #[should_panic(expected = "invalid output name: bin")]
    #[case::fod_non_out(&[(OutputName::from_static("bin").unwrap(), FOD_OUTPUT)])]
    #[should_panic(expected = "duplicate output name: bin")]
    #[case::duplicate(&[
        (OutputName::from_static("bin").unwrap(), Default::default()),
        (OutputName::from_static("bin").unwrap(), Default::default()),
    ])]
    #[should_panic(
        expected = "encountered fixed-output derivation, but more than 1 output in total"
    )]
    #[case::mixed(&[
        (OutputName::from_static("bin").unwrap(), FOD_OUTPUT.clone()),
        (OutputName::from_static("dev").unwrap(), Default::default()),
    ])]
    fn try_from_iter_failure(#[case] it: &[(OutputName, Output)]) {
        panic!(
            "{}",
            Outputs::try_from_iter(it.iter().cloned()).expect_err("try_from_iter succeeded")
        );
    }

    #[rstest]
    #[should_panic(expected = "no outputs defined")]
    #[case::empty(&[])]
    #[should_panic(expected = "invalid output name: bin")]
    #[case::fod_non_out(&[(OutputName::from_static("bin").unwrap(), FOD_OUTPUT)])]
    #[should_panic(
        expected = "encountered fixed-output derivation, but more than 1 output in total"
    )]
    #[case::mixed(&[
        (OutputName::from_static("bin").unwrap(), FOD_OUTPUT.clone()),
        (OutputName::from_static("dev").unwrap(), Default::default()),
    ])]
    fn try_from_failure(#[case] it: &[(OutputName, Output)]) {
        let b = BTreeMap::from_iter(it.iter().cloned());
        panic!("{}", Outputs::try_from(b).expect_err("try_from succeeded"));
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS)]
    #[case::try_single(&TRY_SINGLE)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT)]
    #[case::try_fod(&TRY_FOD)]
    #[case::default(&Default::default())]
    fn is_single(#[case] value: &Outputs) {
        assert!(value.is_single())
    }

    #[rstest]
    #[case::multiple(&TRY_MULTIPLE)]
    fn is_not_single(#[case] value: &Outputs) {
        assert!(!value.is_single())
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS)]
    #[case::try_fod(&TRY_FOD)]
    fn is_fixed(#[case] value: &Outputs) {
        assert!(value.is_fixed())
    }

    #[rstest]
    #[case::try_single(&TRY_SINGLE)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT)]
    #[case::multiple(&TRY_MULTIPLE)]
    #[case::default(&Default::default())]
    fn is_not_fixed(#[case] value: &Outputs) {
        assert!(!value.is_fixed())
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, 1)]
    #[case::try_fod(&TRY_FOD, 1)]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT, 1)]
    #[case::try_single(&TRY_SINGLE, 1)]
    #[case::multiple(&TRY_MULTIPLE, 2)]
    #[case::default(&Default::default(), 1)]
    fn len(#[case] value: &Outputs, #[case] expected: usize) {
        assert_eq!(value.len(), expected)
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, OutputName::out(), Some(&FOD_OUTPUT))]
    #[case::try_fod(&TRY_FOD, OutputName::out(), Some(&FOD_OUTPUT))]
    #[case::try_single(&TRY_SINGLE, OutputName::out(), Some(&Default::default()))]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT, OutputName::from_static("bin").unwrap(), Some(&Default::default()))]
    #[case::multiple_out(&TRY_MULTIPLE, OutputName::out(), None)]
    #[case::multiple_bin(&TRY_MULTIPLE, OutputName::from_static("bin").unwrap(), Some(&Default::default()))]
    #[case::default(&Default::default(), OutputName::out(), Some(&Default::default()))]
    fn get(#[case] value: &Outputs, #[case] name: OutputName, #[case] expected: Option<&Output>) {
        assert_eq!(value.get(&name), expected)
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, OutputName::out())]
    #[case::try_fod(&TRY_FOD, OutputName::out())]
    #[case::try_single(&TRY_SINGLE, OutputName::out())]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT, OutputName::from_static("bin").unwrap())]
    #[case::multiple_bin(&TRY_MULTIPLE, OutputName::from_static("bin").unwrap())]
    #[case::default(&Default::default(), OutputName::out())]
    fn contains_key(#[case] value: &Outputs, #[case] name: OutputName) {
        assert!(value.contains_key(&name))
    }

    #[rstest]
    #[case::multiple_out(&TRY_MULTIPLE, OutputName::out())]
    fn does_not_contain_key(#[case] value: &Outputs, #[case] name: OutputName) {
        assert!(!value.contains_key(&name))
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![(OutputName::out(), FOD_OUTPUT.clone())])]
    #[case::try_fod(&TRY_FOD, vec![(OutputName::out(), FOD_OUTPUT.clone())])]
    #[case::try_single(&TRY_SINGLE, vec![(OutputName::out(), Default::default())])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT, vec![(OutputName::from_static("bin").unwrap(), Default::default())])]
    #[case::multiple(&TRY_MULTIPLE, vec![
        (OutputName::from_static("bin").unwrap(), Default::default()),
        (OutputName::from_static("dev").unwrap(), Default::default()),
    ])]
    #[case::default(&Default::default(), vec![(OutputName::out(), Default::default())])]
    fn iter(#[case] value: &Outputs, #[case] expected: Vec<(OutputName, Output)>) {
        let actual: Vec<_> = value
            .iter()
            .map(|(name, output)| (name.clone(), output.clone()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![OutputName::out()])]
    #[case::try_fod(&TRY_FOD, vec![OutputName::out()])]
    #[case::try_single(&TRY_SINGLE, vec![OutputName::out()])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT, vec![OutputName::from_static("bin").unwrap()])]
    #[case::multiple(&TRY_MULTIPLE, vec![
        OutputName::from_static("bin").unwrap(),
        OutputName::from_static("dev").unwrap(),
    ])]
    #[case::default(&Default::default(), vec![OutputName::out()])]
    fn keys(#[case] value: &Outputs, #[case] expected: Vec<OutputName>) {
        let actual: Vec<_> = value.keys().cloned().collect();
        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case::fod(&FOD_OUTPUTS, vec![FOD_OUTPUT.clone()])]
    #[case::try_fod(&TRY_FOD, vec![FOD_OUTPUT.clone()])]
    #[case::try_single(&TRY_SINGLE, vec![Default::default()])]
    #[case::try_single_non_out(&TRY_SINGLE_NON_OUT, vec![Default::default()])]
    #[case::multiple(&TRY_MULTIPLE, vec![Default::default(), Default::default()])]
    #[case::default(&Default::default(), vec![Default::default()])]
    fn values(#[case] value: &Outputs, #[case] expected: Vec<Output>) {
        let actual: Vec<_> = value.values().cloned().collect();
        assert_eq!(actual, expected);
    }
}
