use std::{cell::RefCell, io, os::unix::ffi::OsStrExt as _, sync::Arc};

use futures::TryStreamExt as _;
use nix_compat::{
    nixhash::CAHash,
    store_path::{StorePath, StorePathRef},
};
use snix_build::buildservice::{BuildResult, BuildService};
use snix_castore::{
    blobservice::BlobService,
    directoryservice::{DirectoryService, traversal::descend_to},
};
use snix_store::{
    nar::NarCalculationService, path_info::PathInfo, pathinfoservice::PathInfoService,
};
use tracing::{Level, Span, instrument, warn};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use url::Url;

use crate::{builder, fetchers::Fetcher, known_paths::KnownPaths};

pub struct BuildState {
    // This is public so helper functions can interact with the stores directly.
    pub blob_service: Arc<dyn BlobService>,
    pub directory_service: Arc<dyn DirectoryService>,
    pub path_info_service: Arc<dyn PathInfoService>,
    pub nar_calculation_service: Arc<dyn NarCalculationService>,

    #[allow(dead_code)]
    build_service: Arc<dyn BuildService>,

    #[allow(clippy::type_complexity)]
    pub fetcher: Fetcher<
        Arc<dyn BlobService>,
        Arc<dyn DirectoryService>,
        Arc<dyn PathInfoService>,
        Arc<dyn NarCalculationService>,
    >,

    // Paths known how to produce, by building or fetching.
    pub known_paths: RefCell<KnownPaths>,
}

impl BuildState {
    pub fn new(
        blob_service: Arc<dyn BlobService>,
        directory_service: Arc<dyn DirectoryService>,
        path_info_service: Arc<dyn PathInfoService>,
        nar_calculation_service: Arc<dyn NarCalculationService>,
        build_service: Arc<dyn BuildService>,
        hashed_mirrors: Vec<Url>,
    ) -> Self {
        Self {
            blob_service: blob_service.clone(),
            directory_service: directory_service.clone(),
            path_info_service: path_info_service.clone(),
            nar_calculation_service: nar_calculation_service.clone(),
            build_service,
            fetcher: Fetcher::new(
                blob_service,
                directory_service,
                path_info_service,
                nar_calculation_service,
                hashed_mirrors,
            ),
            known_paths: Default::default(),
        }
    }

