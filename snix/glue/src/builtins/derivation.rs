//! Implements `builtins.derivation`, the core of what makes Nix build packages.
use crate::builtins::DerivationError;
use crate::snix_store_io::SnixStoreIO;
use bstr::BString;
use nix_compat::derivation::{Derivation, OutputHash, OutputName};
use nix_compat::store_path::{StorePath, StorePathRef};
use snix_build_glue::known_paths::KnownPaths;
use snix_eval::builtin_macros::builtins;
use snix_eval::generators::{self, GenCo, emit_warning_kind};
use snix_eval::{
    AddContext, ErrorKind, NixAttrs, NixContext, NixContextElement, Value, WarningKind,
};
use std::collections::{BTreeSet, btree_map};
use std::rc::Rc;

// Constants used for strangely named fields in derivation inputs.
const IGNORE_NULLS: &str = "__ignoreNulls";
pub const STRUCTURED_ATTRS_ENABLE_KEY: &str = "__structuredAttrs";

/// Populate the inputs of a derivation from the build references
/// found when scanning the derivation's parameters and extracting their contexts.
fn populate_inputs(drv: &mut Derivation, full_context: NixContext, known_paths: &KnownPaths) {
    for element in full_context.iter() {
        match element {
            NixContextElement::Plain(source) => {
                let sp = StorePathRef::from_absolute_path(source.as_bytes())
                    .expect("invalid store path")
                    .to_owned();
                drv.input_sources.insert(sp);
            }

            NixContextElement::Single {
                name,
                derivation: derivation_str,
            } => {
                // TODO: b/264
                // We assume derivations to be passed validated, so ignoring rest
                // and expecting parsing is ok.
                let (derivation, _rest) =
                    StorePath::from_absolute_path_full(derivation_str).expect("valid store path");

                #[cfg(debug_assertions)]
                assert!(
                    _rest.iter().next().is_none(),
                    "Extra path not empty for {derivation_str}"
                );

                let name: OutputName = name
                    .parse()
                    .expect("Snix bug: output name in context invalid");

                match drv.input_derivations.entry(derivation.clone()) {
                    btree_map::Entry::Vacant(entry) => {
                        entry.insert(BTreeSet::from([name]));
                    }

                    btree_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().insert(name);
                    }
                }
            }

            NixContextElement::Derivation(drv_path) => {
                let (derivation, _rest) =
                    StorePath::from_absolute_path_full(drv_path).expect("valid store path");

                #[cfg(debug_assertions)]
                assert!(
                    _rest.iter().next().is_none(),
                    "Extra path not empty for {drv_path}"
                );

                // We need to know all the outputs *names* of that derivation.
                let output_names = known_paths
                    .get_drv_by_drvpath(&derivation.as_ref())
                    .expect("no known derivation associated to that derivation path")
                    .outputs
                    .keys();

                // FUTUREWORK(performance): ideally, we should be able to clone
                // cheaply those outputs rather than duplicate them all around.
                match drv.input_derivations.entry(derivation.clone()) {
                    btree_map::Entry::Vacant(entry) => {
                        entry.insert(output_names.cloned().collect());
                    }

                    btree_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().extend(output_names.cloned());
                    }
                }

                drv.input_sources.insert(derivation);
            }
        }
    }
}

#[builtins(state = "Rc<SnixStoreIO>")]
pub(crate) mod derivation_builtins {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bstr::ByteSlice;

    use nix_compat::derivation::{Output, Outputs};
    use nix_compat::nixhash::{HashAlgo, NixHash};
    use nix_compat::store_path::hash_placeholder;
    use snix_build_glue::builder;
    use snix_eval::generators::Gen;
    use snix_eval::{NixContext, NixContextElement, NixString, try_cek_to_value};

    use crate::builtins::utils::{select_string, strong_importing_coerce_to_string};
    use crate::fetchurl::fetchurl_derivation_to_fetch;

    use super::*;

