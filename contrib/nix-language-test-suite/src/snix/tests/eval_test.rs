use pretty_assertions::{assert_eq, assert_ne};
use snix_eval::builtins::impure_builtins;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use rstest::rstest;

use clap::Parser as _;
use nix_language_test_suite_common::{
    ErrorKind, TestCase, load_expected_error, load_expected_output, load_test_case,
    matches_short_path, normalize_output, setup_environment,
};
use serde::Deserialize;
use snix_build::buildservice::DummyBuildService;
use snix_eval::{EvalIO, EvalMode, Evaluation, Value};
use snix_glue::builtins::ImportError;
use snix_glue::{
    builtins::{add_derivation_builtins, add_fetcher_builtins, add_import_builtins},
    configure_nix_path,
    snix_io::SnixIO,
    snix_store_io::SnixStoreIO,
};
use snix_store::utils::{ServiceUrlsMemory, construct_services};

#[derive(Deserialize, Debug)]
struct SkipConfig {
    paths: HashSet<String>,
    builtins: HashSet<String>,
    features: HashSet<String>,
}

impl SkipConfig {
    fn is_known_failing(&self, test_path: &Path, test_case: &TestCase) -> bool {
        let skip_feature = self
            .features
            .iter()
            .filter_map(|s| s.parse::<nix_language_test_suite_common::Feature>().ok())
            .any(|f| test_case.lang.features.contains(&f));

        let skip_builtins = test_case
            .lang
            .builtins
            .iter()
            .any(|x| self.builtins.contains(x));

        let skip_path = self.paths.iter().any(|s| matches_short_path(test_path, s));

        skip_feature || skip_path || skip_builtins
    }
}

fn build_eval(
    test_case: &TestCase,
) -> (
    Evaluation<'static, 'static, 'static, Box<dyn EvalIO>>,
    tokio::runtime::Runtime,
) {
    let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
    let (blob_service, directory_service, path_info_service, nar_calculation_service) =
        tokio_runtime
            .block_on(async {
                construct_services(ServiceUrlsMemory::parse_from(std::iter::empty::<&str>())).await
            })
            .unwrap();

    let snix_store_io = Rc::new(SnixStoreIO::new(
        blob_service,
        directory_service,
        path_info_service,
        nar_calculation_service,
        Arc::new(DummyBuildService::default()),
        tokio_runtime.handle().clone(),
        Vec::new(),
    ));

    let eval_mode = if test_case.runtime_opts.eval_strict {
        EvalMode::Strict
    } else {
        EvalMode::Lazy
    };

    let mut eval_builder = Evaluation::builder(Box::new(SnixIO::new(
        snix_store_io.clone() as Rc<dyn EvalIO>
    )) as Box<dyn EvalIO>)
    .mode(eval_mode)
    .enable_import();

    eval_builder = add_derivation_builtins(eval_builder, Rc::clone(&snix_store_io));
    eval_builder = add_fetcher_builtins(eval_builder, Rc::clone(&snix_store_io));
    eval_builder = add_import_builtins(eval_builder, snix_store_io);
    eval_builder = eval_builder.add_builtins(impure_builtins());
    eval_builder = configure_nix_path(eval_builder, &None);

    (eval_builder.build(), tokio_runtime)
}

#[rstest::fixture]
#[once]
fn skip_config() -> SkipConfig {
    let s = std::fs::read_to_string("skip.toml").unwrap();
    toml::from_str::<SkipConfig>(&s).unwrap()
}

#[rstest]
fn eval_test(
    #[base_dir = "${TEST_SUITE_DIR:-../../tests}"]
    #[files("cases/**/*.kdl")]
    test_case_path: PathBuf,
    skip_config: &SkipConfig,
) {
    let test_case = load_test_case(&test_case_path);
    let known_failing = skip_config.is_known_failing(&test_case_path, &test_case);

    let (_tmp_dir, code_path) = setup_environment(&test_case_path, &test_case);
    let code = std::fs::read_to_string(&code_path).unwrap();

    let (eval, _tokio_runtime) = build_eval(&test_case);
    let result = eval.evaluate(&code, Some(code_path.clone()));
    let failed = match result.value {
        Some(Value::Catchable(_)) => true,
        _ => !result.errors.is_empty(),
    };

    if let Some(exp_str) = load_expected_output(&test_case_path) {
        if failed {
            if known_failing {
                return;
            }

            let error_string = result
                .errors
                .iter()
                .map(|error| error.fancy_format_str())
                .collect::<Vec<String>>()
                .join("\n");

            panic!(
                "{}: evaluation of test should succeed, but failed with:\n{}",
                code_path.display(),
                error_string,
            );
        }

        let value = result.value.unwrap();

        if test_case.runtime_opts.xml_output {
            let mut xml_actual_buf = Vec::new();
            snix_eval::builtins::value_to_xml(&mut xml_actual_buf, &value).unwrap();
            let actual_xml =
                String::from_utf8(xml_actual_buf).expect("to_xml produced invalid utf-8");

            if known_failing {
                assert_ne!(
                    actual_xml,
                    exp_str,
                    "{}: test passed unexpectedly! consider removing it from skip.toml",
                    code_path.display()
                );
                return;
            }

            assert_eq!(
                actual_xml,
                exp_str,
                "{}: result value representation (left) must match expectation (right)",
                code_path.display()
            );
            return;
        }

        let result_str = normalize_output(&test_case, &code_path, &value.to_string());

        if known_failing {
            assert_ne!(
                result_str,
                exp_str.trim(),
                "{}: test passed unexpectedly! consider removing it from skip.toml",
                code_path.display()
            );
            return;
        }

        assert_eq!(
            result_str,
            exp_str.trim(),
            "{}: result value representation (left) must match expectation (right)",
            code_path.display()
        );

        return;
    }

    if let Some(exp_error) = load_expected_error(&test_case_path) {
        let matches_expected =
            failed && !result.errors.is_empty() && matches_expected_error(&result, &exp_error);

        if known_failing {
            assert!(
                !matches_expected,
                "{}: test passed unexpectedly! consider removing it from skip.toml",
                test_case_path.display(),
            );
            return;
        }

        if !matches_expected {
            panic!(
                "{}: invalid error kind. Expected {:?}, got errors: {:?}, value: {:?}",
                test_case_path.display(),
                exp_error,
                result.errors,
                result.value
            );
        }

        return;
    }

    panic!("neither .exp nor .err file was found");
}

