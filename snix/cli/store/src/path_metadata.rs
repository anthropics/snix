//! Parsing of the store path metadata JSON accepted by `snix-store copy`.
//!
//! CppNix produced two different shapes of it over time, both of which are supported,
//! see [parse_all] for details.

use nix_compat::{narinfo::Signature, store_path::StorePath};
use serde::de::Error as _;
use serde_with::{DefaultOnNull, serde_as};
use std::collections::BTreeMap;

/// Metadata about a single store path.
///
/// It is less strict than [nix_compat::path_info::ExportedPathInfo] (no `closureSize`
/// field), and carries no store path itself - that's provided next to it.
#[serde_as]
#[derive(Debug, serde::Deserialize)]
pub struct PathMetadata {
    #[serde(
        rename = "narHash",
        deserialize_with = "nix_compat::nixhash::serde::from_nix_nixbase32_or_sri"
    )]
    pub nar_sha256: [u8; 32],

    #[serde(rename = "narSize")]
    pub nar_size: u64,

    pub deriver: Option<StorePath>,
    #[serde(default)]
    pub references: Vec<StorePath>,
    #[serde(default)]
    #[serde_as(as = "DefaultOnNull")]
    pub signatures: Vec<Signature<String>>,
}

/// [PathMetadata] with the store path inside the object itself.
#[derive(serde::Deserialize)]
struct KeyedPathMetadata {
    path: StorePath,

    #[serde(flatten)]
    metadata: PathMetadata,
}

/// The shape a document is in, as far as the first few bytes give it away.
enum Shape {
    /// A list of objects.
    List,
    /// An attrset keyed by (absolute) store path.
    ByStorePath,
    /// A single object.
    Object,
}