    #[builtin("placeholder")]
    async fn builtin_placeholder(co: GenCo, input: Value) -> Result<Value, ErrorKind> {
        if input.is_catchable() {
            return Ok(input);
        }

        let nix_string = input
            .to_str()
            .context("looking at output name in builtins.placeholder")?;
        let output_name = nix_string.to_str()?;

        let placeholder = hash_placeholder(output_name);

        Ok(placeholder.into())
    }

    /// Strictly construct a Nix derivation from the supplied arguments.
    ///
    /// This is considered an internal function, users usually want to
    /// use the higher-level `builtins.derivation` instead.
    #[builtin("derivationStrict")]
    async fn builtin_derivation_strict(
        state: Rc<SnixStoreIO>,
        co: GenCo,
        input: Value,
    ) -> Result<Value, ErrorKind> {
        if input.is_catchable() {
            return Ok(input);
        }

        let input = input.to_attrs()?;
        let name = generators::request_force(&co, input.select_required("name")?.clone()).await;

        if name.is_catchable() {
            return Ok(name);
        }

        let name = name.to_str().context("determining derivation name")?;
        if name.is_empty() {
            return Err(ErrorKind::Abort("derivation has empty name".to_string()));
        }
        let name = name.to_str()?;

        let mut drv = Derivation::default();

        let mut input_context = NixContext::new();

        /// Inserts a key and value into the drv.environment BTreeMap, and fails if the
        /// key did already exist before.
        fn insert_env(
            drv: &mut Derivation,
            k: &str, /* TODO: non-utf8 env keys */
            v: BString,
        ) -> Result<(), DerivationError> {
            if drv.environment.insert(k.into(), v).is_some() {
                return Err(DerivationError::DuplicateEnvVar(k.into()));
            }
            Ok(())
        }

        // Check whether null attributes should be ignored or passed through.
        let ignore_nulls = match input.select(IGNORE_NULLS) {
            Some(b) => generators::request_force(&co, b.clone()).await.as_bool()?,
            None => false,
        };

        // Peek at the STRUCTURED_ATTRS argument.
        // If it's set and true, provide a BTreeMap that gets populated while looking at the arguments.
        // We need it to be a BTreeMap, so iteration order of keys is reproducible.
        let mut structured_attrs: Option<BTreeMap<&str, serde_json::Value>> =
            match input.select(STRUCTURED_ATTRS_ENABLE_KEY) {
                Some(b) => generators::request_force(&co, b.clone())
                    .await
                    .as_bool()?
                    .then_some(Default::default()),
                None => None,
            };

        // Look at the arguments passed to builtins.derivationStrict.
        // Some set special fields in the Derivation struct, some change
        // behaviour of other functionality.
        for (arg_name, arg_value) in input.iter_sorted() {
            let arg_name = arg_name.to_str()?;
            // force the current value.
            let value = generators::request_force(&co, arg_value.clone()).await;

            // filter out nulls if ignore_nulls is set.
            if ignore_nulls && matches!(value, Value::Null) {
                continue;
            }

            match arg_name {
                // Command line arguments to the builder.
                // These are only set in drv.arguments.
                "args" => {
                    for arg in value.to_list()? {
                        let s =
                            try_cek_to_value!(strong_importing_coerce_to_string(&co, arg).await);
                        input_context.mimic(&s);
                        drv.arguments.push(s.to_str()?.to_owned())
                    }
                }

                // If outputs is set, populate drv.outputs with them.
                "outputs" => {
                    let outputs = value
                        .to_list()
                        .context("looking at the `outputs` parameter of the derivation")?;

                    let mut output_names = Vec::with_capacity(outputs.len());

                    for output in outputs {
                        let output_name = generators::request_force(&co, output)
                            .await
                            .to_str()
                            .context("determining output name")?;

                        input_context.mimic(&output_name);

                        let output_name: OutputName = output_name
                            .to_str()?
                            .parse()
                            .map_err(|err| ErrorKind::SnixError(Arc::new(err)))?;

                        output_names.push(output_name);
                    }
                    drv.outputs = Outputs::try_from_iter(
                        output_names
                            .iter()
                            .cloned()
                            .map(|name| (name, Output::default())),
                    )
                    .map_err(nix_compat::derivation::DerivationError::from)
                    .map_err(DerivationError::from)?;

                    match structured_attrs.as_mut() {
                        // add outputs to the json itself (as a list of strings)
                        Some(structured_attrs) => {
                            let names = output_names.iter().map(ToString::to_string).collect();
                            structured_attrs.insert(arg_name, names);
                        }
                        // add drv.environment["outputs"] as a space-separated list
                        None => {
                            insert_env(&mut drv, arg_name, output_names.join(" ").into())?;
                        }
                    }
                    // drv.environment[$output_name] is added after the loop,
                    // with whatever is in drv.outputs[$output_name].
                }

                // handle builder and system.
                "builder" | "system" => {
                    let val_str =
                        try_cek_to_value!(strong_importing_coerce_to_string(&co, value).await);
                    input_context.mimic(&val_str);

                    if arg_name == "builder" {
                        val_str.to_str()?.clone_into(&mut drv.builder);
                    } else {
                        val_str.to_str()?.clone_into(&mut drv.system);
                    }

                    // Either populate drv.environment or structured_attrs.
                    if let Some(ref mut structured_attrs) = structured_attrs {
                        // No need to check for dups, we only iterate over every attribute name once
                        structured_attrs.insert(arg_name, val_str.to_str()?.to_owned().into());
                    } else {
                        insert_env(&mut drv, arg_name, val_str.as_bytes().into())?;
                    }
                }

                // Don't add `STRUCTURED_ATTRS_ENABLE_KEY`.
                STRUCTURED_ATTRS_ENABLE_KEY if structured_attrs.is_some() => continue,

                // IGNORE_NULLS is always skipped, even if it's not set to true.
                IGNORE_NULLS => continue,

                // all other args.
                _ => {
                    if arg_name == builder::structured_attrs::JSON_KEY {
                        return Err(DerivationError::StructuredAttrsJsonKeyPresent.into());
                    }

                    match structured_attrs {
                        // In SA case, force and add to structured attrs.
                        Some(ref mut structured_attrs) => {
                            let val = generators::request_force(&co, value).await;
                            if val.is_catchable() {
                                return Ok(val);
                            }

                            let (val_json, context) = val.into_contextful_json(&co).await?;
                            input_context.extend(context);

                            // No need to check for dups, we only iterate over every attribute name once
                            structured_attrs.insert(arg_name, val_json);
                        }
                        // In non-SA case, coerce to string and add to env.
                        None => {
                            let val_str = try_cek_to_value!(
                                strong_importing_coerce_to_string(&co, value).await
                            );
                            input_context.mimic(&val_str);

                            insert_env(&mut drv, arg_name, val_str.as_bytes().into())?;
                        }
                    }
                }
            }
        }
        // end of per-argument loop

        // Set the out output, dealing with FOD fields if required.
        {
            // Unset or set but empty string are treated the same.
            let hash_str = try_cek_to_value!(
                select_string(&co, &input, "outputHash")
                    .await
                    .context("evaluating the `outputHash` parameter")?
            )
            .filter(|s| !s.is_empty());

            let hash_algo = try_cek_to_value!(
                select_string(&co, &input, "outputHashAlgo")
                    .await
                    .context("evaluating the `outputHashAlgo` parameter")?
            )
            .filter(|s| !s.is_empty());

            let hash_mode = try_cek_to_value!(
                select_string(&co, &input, "outputHashMode")
                    .await
                    .context("evaluating the `outputHashMode` parameter")?
            )
            .filter(|s| !s.is_empty());

            // FOD case.
            if let Some(hash_str) = hash_str {
                if !drv.outputs.is_single() {
                    return Err(ErrorKind::SnixError(Arc::new(
                        DerivationError::ConflictingOutputTypes,
                    )));
                }

                // parse outputHashMode.
                let mode = hash_mode
                    .map(|s| s.parse().map_err(|err| ErrorKind::SnixError(Arc::new(err))))
                    .transpose()?
                    .unwrap_or_default();

                // parse outputHashAlgo
                let want_algo: Option<HashAlgo> = hash_algo
                    .map(|s| s.parse())
                    .transpose()
                    .map_err(|err| ErrorKind::SnixError(Arc::new(err)))?;

                // construct a NixHash
                let hash = NixHash::from_str(&hash_str, want_algo)
                    .map_err(|err| ErrorKind::SnixError(Arc::new(err)))?;

                // Emit a warning if the hash was SRI, but with wrong padding.
                if let Some(rest) = hash_str.strip_prefix(hash.algo().sri_prefix())
                    && data_encoding::BASE64.encode_len(hash.algo().digest_length()) != rest.len()
                {
                    emit_warning_kind(&co, WarningKind::SRIHashWrongPadding).await;
                }
                drv.outputs = Outputs::from_fod_hash(OutputHash { mode, hash });
            }
        }

        // Each output name needs to exist in the environment, at this
        // point initialised as an empty string, as the ATerm serialization of that is later
        // used for the output path calculation (which will also update output
        // paths post-calculation, both in drv.environment and drv.outputs)
        for output in drv.outputs.keys() {
            if drv
                .environment
                .insert(output.to_string(), String::new().into())
                .is_some()
            {
                emit_warning_kind(&co, WarningKind::ShadowedOutput(output.to_string())).await;
            }
        }

        if let Some(structured_attrs) = structured_attrs {
            // configure __json
            drv.environment.insert(
                builder::structured_attrs::JSON_KEY.to_string(),
                BString::from(serde_json::to_string(&structured_attrs)?),
            );
        }

        let mut known_paths = state.as_ref().build_state.known_paths.borrow_mut();
        populate_inputs(&mut drv, input_context, &known_paths);

        // At this point, derivation fields are fully populated from
        // eval data structures.
        drv.validate().map_err(DerivationError::InvalidDerivation)?;

        // Calculate the hash_derivation_modulo for the current derivation..
        debug_assert!(
            drv.outputs.values().all(|output| { output.path.is_none() }),
            "outputs should still be unset"
        );

        // Mutate the Derivation struct and set output paths
        drv.calculate_output_paths(
            name,
            // This one is still intermediate (so not added to known_paths),
            // as the outputs are still unset.
            &drv.hash_derivation_modulo(|drv_path| {
                *known_paths
                    .get_hash_derivation_modulo(drv_path)
                    .unwrap_or_else(|| panic!("{drv_path} not found"))
            }),
        )
        .map_err(DerivationError::InvalidDerivation)?;

        let drv_path = drv
            .calculate_derivation_path(name)
            .map_err(DerivationError::InvalidDerivation)?;

        // Assemble the attrset to return from this builtin.
        let out = Value::Attrs(NixAttrs::from_iter(
            drv.outputs
                .iter()
                .map(|(name, output)| {
                    (
                        name.into(),
                        NixString::new_context_from(
                            NixContextElement::Single {
                                name: name.into(),
                                derivation: drv_path.to_absolute_path(),
                            }
                            .into(),
                            output.path.as_ref().unwrap().to_absolute_path(),
                        ),
                    )
                })
                .chain(std::iter::once((
                    "drvPath".to_owned(),
                    NixString::new_context_from(
                        NixContextElement::Derivation(drv_path.to_absolute_path()).into(),
                        drv_path.to_absolute_path(),
                    ),
                ))),
        ));

        // If the derivation is a fake derivation (builtin:fetchurl),
        // synthesize a [Fetch] and add it there, too.
        if drv.builder == "builtin:fetchurl" {
            let (name, fetch) = fetchurl_derivation_to_fetch(&drv)
                .map_err(|e| ErrorKind::SnixError(Arc::from(e)))?;

            known_paths
                .add_fetch(fetch, &name)
                .map_err(|e| ErrorKind::SnixError(Arc::from(e)))?;
        }

        // Register the Derivation in known_paths.
        known_paths.add_derivation(drv_path, drv);

        Ok(out)
    }
}
