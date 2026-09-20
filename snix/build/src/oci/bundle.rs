//! Module to create an OCI runtime bundle for a given [BuildRequest].
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use super::scratch_name;
use crate::buildservice::BuildRequest;
use anyhow::Context;
use tracing::{debug, instrument};

/// Produce an OCI bundle in a given path.
/// Check [super::spec::make_spec] for a description about the paths produced.
#[instrument(err)]
pub(crate) fn make_bundle(
    request: &BuildRequest,
    runtime_spec: &oci_spec::runtime::Spec,
    path: &Path,
) -> anyhow::Result<()> {
    fs::create_dir_all(path).context("failed to create bundle path")?;

    let spec_json = serde_json::to_string(runtime_spec).context("failed to render spec to json")?;
    fs::write(path.join("config.json"), spec_json).context("failed to write config.json")?;

    fs::create_dir_all(path.join("inputs")).context("failed to create inputs dir")?;

    let root_path = path.join("root");

    fs::create_dir_all(&root_path).context("failed to create root path dir")?;
    fs::create_dir_all(root_path.join("etc")).context("failed to create root/etc dir")?;

    // TODO: populate /etc/{group,passwd}. It's a mess?

    let scratch_root = path.join("scratch");
    fs::create_dir_all(&scratch_root).context("failed to create scratch/ dir")?;

    // for each scratch path, calculate its name inside scratch, and ensure the
    // directory exists.
    for p in request.scratch_paths.iter() {
        let scratch_path = scratch_root.join(scratch_name(p));
        debug!(scratch_path=?scratch_path, path=?p, "about to create scratch dir");
        fs::create_dir_all(scratch_path.clone()).context("Unable to create scratch dir")?;

        // TODO(#152): this is a hack, in the general case we may not have the "build" directory and additional files
        // may not have /build prefix. But in practice today snix_build.rs is the only user of the builder and
        // it always sets up a /build scratch and populates all additional_files with the /build prefix.
        // For now this unblocks builds, but worth improving in the future.
        if p == Path::new("build") {
            for file in request.additional_files.iter() {
                if file.path.components().count() < 2
                    || file.path.components().next() != Some(Component::Normal(OsStr::new("build")))
                {
                    Err(std::io::Error::other(
                        "Additional files must start with build/",
                    ))?
                }

                // remove build/ prefix
                let p = file.path.components().skip(1).collect::<PathBuf>();
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(scratch_path.clone().join(parent))
                        .context("Failed to create dir for additional file")?;
                }
                let p = scratch_path.join(p);
                let mut out = std::fs::File::create(p).context("could not create file")?;
                out.write_all(&file.contents)?;
            }
        }
    }

    Ok(())
}
