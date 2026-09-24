mod bundle;
mod spec;
pub(crate) mod subuid;

pub(crate) use bundle::make_bundle;
pub(crate) use spec::make_spec;

use std::path::{Path, PathBuf};

use crate::sandbox::SandboxOutputs;

/// For a given scratch path, return the scratch_name that's allocated.
// We currently use use lower hex encoding of the b3 digest of the scratch
// path, so we don't need to globally allocate and pass down some uuids.
pub(crate) fn scratch_name(scratch_path: impl AsRef<[u8]>) -> String {
    data_encoding::BASE32.encode(blake3::hash(scratch_path.as_ref()).as_bytes())
}

#[derive(Debug, Clone)]
pub struct OciOutputs {
    /// Pre-sorted scratch mappings: `(guest_mountpoint, host_scratch_dir)`
    /// Sorted descending by component count so longer, more specific prefixes match first.
    scratch_mappings: Vec<(String, PathBuf)>,
}

impl OciOutputs {
    pub fn new<S>(bundle_path: &Path, scratches: &[S]) -> anyhow::Result<Self>
    where
        S: AsRef<str>,
    {
        let scratch_root = bundle_path.join("scratch");

        let mut scratch_mappings: Vec<(String, PathBuf)> = Vec::with_capacity(scratches.len());
        for mp in scratches {
            let mp = mp.as_ref();
            if !Path::new(mp).is_relative() || mp.is_empty() {
                anyhow::bail!("scratch path must be relative and non-empty: {mp:?}");
            }
            let host_dir = scratch_root.join(scratch_name(mp));
            scratch_mappings.push((mp.to_owned(), host_dir));
        }

        // Sort descending by number of path components so more specific prefixes match first
        scratch_mappings
            .sort_by_key(|a| std::cmp::Reverse(std::path::Path::new(&a.0).components().count()));

        Ok(Self { scratch_mappings })
    }
}

impl SandboxOutputs for OciOutputs {
    async fn find_path(&self, path: impl AsRef<Path>) -> Option<PathBuf> {
        let path = path.as_ref();
        for (mp, host_dir) in &self.scratch_mappings {
            if let Ok(relpath) = path.strip_prefix(mp) {
                let host_path = host_dir.join(relpath);
                if let Ok(metadata) = tokio::fs::symlink_metadata(&host_path).await
                    && (metadata.is_symlink() || metadata.is_dir() || metadata.is_file())
                {
                    return Some(host_path);
                }
                return None;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use rstest::rstest;
    use tempfile::TempDir;

    use super::{OciOutputs, scratch_name};
    use crate::sandbox::SandboxOutputs;

    #[rstest]
    #[tokio::test]
    #[case::simple("nix/store/aaaa", &["nix/store".into()], Some(("nix/store", "aaaa")))]
    #[case::prefix_no_sep("nix/store/aaaa", &["nix/sto".into()], None)]
    #[case::not_found("nix/store/aaaa", &["build".into()], None)]
    async fn test_find_path_in_scratches(
        #[case] search_path: &str,
        #[case] mountpoints: &[String],
        #[case] expected: Option<(&str, &str)>,
    ) {
        let temp_dir = TempDir::new().unwrap();
        let bundle_path = temp_dir.path();

        let expected_path = expected.map(|(mp, rel)| {
            let p = bundle_path.join("scratch").join(scratch_name(mp)).join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"").unwrap();
            p
        });

        let outputs = OciOutputs::new(bundle_path, mountpoints).expect("must succeed");

        assert_eq!(outputs.find_path(search_path).await, expected_path);
    }

    #[tokio::test]
    async fn test_get_host_output_paths_simple() {
        let temp_dir = TempDir::new().unwrap();
        let bundle_path = temp_dir.path();
        let scratch_paths = vec!["build", "nix/store"];

        let mut expected_path = PathBuf::new();
        expected_path.push(bundle_path);
        expected_path.push("scratch");
        expected_path.push(scratch_name("nix/store"));
        expected_path.push("fhaj6gmwns62s6ypkcldbaj2ybvkhx3p-foo");

        fs::create_dir_all(expected_path.parent().unwrap()).unwrap();
        fs::write(&expected_path, b"").unwrap();

        let outputs = OciOutputs::new(bundle_path, &scratch_paths).expect("must succeed");

        assert_eq!(
            Some(expected_path),
            outputs
                .find_path("nix/store/fhaj6gmwns62s6ypkcldbaj2ybvkhx3p-foo")
                .await
        );
    }

    #[test]
    fn test_oci_outputs_invalid_scratch() {
        let temp_dir = TempDir::new().unwrap();
        let bundle_path = temp_dir.path();

        let err = OciOutputs::new(bundle_path, &["/absolute"]).unwrap_err();
        assert!(err.to_string().contains("scratch path must be relative"));

        let err = OciOutputs::new(bundle_path, &[""]).unwrap_err();
        assert!(err.to_string().contains("scratch path must be relative"));
    }
}
