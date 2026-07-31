//! This module constructs a [UnverifiedDerivation] by parsing its [ATerm][]
//! serialization.
//!
//! [ATerm]: http://program-transformation.org/Tools/ATermFormat.html

use nom::Parser;
use nom::bytes::streaming::tag;
use nom::character::streaming::char as nomchar;
use nom::combinator::{all_consuming, consumed, map_res};
use nom::multi::{separated_list0, separated_list1};
use nom::sequence::{delimited, preceded, separated_pair, terminated};
use std::collections::{BTreeMap, BTreeSet, btree_map};
use thiserror;

use crate::derivation::parse_error::{ErrorKind, NomError, NomResult, into_nomerror};
use crate::derivation::{
    DerivationBuilder, OutputHash, OutputName, Outputs, UnverifiedDerivation,
    UnverifiedOutputsBuilder, write,
};
use crate::store_path::{self, StorePath};
use crate::{aterm, nixhash};

#[derive(Debug, thiserror::Error)]
/// The error type for derivation parsing errors
#[allow(missing_docs)]
pub enum Error<I> {
    #[error("parsing error: {0}")]
    Parser(#[from] NomError<I>),
    #[error("premature EOF")]
    Incomplete,
    #[error("validation error: {0}")]
    Validation(super::DerivationError),
}

/// Convenience conversion of borrowed Error to an owned counterpart.
impl From<Error<&[u8]>> for Error<Vec<u8>> {
    fn from(value: Error<&[u8]>) -> Self {
        match value {
            Error::Parser(nom_error) => Error::Parser(NomError {
                input: nom_error.input.to_vec(),
                code: nom_error.code,
            }),
            Error::Incomplete => Error::Incomplete,
            Error::Validation(e) => Error::Validation(e),
        }
    }
}

pub fn parse(i: &[u8]) -> Result<UnverifiedDerivation, Error<&[u8]>> {
    match all_consuming(parse_derivation).parse(i) {
        Ok((rest, (builder, outputs))) => {
            // this shouldn't happen, as all_consuming shouldn't return.
            debug_assert!(rest.is_empty());

            // invoke validate
            builder.build_unverified(outputs).map_err(Error::Validation)
        }
        Err(nom::Err::Incomplete(_)) => Err(Error::Incomplete),
        Err(nom::Err::Error(e) | nom::Err::Failure(e)) => Err(e.into()),
    }
}

/// This parses a derivation in streaming fashion.
/// If the parse is successful, it returns the leftover bytes which were not used for the parsing.
/// If the parse is unsuccessful, either it returns incomplete or an error with the input as
/// leftover.
#[allow(dead_code)]
pub fn parse_streaming(i: &[u8]) -> (Result<UnverifiedDerivation, Error<&[u8]>>, &[u8]) {
    match consumed(parse_derivation).parse(i) {
        Ok((_, (rest, (builder, outputs)))) => {
            // invoke validate
            match builder.build_unverified(outputs) {
                Ok(derivation) => (Ok(derivation), rest),
                Err(e) => (Err(Error::Validation(e)), i),
            }
        }
        Err(nom::Err::Incomplete(_)) => (Err(Error::Incomplete), i),
        Err(nom::Err::Error(e) | nom::Err::Failure(e)) => (Err(e.into()), i),
    }
}

/// Parse one output in ATerm. This is 4 string fields inside parans:
/// output name, output path, algo (and mode), digest.
/// Returns the [`OutputName`], output [`StorePath`] and possible FOD [`OutputHash`].
fn parse_output(i: &[u8]) -> NomResult<&[u8], (OutputName, StorePath, Option<OutputHash>)> {
    delimited(
        nomchar('('),
        map_res(
            |i| {
                (
                    terminated(aterm::parse_string_field, nomchar(',')),
                    terminated(aterm::parse_string_field, nomchar(',')),
                    terminated(aterm::parse_string_field, nomchar(',')),
                    aterm::parse_bytes_field,
                )
                    .parse(i)
                    .map_err(into_nomerror)
            },
            |(output_name_str, output_path_str, algo_and_mode, encoded_digest)| -> Result<_, nom::Err<NomError<&[u8]>>> {
                let output_name: OutputName = output_name_str.parse().map_err(|err| {
                    nom::Err::Failure(NomError {
                        input: i,
                        code: ErrorKind::InvalidOutputName(err),
                    })
                })?;

                // This can't be an empty string in ATerms written to disk.
                // This being an empty string can only occur during output path calculation.
                let path = string_to_store_path(i, &output_path_str)?;

                let output_hash = if algo_and_mode.is_empty() && encoded_digest.is_empty() {
                    None
                } else {
                    let digest =
                        data_encoding::HEXLOWER
                            .decode(&encoded_digest)
                            .map_err(|err| {
                                nom::Err::Failure(NomError {
                                    input: i,
                                    code: ErrorKind::NixHashError(
                                        // TODO: do we still need the outer error?
                                        nixhash::Error::InvalidBase16Encoding(err),
                                    ),
                                })
                            })?;

                    Some(
                        OutputHash::from_mode_algo_and_digest(&algo_and_mode, digest)
                            .map_err(|err| {
                                nom::Err::Failure(NomError {
                                    input: i,
                                    code: ErrorKind::NixHashError(err),
                                })
                            })?,
                    )
                };
                Ok((output_name, path, output_hash))
            },
        ),
        nomchar(')'),
    )
    .parse(i)
}

/// Parse multiple outputs in ATerm. This is a list of things acccepted by
/// parse_output, and takes care of turning the (String, Output) returned from
/// it to a BTreeMap.
/// We don't use parse_kv here, as it's dealing with 2-tuples, and these are
/// 4-tuples.
fn parse_outputs(i: &[u8]) -> NomResult<&[u8], Outputs> {
    let res = delimited(
        nomchar('['),
        separated_list1(tag(","), parse_output),
        nomchar(']'),
    )
    .parse(i);

    match res {
        Ok((rst, outputs_lst)) => {
            let mut builder = UnverifiedOutputsBuilder::default();
            for (output_name, store_path, output_hash) in outputs_lst {
                builder
                    .try_insert(output_name, store_path, output_hash)
                    .map_err(|err| {
                        nom::Err::Failure(NomError {
                            input: i,
                            code: ErrorKind::InvalidOutputs(err),
                        })
                    })?;
            }
            let outputs = builder.try_build().map_err(|err| {
                nom::Err::Failure(NomError {
                    input: i,
                    code: ErrorKind::InvalidOutputs(err),
                })
            })?;
            Ok((rst, outputs))
        }
        // pass regular parse errors along
        Err(e) => Err(e),
    }
}

fn parse_input_derivations(
    i: &[u8],
) -> NomResult<&[u8], BTreeMap<StorePath, BTreeSet<OutputName>>> {
    let (i, input_derivations_list) = parse_kv(aterm::parse_string_list)(i)?;

    // This is a HashMap of drv paths to a list of output names.
    let mut input_derivations: BTreeMap<StorePath, BTreeSet<_>> = BTreeMap::new();

    for (input_derivation, output_names_strings) in input_derivations_list {
        let mut output_names = BTreeSet::<OutputName>::new();
        for output_name_string in output_names_strings.into_iter() {
            let output_name = OutputName::try_from(output_name_string).map_err(|err| {
                nom::Err::Failure(NomError {
                    input: i,
                    code: ErrorKind::InvalidOutputName(err),
                })
            })?;

            if output_names.contains(&output_name) {
                return Err(nom::Err::Failure(NomError {
                    input: i,
                    code: ErrorKind::DuplicateInputDerivationOutputName(
                        output_name,
                        input_derivation,
                    ),
                }));
            }
            output_names.insert(output_name);
        }

        let input_derivation = string_to_store_path(i, input_derivation.as_str())?;

        input_derivations.insert(input_derivation, output_names);
    }

    Ok((i, input_derivations))
}

fn parse_input_sources(i: &[u8]) -> NomResult<&[u8], BTreeSet<StorePath>> {
    let (i, input_sources_lst) = aterm::parse_string_list(i).map_err(into_nomerror)?;

    let mut input_sources: BTreeSet<_> = BTreeSet::new();
    for input_source in input_sources_lst.into_iter() {
        let input_source = string_to_store_path(i, input_source.as_str())?;
        if input_sources.contains(&input_source) {
            return Err(nom::Err::Failure(NomError {
                input: i,
                code: ErrorKind::DuplicateInputSource(input_source.to_owned()),
            }));
        } else {
            input_sources.insert(input_source);
        }
    }

    Ok((i, input_sources))
}

fn string_to_store_path<'i>(
    i: &'i [u8],
    path_str: &str,
) -> Result<StorePath, nom::Err<NomError<&'i [u8]>>> {
    let path = StorePath::from_absolute_path(path_str.as_bytes()).map_err(
        |e: store_path::ParseStorePathError| {
            nom::Err::Failure(NomError {
                input: i,
                code: e.into(),
            })
        },
    )?;

    #[cfg(debug_assertions)]
    assert_eq!(path_str, path.to_absolute_path());

    Ok(path)
}

