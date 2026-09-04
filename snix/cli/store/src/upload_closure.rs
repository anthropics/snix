use std::collections::HashSet;

use futures::{FutureExt, StreamExt, TryStreamExt, future::ready};
use futures_dag::FuturesDag;
use nix_compat::store_path::StorePath;
use snix_castore::{
    blobservice::BlobService,
    directoryservice::DirectoryService,
    import::{IngestionError, fs},
};
use snix_store::{path_info::PathInfo, pathinfoservice::PathInfoService};
use tokio::sync::Semaphore;
use tracing::{Instrument, Span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::path_metadata::PathMetadata;

/// Takes a closure of store path metadata (keyed by StorePath),
/// and uploads it to the passed services.
///
/// A PathInfo is only written once all PathInfos of referenced store paths have
/// been written (or already existed before), and its contents have been uploaded.
///
// FUTUREWORK: This does not re-calculate the NAR Hash/Size.
// Should probably be configurable.
#[tracing::instrument(skip_all, fields(indicatif.pb_show = tracing::field::Empty), err)]
pub async fn upload_closure<PS, DS, BS>(
    reference_graph: Vec<(StorePath, PathMetadata)>,
    blob_service: BS,
    directory_service: DS,
    path_info_service: PS,
    ingest_concurrency: usize,
) -> Result<Vec<PathInfo>, Box<dyn std::error::Error + Send + Sync + 'static>>
where
    PS: PathInfoService,
    DS: DirectoryService,
    BS: BlobService,
{
    let closure_span = Span::current();
    closure_span.pb_set_style(&snix_tracing::PB_PROGRESS_STYLE);
    closure_span.pb_set_message("Uploading closure");
    closure_span.pb_set_length(reference_graph.len() as u64);

    // create a child span tracking ingestion
    let ingestion_span =
        tracing::info_span!("ingest_paths", "indicatif.pb_show" = tracing::field::Empty);
    ingestion_span.pb_set_style(&snix_tracing::PB_PROGRESS_STYLE);
    ingestion_span.pb_set_message("Ingesting contents");

    let ingest_permits = Semaphore::new(ingest_concurrency);

    let mut dag = FuturesDag::<DagKey<'_>, _>::new();

    let mut ingestion_tasks_total: u64 = 0;

    for (store_path, path_metadata) in &reference_graph {
        // Check if the PathInfo already exists. If it does, we don't need to do any work,
        // so add an empty metadata task for other tasks to depend on.
        if path_info_service.has(*store_path.digest()).await? {
            dag.insert(
                DagKey::PathInfo { store_path },
                [].into(), // we transitively assume this also means all children exist.
                ready(Ok::<_, Error>(None)).boxed(),
            )
            .expect("failed to insert empty metadata task");

            closure_span.pb_inc(1);

            continue;
        }
        ingestion_tasks_total += 1;

        // Else, create a task to upload the contents, not depending on anything,
        // and a task persisting metadata, depending on that task, and on all metadata from its references.
        // We use a oneshot channel to store the PathInfo returned from the ingestion task and pick it up from the metadata task.
        let (tx_node, rx_node) = tokio::sync::oneshot::channel();

        dag.insert(
            DagKey::Ingestion { store_path },
            [].into(),
            async {
                let p = ingest_permits.acquire().await.expect("semaphore closed");

                let path_info = ingest(
                    store_path.to_owned(),
                    path_metadata.to_owned(),
                    &blob_service,
                    &directory_service,
                )
                .await?;

                tx_node
                    .send(path_info)
                    .expect("Snix bug: channel closed (tx)");
                ingestion_span.pb_inc(1);

                drop(p);

                Ok(None)
            }
            .instrument({
                tracing::info_span!(
                    parent: &ingestion_span,
                    "ingest_task",
                )
            })
            .boxed(),
        )
        .expect("failed to insert ingestion task");

        dag.insert(
            DagKey::PathInfo { store_path },
            // have that task depend on all references (except self-references)
            HashSet::from_iter(
                path_metadata.references.iter().filter_map(|r| {
                    (r != store_path).then_some(DagKey::PathInfo { store_path: r })
                }),
            ),
            async {
                let path_info = rx_node.await.expect("Snix bug: channel closed (rx)");

                let path_info = path_info_service.put(path_info).await?;
                closure_span.pb_inc(1);

                Ok(Some(path_info))
            }
            .instrument({
                let sp = tracing::trace_span!(
                    parent: &closure_span,
                    "pathinfo_task",
                    "indicatif.pb_show" = tracing::field::Empty,
                    path_info.store_path = %store_path,
                );
                sp.pb_set_style(&snix_tracing::PB_SPINNER_STYLE);
                sp.pb_set_message(&format!(
                    "Persisting PathInfo for {}",
                    store_path.to_absolute_path()
                ));
                sp
            })
            .boxed(),
        )
        .expect("failed to insert metadata task");
    }

    ingestion_span.pb_set_length(ingestion_tasks_total);

    Ok(dag
        .filter_map(|(_key, result)| async { result.transpose() })
        .try_collect()
        .await?)
}

/// The key used in the FuturesDag to reference tasks with.
#[derive(Eq, Clone, PartialEq, Hash)]
enum DagKey<'a> {
    /// Refers to the upload of contents
    Ingestion { store_path: &'a StorePath },
    /// Refers to the upload of Pathtinfos
    PathInfo { store_path: &'a StorePath },
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("ingestion error")]
    Ingestion(#[from] IngestionError<fs::Error>),

    #[error("other error")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// For given store path and metadata, ingest contents into castore.
///
/// The to-be-inserted PathInfo is returned, but not inserted anywhere,
/// that's left for the callsite.
#[tracing::instrument(skip_all, fields(path_info.store_path = %store_path, indicatif.pb_show = tracing::field::Empty), err)]
pub async fn ingest<DS, BS>(
    store_path: StorePath,
    metadata: PathMetadata,
    blob_service: BS,
    directory_service: DS,
) -> Result<PathInfo, IngestionError<snix_castore::import::fs::Error>>
where
    BS: BlobService,
    DS: DirectoryService,
{
    let PathMetadata {
        nar_sha256,
        nar_size,
        deriver,
        references,
        signatures,
    } = metadata;

    let span = Span::current();
    span.pb_set_style(&snix_tracing::PB_SPINNER_NO_POS_LEN_STYLE);
    span.pb_set_message(&format!("Ingesting {}", store_path.to_absolute_path()));

    let node = snix_castore::import::fs::ingest_path::<_, _, _, &[u8]>(
        &blob_service,
        &directory_service,
        store_path.to_absolute_path(),
        None,
    )
    .await?;

    Ok(PathInfo {
        store_path,
        node,
        references,
        nar_size,
        nar_sha256,
        signatures,
        deriver,
        ca: None,
    })
}