    /// for a given [StorePath] and additional [snix_castore::Path] inside the store path,
    /// look up the [PathInfo], and if it exists, and then uses
    /// [descend_to] to return the [snix_castore::Node] specified by `sub_path`.
    ///
    /// In case there is no PathInfo yet, we check "self.known_paths" (learnt by
    /// the evaluator)
    ///  - If there's a fetch, we do that and return the resulting PathInfo.
    ///  - If there's a Derivation, we build it and return the resulting
    ///    PathInfo.
    ///    To build it, we need to do a function call to ourselves with all
    ///    inputs of that Derivation, which will trigger fetches / builds of
    ///    inputs, recursively.
    ///
    /// This should be replaced with a proper scheduler knowing about a partial
    /// subgraph at some point, because this design doesn't allow concurrent
    /// builds yet.
    #[instrument(skip(self, store_path), fields(store_path=%store_path, indicatif.pb_show=tracing::field::Empty), ret(level = Level::TRACE), err(level = Level::TRACE))]
    pub async fn store_path_to_path_info(
        &self,
        store_path: &StorePathRef<'_>,
        sub_path: &snix_castore::Path,
    ) -> io::Result<Option<PathInfo>> {
        Span::current().pb_set_message(&format!("Accessing {store_path}/{sub_path}"));

        // Find the root node for the store_path.
        // It asks the PathInfoService first, but in case there was a Derivation
        // produced that would build it, fall back to triggering the build.
        // To populate the input nodes, it might recursively trigger builds of
        // its dependencies too.
        let mut path_info = if let Some(path_info) = self
            .path_info_service
            .as_ref()
            .get(*store_path.digest())
            .await
            .map_err(std::io::Error::other)?
        {
            path_info
        } else {
            // If there's no PathInfo found, this normally means we have to
            // trigger the build (and insert into PathInfoService, after
            // reference scanning).
            // However, as Snix is (currently) not managing /nix/store itself,
            // we return Ok(None) to let std_io take over.
            // While reading from store paths that are not known to Snix during
            // that evaluation clearly is an impurity, we still need to support
            // it for things like <nixpkgs> pointing to a store path.
            // In the future, these things will (need to) have PathInfo.
            //
            // The store path doesn't exist yet, so we need to fetch or build it.
            // We check for fetches first, as we might have both native
            // fetchers and FODs in KnownPaths, and prefer the former.
            // This will also find [Fetch] synthesized from
            // `builtin:fetchurl` Derivations.
            let maybe_fetch = self
                .known_paths
                .borrow()
                .get_fetch_for_output_path(store_path);
            if let Some((name, fetch)) = maybe_fetch {
                let (sp, path_info) = self
                    .fetcher
                    .ingest_and_persist(&name, fetch)
                    .await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

                debug_assert_eq!(
                    sp.to_absolute_path(),
                    store_path.to_absolute_path(),
                    "store path returned from fetcher must match store path we have in fetchers"
                );

                path_info
            } else {
                // Look up the derivation for this output path.
                let (drv_path, drv) = {
                    let known_paths = self.known_paths.borrow();
                    if let Some(drv_path) = known_paths.get_drv_path_for_output_path(store_path) {
                        (
                            drv_path.to_owned(),
                            known_paths
                                .get_drv_by_drvpath(&drv_path.as_ref())
                                .unwrap()
                                .to_owned(),
                        )
                    } else {
                        warn!(store_path=%store_path, "no drv found");
                        // let StdIO take over
                        return Ok(None);
                    }
                };
                let span = Span::current();
                span.pb_start();
                span.pb_set_style(&snix_tracing::PB_SPINNER_STYLE);
                span.pb_set_message(&format!("⏳Waiting for inputs {}", store_path));

                // derivation_to_build_request needs castore nodes for all inputs.
                // Provide them, which means, here is where we recursively build
                // all dependencies.
                let resolved_inputs = {
                    let known_paths = &self.known_paths.borrow();
                    builder::get_all_inputs(&drv, known_paths, |path| {
                        Box::pin(async move {
                            self.store_path_to_path_info(&path.as_ref(), snix_castore::Path::ROOT)
                                .await
                        })
                    })
                }
                .try_collect()
                .await?;

                // precompute the `ca` field in the PathInfo, so drv can be sent away owned.
                let mut ca = drv.fod_digest().map(|fod_digest| {
                    CAHash::Nar(nix_compat::nixhash::NixHash::Sha256(fod_digest.into()))
                });

                // synthesize the build request.
                let build_request = builder::derivation_into_build_request(drv, &resolved_inputs)?;

                // Assemble a mapping table from needle back to store path, as well as a list of all outputs.
                // The latter is a subset of the former.
                // We need this to understand the response later.
                let mut output_paths: Vec<StorePath> =
                    Vec::with_capacity(build_request.outputs.len());
                let all_possible_refs: Vec<StorePath> = build_request
                    .outputs
                    .iter()
                    .map(|p| {
                        let sp = StorePath::from_bytes(
                            p.strip_prefix(&nix_compat::store_path::STORE_DIR[1..])
                                .expect("output doesn't have expected store_dir prefix")
                                .as_os_str()
                                .as_bytes(),
                        )
                        .expect("Snix bug: cannot parse output as StorePath");
                        output_paths.push(sp.clone());

                        sp
                    })
                    .chain(resolved_inputs.keys().cloned())
                    .collect();

                span.pb_set_message(&format!("🔨Building {}", store_path));

                // create a build
                let build_result = BuildResult::try_from_build_updates(
                    self.build_service.do_build_streaming(build_request),
                )
                .await
                .map_err(std::io::Error::other)?;

                let mut out_path_info: Option<PathInfo> = None;

                // For each output, insert a PathInfo.
                for (output, output_path) in build_result.outputs.into_iter().zip(output_paths) {
                    // calculate the nar representation
                    let (nar_size, nar_sha256) = self
                        .nar_calculation_service
                        .calculate_nar(&output.node)
                        .await
                        .map_err(std::io::Error::other)?;

                    // assemble the PathInfo to persist
                    let path_info = PathInfo {
                        store_path: output_path.clone(),
                        node: output.node,
                        references: {
                            let mut references = Vec::with_capacity(output.output_needles.len());

                            // Map each output needle index back into a store path.
                            for needle_idx in output.output_needles {
                                let output = all_possible_refs
                                    .get(needle_idx as usize)
                                    .ok_or(std::io::Error::other("invalid needle_idx"))?
                                    .clone();
                                references.push(output);
                            }

                            // Produce references sorted by name for consistency with nix narinfos
                            references.sort();
                            references
                        },
                        nar_size,
                        nar_sha256,
                        signatures: vec![],
                        deriver: Some(
                            StorePath::from_name_and_digest_fixed(
                                drv_path
                                    .name()
                                    .strip_suffix(".drv")
                                    .expect("missing .drv suffix"),
                                *drv_path.digest(),
                            )
                            .expect("Snix bug: StorePath without .drv suffix must be valid"),
                        ),
                        // CA derivations only have one output, so this only runs once.
                        ca: ca.take(),
                    };

                    self.path_info_service
                        .put(path_info.clone())
                        .await
                        .map_err(std::io::Error::other)?;

                    if *store_path == output_path.as_ref() {
                        out_path_info = Some(path_info);
                    }
                }

                out_path_info.ok_or(io::Error::other("build didn't produce store path"))?
            }
        };

        // now with the root_node and sub_path, descend to the node requested.
        Ok(
            descend_to(&self.directory_service, path_info.node.clone(), sub_path)
                .await
                .map_err(std::io::Error::other)?
                .map(|node| {
                    path_info.node = node;
                    path_info
                }),
        )
    }
}