fn matches_expected_error(result: &snix_eval::EvaluationResult, exp_kind: &ErrorKind) -> bool {
    let snix_kind = innermost_error_kind(
        result
            .errors
            .first()
            .expect("at least one error is expected"),
    );

    match exp_kind {
        ErrorKind::Abort => matches!(snix_kind, snix_eval::ErrorKind::Abort(_)),
        ErrorKind::AssertionFailed => matches!(
            snix_kind,
            snix_eval::ErrorKind::CatchableError(snix_eval::CatchableErrorKind::AssertionFailed)
        ),
        ErrorKind::IO => matches!(snix_kind, snix_eval::ErrorKind::IO { .. }),
        ErrorKind::NotCoercibleToString => {
            matches!(snix_kind, snix_eval::ErrorKind::NotCoercibleToString { .. })
        }
        ErrorKind::TypeError => matches!(snix_kind, snix_eval::ErrorKind::TypeError { .. }),
        ErrorKind::HashMismatch => {
            let snix_eval::ErrorKind::SnixError(err) = snix_kind else {
                return false;
            };
            matches!(
                err.downcast_ref::<ImportError>(),
                Some(ImportError::HashMismatch(..))
            )
        }
        ErrorKind::InvalidHash => {
            let snix_eval::ErrorKind::SnixError(err) = snix_kind else {
                return false;
            };
            matches!(
                err.downcast_ref::<nix_compat::nixhash::Error>(),
                Some(nix_compat::nixhash::Error::InvalidDigestLength(_))
            )
        }
        ErrorKind::InvalidStorePath => {
            let snix_eval::ErrorKind::SnixError(err) = snix_kind else {
                return false;
            };
            err.downcast_ref::<nix_compat::store_path::ParseStorePathError>()
                .is_some()
        }
        ErrorKind::DerivationError => match snix_kind {
            snix_eval::ErrorKind::Abort(err) => err.contains("derivation has empty name"),
            snix_eval::ErrorKind::SnixError(err) => {
                err.downcast_ref::<snix_glue::builtins::DerivationError>()
                    .is_some()
                    || matches!(
                        err.downcast_ref::<nix_compat::nixhash::Error>(),
                        Some(nix_compat::nixhash::Error::ConflictingHashAlgos(_, _))
                    )
            }
            _ => false,
        },
        ErrorKind::UnexpectedArgument => match snix_kind {
            snix_eval::ErrorKind::UnexpectedArgumentBuiltin(_)
            | snix_eval::ErrorKind::UnexpectedArgumentFormals { .. } => true,
            _ => false,
        },
        ErrorKind::VariableAlreadyDefined => {
            matches!(snix_kind, snix_eval::ErrorKind::VariableAlreadyDefined(_))
        }
        ErrorKind::DuplicateAttrsKey => {
            matches!(snix_kind, snix_eval::ErrorKind::DuplicateAttrsKey { .. })
        }
        ErrorKind::UnexpectedContext => {
            matches!(snix_kind, snix_eval::ErrorKind::UnexpectedContext)
        }
        ErrorKind::InfiniteRecursion => {
            matches!(snix_kind, snix_eval::ErrorKind::InfiniteRecursion { .. })
        }
    }
}

fn innermost_error_kind(err: &snix_eval::Error) -> &snix_eval::ErrorKind {
    match &err.kind {
        snix_eval::ErrorKind::NativeError { err, .. }
        | snix_eval::ErrorKind::BytecodeError(err) => innermost_error_kind(err),
        kind => kind,
    }
}
