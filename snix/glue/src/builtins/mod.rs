//! Contains builtins that deal with the store or builder.

use std::rc::Rc;

use crate::snix_store_io::SnixStoreIO;

mod derivation;
mod errors;
mod fetchers;
mod import;
mod utils;

pub use errors::{DerivationError, ImportError};

/// Adds derivation-related builtins to the passed [snix_eval::EvaluationBuilder]:
///
/// * `derivation`
/// * `derivationStrict`
/// * `toFile`
///
/// As they need to interact with `known_paths`, we also need to pass in
/// `known_paths`.
pub fn add_derivation_builtins<'co, 'ro, 'env, IO>(
    eval_builder: snix_eval::EvaluationBuilder<'co, 'ro, 'env, IO>,
    io: Rc<SnixStoreIO>,
) -> snix_eval::EvaluationBuilder<'co, 'ro, 'env, IO> {
    eval_builder
        .add_builtins(derivation::derivation_builtins::builtins(Rc::clone(&io)))
        // Add the actual `builtins.derivation` from compiled Nix code
        .add_src_builtin("derivation", include_str!("derivation.nix"))
}

/// Adds fetcher builtins to the passed [snix_eval::EvaluationBuilder]:
///
/// * `fetchurl`
/// * `fetchTarball`
/// * `fetchGit`
pub fn add_fetcher_builtins<'co, 'ro, 'env, IO>(
    eval_builder: snix_eval::EvaluationBuilder<'co, 'ro, 'env, IO>,
    io: Rc<SnixStoreIO>,
) -> snix_eval::EvaluationBuilder<'co, 'ro, 'env, IO> {
    eval_builder.add_builtins(fetchers::fetcher_builtins::builtins(Rc::clone(&io)))
}

/// Adds import-related builtins to the passed [snix_eval::EvaluationBuilder]:
///
///
/// * `filterSource`
/// * `path`
/// * `storePath`
///
/// As they need to interact with the store implementation, we pass [`SnixStoreIO`].
/// Due to #176, some IO still sidesteps `EvalIO` and accesses the filesystem directly.
pub fn add_import_builtins<'co, 'ro, 'env, IO>(
    eval_builder: snix_eval::EvaluationBuilder<'co, 'ro, 'env, IO>,
    io: Rc<SnixStoreIO>,
) -> snix_eval::EvaluationBuilder<'co, 'ro, 'env, IO> {
    eval_builder.add_builtins(import::import_builtins(io))
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::Arc};

    use crate::snix_store_io::SnixStoreIO;

    use super::{add_derivation_builtins, add_fetcher_builtins, add_import_builtins};
    use clap::Parser;
    use nix_compat::store_path::hash_placeholder;
    use rstest::rstest;
    use snix_build::buildservice::DummyBuildService;
    use snix_castore::blob_engine::BlobServiceEngine;
    use snix_eval::{EvalIO, EvaluationResult};
    use snix_store::utils::{ServiceUrlsMemory, construct_services};

    /// evaluates a given nix expression and returns the result.
    /// Takes care of setting up the evaluator so it knows about the
    // `derivation` builtin.
    fn eval(str: &str) -> EvaluationResult {
        // We assemble a complete store in memory.
        let runtime = tokio::runtime::Runtime::new().expect("Failed to build a Tokio runtime");
        let (blob_service, directory_service, path_info_service, nar_calculation_service) = runtime
            .block_on(async {
                construct_services(ServiceUrlsMemory::parse_from(std::iter::empty::<&str>())).await
            })
            .expect("Failed to construct store services in memory");

        let io = Rc::new(SnixStoreIO::new(
            Arc::new(BlobServiceEngine(blob_service)),
            directory_service,
            path_info_service,
            nar_calculation_service,
            Arc::<DummyBuildService>::default(),
            runtime.handle().clone(),
            Vec::new(),
        ));

        let mut eval_builder = snix_eval::Evaluation::builder(io.clone() as Rc<dyn EvalIO>);
        eval_builder = add_derivation_builtins(eval_builder, Rc::clone(&io));
        eval_builder = add_fetcher_builtins(eval_builder, Rc::clone(&io));
        eval_builder = add_import_builtins(eval_builder, io);
        let eval = eval_builder.build();

        // run the evaluation itself.
        eval.evaluate(str, None)
    }

    #[test]
    fn builtins_placeholder_hashes() {
        assert_eq!(
            hash_placeholder("out").as_str(),
            "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"
        );

        assert_eq!(
            hash_placeholder("").as_str(),
            "/171rf4jhx57xqz3p7swniwkig249cif71pa08p80mgaf0mqz5bmr"
        );
    }

    /// constructs calls to builtins.derivation that should succeed, but produce warnings
    #[rstest]
    #[case::r_sha256_wrong_padding(r#"(builtins.derivation { name = "foo"; builder = "/bin/sh"; system = "x86_64-linux"; outputHashMode = "recursive"; outputHashAlgo = "sha256"; outputHash = "sha256-fgIr3TyFGDAXP5+qoAaiMKDg/a1MlT6Fv/S/DaA24S8===="; }).outPath"#, "/nix/store/xm1l9dx4zgycv9qdhcqqvji1z88z534b-foo")]
    fn builtins_derivation_hash_wrong_padding_warn(
        #[case] code: &str,
        #[case] expected_path: &str,
    ) {
        let eval_result = eval(code);

        let value = eval_result.value.expect("must succeed");

        match value {
            snix_eval::Value::String(s) => {
                assert_eq!(*s, expected_path);
            }
            _ => panic!("unexpected value type: {value:?}"),
        }

        assert!(
            !eval_result.warnings.is_empty(),
            "warnings should not be empty"
        );
    }
}