pub fn parse_derivation(i: &[u8]) -> NomResult<&[u8], (DerivationBuilder, Outputs)> {
    use nom::Parser;
    preceded(
        tag(write::DERIVATION_PREFIX),
        delimited(
            // inside parens
            nomchar('('),
            // tuple requires all errors to be of the same type, so we need to be a
            // bit verbose here wrapping generic IResult into [NomATermResult].
            (
                // parse outputs
                terminated(parse_outputs, nomchar(',')),
                // // parse input derivations
                terminated(parse_input_derivations, nomchar(',')),
                // // parse input sources
                terminated(parse_input_sources, nomchar(',')),
                // // parse system
                |i| {
                    terminated(aterm::parse_string_field, nomchar(','))
                        .parse(i)
                        .map_err(into_nomerror)
                },
                // // parse builder
                |i| {
                    terminated(aterm::parse_string_field, nomchar(','))
                        .parse(i)
                        .map_err(into_nomerror)
                },
                // // parse arguments
                |i| {
                    terminated(aterm::parse_string_list, nomchar(','))
                        .parse(i)
                        .map_err(into_nomerror)
                },
                // parse environment
                parse_kv(aterm::parse_bytes_field),
            ),
            nomchar(')'),
        )
        .map(
            |(
                outputs,
                input_derivations,
                input_sources,
                system,
                command,
                arguments,
                environment,
            )| {
                let outputs_builder = outputs.clone().into_builder();
                let builder = DerivationBuilder {
                    arguments,
                    command,
                    environment,
                    input_derivations,
                    input_sources,
                    outputs: outputs_builder,
                    system,
                };
                (builder, outputs)
            },
        ),
    )
    .parse(i)
}

