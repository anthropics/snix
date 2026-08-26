use pretty_assertions::{assert_eq, assert_ne};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix_language_test_suite_common::{
    ErrorKind, TestCase, load_expected_error, load_expected_output, load_test_case,
    matches_short_path, normalize_output, setup_environment,
};
use rstest::rstest;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct SkipConfig {
    nix_2_3: VersionSkip,
    nix_latest: VersionSkip,
    lix_latest: VersionSkip,
}

#[derive(Deserialize, Debug)]
struct VersionSkip {
    #[serde(default)]
    paths: HashSet<String>,

    #[serde(default)]
    builtins: HashSet<String>,

    #[serde(default)]
    features: HashSet<String>,
}

#[derive(Debug)]
enum NixVersion {
    CppNix23,
    CppNixLatest,
    LixLatest,
}

impl SkipConfig {
    fn is_known_failing(
        &self,
        version: &NixVersion,
        test_path: &Path,
        test_case: &TestCase,
    ) -> bool {
        let config = match version {
            NixVersion::CppNix23 => &self.nix_2_3,
            NixVersion::CppNixLatest => &self.nix_latest,
            NixVersion::LixLatest => &self.lix_latest,
        };

        // NIX_SANDBOX is defined in default.nix
        let skip_sandbox =
            is_sandbox() && (test_case.environment.network || test_case.environment.nix_store);

        let skip_builtins = test_case
            .lang
            .builtins
            .iter()
            .any(|x| config.builtins.contains(x));

        let skip_feature = config
            .features
            .iter()
            .filter_map(|s| s.parse::<nix_language_test_suite_common::Feature>().ok())
            .any(|f| test_case.lang.features.contains(&f));

        let skip_path = config
            .paths
            .iter()
            .any(|s| matches_short_path(test_path, s));

        skip_sandbox || skip_builtins || skip_feature || skip_path
    }
}

fn is_sandbox() -> bool {
    std::env::var("NIX_SANDBOX").is_ok()
}

fn nix_version() -> NixVersion {
    let version = std::env::var("NIX_VERSION").expect("NIX_VERSION env should be present");

    if version.starts_with("lix") {
        NixVersion::LixLatest
    } else if version.starts_with("nix-2.3.") {
        NixVersion::CppNix23
    } else {
        NixVersion::CppNixLatest
    }
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
    let nix_version = nix_version();
    let test_case = load_test_case(&test_case_path);
    let known_failing = skip_config.is_known_failing(&nix_version, &test_case_path, &test_case);

    // This branch mostly matches fetchers tests.
    //
    // Because Nix/Lix are less lazier than Snix, they're trying to resolve
    // hostname and only after they can't they fail. That increases the
    // execution time of the suite by multiple seconds. That might not
    // make difference in the CI, but becomes more significant during local development.
    if known_failing && test_case.environment.network {
        eprintln!("SKIP {}", test_case_path.display());
        return;
    }

    let (base_dir, nix_code_path) = setup_environment(&test_case_path, &test_case);

    let mut nixcpp_cmd = {
        let mut cmd = Command::new("nix-instantiate");

        cmd.current_dir(base_dir.path());
        if is_sandbox() {
            // A workaround for `SQLite database '...db.sqlite' is busy`
            cmd.env("NIX_STATE_DIR", base_dir.path().join("state"));
        }

        cmd.arg("--eval");
        cmd.arg(&nix_code_path);

        if test_case.runtime_opts.eval_strict {
            cmd.arg("--strict");
        }

        if matches!(nix_version, NixVersion::LixLatest) {
            cmd.args(["--extra-deprecated-features", "url-literals ancient-let"]);
        }

        if test_case.runtime_opts.xml_output {
            cmd.arg("--no-location");
            cmd.arg("--xml");
        }

        for p in &test_case.runtime_opts.search_path {
            cmd.arg("--include");
            cmd.arg(p);
        }

        for feature in &test_case.lang.features {
            match feature {
                nix_language_test_suite_common::Feature::Flakes => {
                    cmd.arg("--extra-experimental-features");
                    cmd.arg("flakes");
                }
                nix_language_test_suite_common::Feature::PipeOperators => {
                    let name = if matches!(nix_version, NixVersion::LixLatest) {
                        "pipe-operator"
                    } else {
                        "pipe-operators"
                    };
                    cmd.arg("--extra-experimental-features");
                    cmd.arg(name);
                }
                _ => {}
            }
        }

        for env_var in &test_case.environment.env_vars {
            cmd.env(&env_var.name, &env_var.value);
        }

        cmd
    };

    let result = nixcpp_cmd.output().unwrap();
    let failed = !result.status.success();

    if let Some(exp_str) = load_expected_output(&test_case_path) {
        if failed {
            if known_failing {
                return;
            }

            let error_string = str::from_utf8(&result.stderr).unwrap();

            panic!(
                "{}: evaluation of test should succeed, but failed with:\n{}",
                nix_code_path.display(),
                error_string,
            );
        }

        let value = {
            let s = str::from_utf8(&result.stdout).unwrap();
            normalize_output(&test_case, &nix_code_path, s)
        };

        if known_failing {
            assert_ne!(
                value.trim(),
                exp_str.trim(),
                "{}: test passed unexpectedly! consider removing it from skip.toml",
                nix_code_path.display()
            );
            return;
        }

        assert_eq!(
            value.trim(),
            exp_str.trim(),
            "{}: test case failed",
            nix_code_path.display()
        );

        return;
    }

    if let Some(exp_err) = load_expected_error(&test_case_path) {
        let error_string = str::from_utf8(&result.stderr).unwrap();
        let output = str::from_utf8(&result.stdout).unwrap();
        let matches_expected =
            failed && matches_expected_error(nix_version, error_string, &exp_err);

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
                "{}: invalid error kind. Expected {:?}, got stderr:\n{}\nstdout:\n{}",
                test_case_path.display(),
                exp_err,
                error_string,
                output,
            );
        }

        return;
    }

    panic!("neither .exp nor .err file was found");
}