/// Peeks at the first few bytes to determine the shape, rather than using serde's
/// `untagged`, which would replace field-level parse errors with "data did not match
/// any variant".
fn shape(bytes: &[u8]) -> Shape {
    let trimmed = bytes.trim_ascii_start();

    match trimmed.first() {
        Some(b'[') => Shape::List,
        // Keys in the attrset shape are absolute store paths, whereas field names never
        // start with a slash. This also excludes the `--json-format 2` and 3 wrapper
        // object, whose keys are field names and store path base names.
        Some(b'{') if trimmed[1..].trim_ascii_start().starts_with(br#""/"#) => Shape::ByStorePath,
        _ => Shape::Object,
    }
}

/// Parses metadata for multiple store paths, either as
///
///  - a list of objects, each carrying its own `path` field (produced by the
///    `exportReferencesGraph` feature, as well as `nix path-info --json` in
///    Nix < 2.19 and Lix), or
///  - an attrset keyed by store path (produced by `nix path-info --json` in
///    CppNix >= 2.19, <https://github.com/NixOS/nix/issues/13413>).
pub fn parse_all(bytes: &[u8]) -> serde_json::Result<Vec<(StorePath, PathMetadata)>> {
    match shape(bytes) {
        Shape::List => Ok(serde_json::from_slice::<Vec<KeyedPathMetadata>>(bytes)?
            .into_iter()
            .map(|KeyedPathMetadata { path, metadata }| (path, metadata))
            .collect()),
        Shape::ByStorePath => Ok(serde_json::from_slice::<BTreeMap<StorePath, PathMetadata>>(
            bytes,
        )?
        .into_iter()
        .collect()),
        Shape::Object => Err(serde_json::Error::custom(
            "expected a list of objects, or an attrset keyed by store path",
        )),
    }
}

/// Parses metadata for a single store path, either as an object carrying its own
/// `path` field, or as an attrset with a single store path key.
pub fn parse_one(bytes: &[u8]) -> serde_json::Result<(StorePath, PathMetadata)> {
    match shape(bytes) {
        Shape::Object => {
            let KeyedPathMetadata { path, metadata } = serde_json::from_slice(bytes)?;

            Ok((path, metadata))
        }
        Shape::ByStorePath => {
            let elems: BTreeMap<StorePath, PathMetadata> = serde_json::from_slice(bytes)?;
            if elems.len() != 1 {
                return Err(serde_json::Error::custom(format!(
                    "expected metadata for a single store path, got {}",
                    elems.len()
                )));
            }

            Ok(elems.into_iter().next().expect("length checked"))
        }
        Shape::List => Err(serde_json::Error::custom(
            "expected a single object, or an attrset with a single store path key",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{PathMetadata, StorePath, parse_all, parse_one};

    /// `nix path-info --json --closure-size` in Nix < 2.19 and Lix. The
    /// `exportReferencesGraph` feature produces the same list-of-objects shape, but
    /// with different fields, see [EXPORT_REFERENCES_GRAPH].
    const LIST: &str = r#"[{"closureSize":10756176,"deriver":"/nix/store/vs9976cyyxpykvdnlv7x85fpp3shn6ij-libcxx-16.0.6.drv","narHash":"sha256-E73Nt0NAKGxCnsyBFDUaCAbA+wiF5qjq1O9J7WrnT0E=","narSize":7020664,"path":"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev","references":["/nix/store/lzzd5jgybnpfj86xkcpnd54xgwc4m457-libcxx-16.0.6"],"registrationTime":1730048276,"signatures":["cache.nixos.org-1:cTdhK6hnpPwtMXFX43CYb7v+CbpAusVI/MORZ3v5aHvpBYNg1MfBHVVeoexMBpNtHA8uFAn0aEsJaLXYIDhJDg=="],"valid":true}]"#;

    /// The same store path, as emitted by `nix path-info --json` in CppNix 2.35.1.
    /// The `storeDir` and `version` fields are ignored.
    const BY_STORE_PATH: &str = r#"{"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev":{"ca":null,"closureSize":10756176,"deriver":"/nix/store/vs9976cyyxpykvdnlv7x85fpp3shn6ij-libcxx-16.0.6.drv","narHash":"sha256-E73Nt0NAKGxCnsyBFDUaCAbA+wiF5qjq1O9J7WrnT0E=","narSize":7020664,"references":["/nix/store/lzzd5jgybnpfj86xkcpnd54xgwc4m457-libcxx-16.0.6"],"registrationTime":1730048276,"signatures":["cache.nixos.org-1:cTdhK6hnpPwtMXFX43CYb7v+CbpAusVI/MORZ3v5aHvpBYNg1MfBHVVeoexMBpNtHA8uFAn0aEsJaLXYIDhJDg=="],"storeDir":"/nix/store","ultimate":false,"version":1}}"#;

    /// The same store path as a single object, which Nix itself never emits - that's
    /// the shape we accept to support `--jsonl` mode in the CLI.
    const OBJECT: &str = r#"{"deriver":"/nix/store/vs9976cyyxpykvdnlv7x85fpp3shn6ij-libcxx-16.0.6.drv","narHash":"sha256-E73Nt0NAKGxCnsyBFDUaCAbA+wiF5qjq1O9J7WrnT0E=","narSize":7020664,"path":"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev","references":["/nix/store/lzzd5jgybnpfj86xkcpnd54xgwc4m457-libcxx-16.0.6"],"signatures":["cache.nixos.org-1:cTdhK6hnpPwtMXFX43CYb7v+CbpAusVI/MORZ3v5aHvpBYNg1MfBHVVeoexMBpNtHA8uFAn0aEsJaLXYIDhJDg=="]}"#;

    /// A single element of what the `exportReferencesGraph` feature produces: `narHash`
    /// as `sha256:$nixbase32` rather than SRI, and neither `deriver` nor `signatures`.
    const EXPORT_REFERENCES_GRAPH: &str = r#"[{"closureSize":1828984,"narHash":"sha256:11vm2x1ajhzsrzw7lsyss51mmr3b6yll9wdjn51bh7liwkpc8ila","narSize":1828984,"path":"/nix/store/7n0mbqydcipkpbxm24fab066lxk68aqk-libunistring-1.1","references":["/nix/store/7n0mbqydcipkpbxm24fab066lxk68aqk-libunistring-1.1"]}]"#;

    /// Two store paths in the attrset shape.
    const BY_STORE_PATH_MULTI: &str = r#"{"/nix/store/lzzd5jgybnpfj86xkcpnd54xgwc4m457-libcxx-16.0.6":{"narHash":"sha256-E73Nt0NAKGxCnsyBFDUaCAbA+wiF5qjq1O9J7WrnT0E=","narSize":42},"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev":{"narHash":"sha256-E73Nt0NAKGxCnsyBFDUaCAbA+wiF5qjq1O9J7WrnT0E=","narSize":7020664}}"#;

    fn assert_libcxx(store_path: &StorePath, metadata: &PathMetadata) {
        assert_eq!(
            "z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev",
            store_path.to_string()
        );
        assert_eq!(7020664, metadata.nar_size);
        assert_eq!(
            "13bdcdb74340286c429ecc8114351a0806c0fb0885e6a8ead4ef49ed6ae74f41",
            data_encoding::HEXLOWER.encode(&metadata.nar_sha256)
        );
        assert_eq!(
            Some("vs9976cyyxpykvdnlv7x85fpp3shn6ij-libcxx-16.0.6.drv".to_string()),
            metadata.deriver.as_ref().map(ToString::to_string)
        );
        assert_eq!(
            vec!["lzzd5jgybnpfj86xkcpnd54xgwc4m457-libcxx-16.0.6"],
            metadata
                .references
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(1, metadata.signatures.len());
    }

    /// Both shapes need to produce the same entries, leading whitespace included.
    #[test]
    fn parse_all_shapes() {
        for doc in [LIST, BY_STORE_PATH, &format!(" \n{BY_STORE_PATH}")] {
            let entries = parse_all(doc.as_bytes()).expect("must parse");

            assert_eq!(1, entries.len());
            assert_libcxx(&entries[0].0, &entries[0].1);
        }
    }

    /// A single object, as well as an attrset with a single key.
    #[test]
    fn parse_one_shapes() {
        for doc in [OBJECT, BY_STORE_PATH] {
            let (store_path, metadata) = parse_one(doc.as_bytes()).expect("must parse");

            assert_libcxx(&store_path, &metadata);
        }
    }

    /// The `exportReferencesGraph` shape encodes `narHash` differently, and carries
    /// neither `deriver` nor `signatures`.
    #[test]
    fn export_references_graph() {
        let entries = parse_all(EXPORT_REFERENCES_GRAPH.as_bytes()).expect("must parse");

        assert_eq!(1, entries.len());
        let (store_path, metadata) = &entries[0];

        assert_eq!(
            "7n0mbqydcipkpbxm24fab066lxk68aqk-libunistring-1.1",
            store_path.to_string()
        );
        assert_eq!(
            "8a46c4eee4911eb842b1b2f144a9376be45a43d1da6b7af8cffa43a942177587",
            data_encoding::HEXLOWER.encode(&metadata.nar_sha256)
        );
        assert_eq!(None, metadata.deriver);
        assert!(metadata.signatures.is_empty());
    }

    /// Each entrypoint only accepts the shapes it documents.
    #[test]
    fn wrong_shape() {
        assert!(parse_all(OBJECT.as_bytes()).is_err());
        assert!(parse_one(LIST.as_bytes()).is_err());
    }

    /// [parse_all] collects metadata for more than one store path.
    #[test]
    fn parse_all_multiple() {
        let entries = parse_all(BY_STORE_PATH_MULTI.as_bytes()).expect("must parse");

        assert_eq!(2, entries.len());
    }

    /// [parse_one] rejects metadata for more than a single store path.
    #[test]
    fn multiple_store_paths() {
        let err = parse_one(BY_STORE_PATH_MULTI.as_bytes())
            .expect_err("must fail")
            .to_string();

        assert!(err.contains("single store path"), "unexpected error: {err}");
    }

    /// The list shape needs to carry the store path in each object.
    #[test]
    fn missing_path() {
        let err = parse_all(
            br#"[{"narHash":"sha256-E73Nt0NAKGxCnsyBFDUaCAbA+wiF5qjq1O9J7WrnT0E=","narSize":7020664}]"#,
        )
        .expect_err("must fail")
        .to_string();

        assert!(err.contains("path"), "unexpected error: {err}");
    }

    /// Nix uses `null` for store paths that are not valid. We reject those: an invalid
    /// store path can't be part of a closure being copied, so it can't legitimately be
    /// referred to by any of the other store paths either.
    #[test]
    fn invalid_path() {
        const NULL: &[u8] =
            br#"{"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev":null}"#;

        for err in [
            parse_all(NULL).expect_err("must fail").to_string(),
            parse_one(NULL).expect_err("must fail").to_string(),
        ] {
            assert!(err.contains("null"), "unexpected error: {err}");
        }
    }

    /// Parse errors need to point at the offending field, rather than being swallowed
    /// by a fallback to another shape.
    #[test]
    fn parse_error() {
        for doc in [
            br#"{"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev":{"narHash":"nope","narSize":1}}"#.as_slice(),
            br#"[{"path":"/nix/store/z6r3bn5l51679pwkvh9nalp6c317z34m-libcxx-16.0.6-dev","narHash":"nope","narSize":1}]"#.as_slice(),
        ] {
            let err = parse_all(doc).expect_err("must fail").to_string();

            assert!(err.contains("nope"), "unexpected error: {err}");
        }
    }
}