/// Parse a list of key/value pairs into a BTreeMap.
/// The parser for the values can be passed in.
/// In terms of ATerm, this is just a 2-tuple,
/// but we have the additional restriction that the first element needs to be
/// unique across all tuples.
pub(crate) fn parse_kv<'a, V, VF>(
    vf: VF,
) -> impl FnMut(&'a [u8]) -> NomResult<&'a [u8], BTreeMap<String, V>> + 'static
where
    VF: FnMut(&'a [u8]) -> nom::IResult<&'a [u8], V, nom::error::Error<&'a [u8]>> + Clone + 'static,
{
    move |i|
    // inside brackets
    delimited(
        nomchar('['),
        |ii| {
            let res = separated_list0(
                nomchar(','),
                // inside parens
                delimited(
                    nomchar('('),
                    separated_pair(
                        aterm::parse_string_field,
                        nomchar(','),
                        vf.clone(),
                    ),
                    nomchar(')'),
                ),
            ).parse(ii).map_err(into_nomerror);

            match res {
                Ok((rest, pairs)) => {
                    let mut kvs: BTreeMap<String, V> = BTreeMap::new();
                    for (k, v) in pairs.into_iter() {
                        // collect the 2-tuple to a BTreeMap,
                        // and fail if the key was already seen before.
                        match kvs.entry(k) {
                            btree_map::Entry::Vacant(e) => { e.insert(v); },
                            btree_map::Entry::Occupied(e) => {
                                return Err(nom::Err::Failure(NomError {
                                    input: i,
                                    code: ErrorKind::DuplicateMapKey(e.key().clone()),
                                }));
                            }
                        }
                    }
                    Ok((rest, kvs))
                }
                Err(e) => Err(e),
            }
        },
        nomchar(']'),
    ).parse(i)
}

#[cfg(test)]
mod tests {
    use super::OutputHash;
    use crate::derivation::{OutputHashMode, OutputName, Outputs};
    use crate::store_path::StorePathRef;
    use crate::{
        derivation::{NixHash, parse_error::ErrorKind},
        store_path::StorePath,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::LazyLock;

    use bstr::{BString, ByteSlice};
    use hex_literal::hex;
    use rstest::rstest;

    static EXP_MULTI_OUTPUTS: LazyLock<Outputs> = LazyLock::new(|| {
        Outputs::input_addressed_from_iter([
            (
                OutputName::from_static("lib").unwrap(),
                StorePath::from_bytes(b"2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib")
                    .unwrap(),
            ),
            (
                OutputName::out(),
                StorePath::from_bytes(b"55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out".as_bytes())
                    .unwrap(),
            ),
        ])
        .expect("valid outputs")
    });

    static EXP_AB_MAP: LazyLock<BTreeMap<String, BString>> = LazyLock::new(|| {
        let mut b = BTreeMap::new();
        b.insert("a".to_string(), b"1".into());
        b.insert("b".to_string(), b"2".into());
        b
    });

    static EXP_INPUT_DERIVATIONS_SIMPLE: LazyLock<BTreeMap<StorePath, BTreeSet<OutputName>>> =
        LazyLock::new(|| {
            let mut b = BTreeMap::new();
            b.insert(
                StorePath::from_bytes(b"8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv")
                    .unwrap(),
                BTreeSet::from([OutputName::out()]),
            );
            b.insert(
                StorePath::from_bytes(b"p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv")
                    .unwrap(),
                BTreeSet::from([OutputName::out(), "lib".parse().expect("valid OutputName")]),
            );
            b
        });

    static EXP_INPUT_DERIVATIONS_SIMPLE_ATERM: LazyLock<String> = LazyLock::new(|| {
        format!(
            "[(\"{0}\",[\"out\"]),(\"{1}\",[\"out\",\"lib\"])]",
            "/nix/store/8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv",
            "/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv"
        )
    });

    static EXP_INPUT_SOURCES_SIMPLE: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
        let mut b = BTreeSet::new();
        b.insert("/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out".to_string());
        b.insert("/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib".to_string());
        b
    });

    /// Ensure parsing KVs works
    #[rstest]
    #[case::empty(b"[]", &BTreeMap::new(), b"")]
    #[case::simple(b"[(\"a\",\"1\"),(\"b\",\"2\")]", &EXP_AB_MAP, b"")]
    fn parse_kv(
        #[case] input: &'static [u8],
        #[case] expected: &BTreeMap<String, BString>,
        #[case] exp_rest: &[u8],
    ) {
        let (rest, parsed) =
            super::parse_kv(crate::aterm::parse_bytes_field)(input).expect("must parse");
        assert_eq!(exp_rest, rest, "expected remainder");
        assert_eq!(*expected, parsed);
    }

    #[rstest]
    #[case::incomplete_empty(b"[")]
    #[case::incomplete_simple(b"[(\"a\",\"1\")")]
    #[case::incomplete_complicated_escape(b"[(\"a")]
    #[case::incomplete_complicated_sep(b"[(\"a\",")]
    #[case::incomplete_complicated_multi_escape(b"[(\"a\",\"")]
    #[case::incomplete_complicated_multi_outer_sep(b"[(\"a\",\"b\"),")]
    fn parse_kv_incomplete(#[case] input: &'static [u8]) {
        assert!(matches!(
            super::parse_kv(crate::aterm::parse_bytes_field)(input),
            Err(nom::Err::Incomplete(_))
        ));
    }

    /// Ensures the kv parser complains about duplicate map keys
    #[test]
    fn parse_kv_fail_dup_keys() {
        let input: &'static [u8] = b"[(\"a\",\"1\"),(\"a\",\"2\")]";
        let e = super::parse_kv(crate::aterm::parse_bytes_field)(input).expect_err("must fail");

        match e {
            nom::Err::Failure(e) => {
                assert_eq!(ErrorKind::DuplicateMapKey("a".to_string()), e.code);
            }
            _ => panic!("unexpected error"),
        }
    }

    /// Ensure parsing input derivations works.
    #[rstest]
    #[case::empty(b"[]", &BTreeMap::new())]
    #[case::simple(EXP_INPUT_DERIVATIONS_SIMPLE_ATERM.as_bytes(), &EXP_INPUT_DERIVATIONS_SIMPLE)]
    fn parse_input_derivations(
        #[case] input: &'static [u8],
        #[case] expected: &BTreeMap<StorePath, BTreeSet<OutputName>>,
    ) {
        let (rest, parsed) = super::parse_input_derivations(input).expect("must parse");

        assert_eq!(expected, &parsed, "parsed mismatch");
        assert!(rest.is_empty(), "rest must be empty");
    }

    /// Ensures the input derivation parser complains about duplicate output names
    #[test]
    fn parse_input_derivations_fail_dup_output_names() {
        let input_str = format!(
            "[(\"{0}\",[\"out\"]),(\"{1}\",[\"out\",\"out\"])]",
            "/nix/store/8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv",
            "/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv"
        );
        let e = super::parse_input_derivations(input_str.as_bytes()).expect_err("must fail");

        match e {
            nom::Err::Failure(e) => {
                assert_eq!(
                    ErrorKind::DuplicateInputDerivationOutputName(
                        "out".parse().expect("Valid OutputName"),
                        "/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv".to_string(),
                    ),
                    e.code
                );
            }
            _ => panic!("unexpected error"),
        }
    }

    /// Ensure parsing input sources works
    #[rstest]
    #[case::empty(b"[]", &BTreeSet::new())]
    #[case::simple(b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out\",\"/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib\"]", &EXP_INPUT_SOURCES_SIMPLE)]
    fn parse_input_sources(#[case] input: &'static [u8], #[case] expected: &BTreeSet<String>) {
        let (rest, parsed) = super::parse_input_sources(input).expect("must parse");

        assert_eq!(
            expected,
            &parsed
                .iter()
                .map(StorePath::to_absolute_path)
                .collect::<BTreeSet<_>>(),
            "parsed mismatch"
        );
        assert!(rest.is_empty(), "rest must be empty");
    }

    /// Ensures the input sources parser complains about duplicate input sources
    #[test]
    fn parse_input_sources_fail_dup_keys() {
        let input: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\"]";
        let e = super::parse_input_sources(input).expect_err("must fail");

        match e {
            nom::Err::Failure(e) => {
                assert_eq!(
                    ErrorKind::DuplicateInputSource(
                        StorePathRef::from_absolute_path(
                            "/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo".as_bytes()
                        )
                        .unwrap()
                        .to_owned()
                    ),
                    e.code
                );
            }
            _ => panic!("unexpected error"),
        }
    }

    #[rstest]
    #[case::simple(
        br#"("out","/nix/store/5vyvcwah9l9kf07d52rcgdk70g2f4y13-foo","","")"#,
        (
            OutputName::out(),
            StorePathRef::from_absolute_path("/nix/store/5vyvcwah9l9kf07d52rcgdk70g2f4y13-foo".as_bytes()).unwrap().to_owned(),
            None
        )
    )]
    #[case::fod(
        br#"("out","/nix/store/4q0pg5zpfmznxscq3avycvf9xdvx50n3-bar","r:sha256","08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba")"#,
        (
            OutputName::out(),
            StorePathRef::from_absolute_path(
                "/nix/store/4q0pg5zpfmznxscq3avycvf9xdvx50n3-bar".as_bytes()).unwrap().to_owned(),
            Some(OutputHash{
                mode: OutputHashMode::Recursive,
                hash: NixHash::Sha256(hex!("08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba")),
            }),
        )
    )]
    fn parse_output(
        #[case] input: &[u8],
        #[case] expected: (OutputName, StorePath, Option<OutputHash>),
    ) {
        let (rest, parsed) = super::parse_output(input).expect("must parse");
        assert!(rest.is_empty());
        assert_eq!(expected, parsed);
    }

    #[rstest]
    #[case::multi_out(
        br#"[("lib","/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib","",""),("out","/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out","","")]"#,
        &EXP_MULTI_OUTPUTS
    )]
    fn parse_outputs(#[case] input: &[u8], #[case] expected: &Outputs) {
        let (rest, parsed) = super::parse_outputs(input).expect("must parse");
        assert!(rest.is_empty());
        assert_eq!(*expected, parsed);
    }
}