// FUTUREWORK: error kind is not stable yet, perhaps in the future
// having smarter regexp is preferred
fn matches_expected_error(version: NixVersion, error_string: &str, expected: &ErrorKind) -> bool {
    let must_contain = match expected {
        ErrorKind::Abort => &["evaluation aborted"][..],
        ErrorKind::NotCoercibleToString => &["cannot coerce"][..],
        ErrorKind::IO => match version {
            NixVersion::CppNixLatest => &["does not exist", "has an unsupported type"][..],
            _ => &["No such file or directory", "has an unsupported type"][..],
        },
        ErrorKind::TypeError => match version {
            NixVersion::CppNix23 => &["requires a function", "was expected"][..],
            _ => &["requires a function", "expected a"][..],
        },
        ErrorKind::InvalidHash => &["invalid SRI hash"][..],
        ErrorKind::InvalidStorePath => match version {
            NixVersion::CppNixLatest => &["is not a valid store path"][..],
            NixVersion::CppNix23 => &["Path names are alphanumeric"],
            NixVersion::LixLatest => &["store path"][..],
        },
        ErrorKind::HashMismatch => &["store path mismatch"][..],
        ErrorKind::DerivationError => match version {
            NixVersion::LixLatest => &["trace involved the following derivations"][..],
            NixVersion::CppNix23 => &[
                "Path names are alphanumeric",
                "should have type",
                "duplicate derivation output",
            ],
            _ => &[
                "invalid derivation name",
                "should have type",
                "duplicate derivation output",
                "cannot process __json attribute",
            ][..],
        },
        ErrorKind::UnexpectedArgument => &["unsupported argument"][..],
        ErrorKind::VariableAlreadyDefined => &["already defined at"][..],
        ErrorKind::DuplicateAttrsKey => &["already defined at"][..],
        ErrorKind::UnexpectedContext => match version {
            NixVersion::CppNix23 => &["cannot refer to derivation outputs"][..],
            _ => &["files created by builtins.toFile may not reference derivations"][..],
        },
    };

    must_contain.iter().any(|x| error_string.contains(x))
}
