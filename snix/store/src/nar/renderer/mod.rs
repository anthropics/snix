use crate::pathinfoservice;

use super::{NarCalculationService, RenderError};
use nix_compat::nixhash::Sha256Digester;
use snix_castore::{Node, blobservice::BlobService, directoryservice::DirectoryService};
use tokio_util::io::InspectWriter;
use tonic::async_trait;
use tracing::instrument;

mod seekable;
mod simple;

pub use seekable::{Reader, write_nar};
pub use simple::write_nar as write_nar_simple;

// FUTUREWORK: rename and move this to closer to NarCalculationService trait
pub struct SimpleRenderer<BS, DS> {
    blob_service: BS,
    directory_service: DS,
}

impl<BS, DS> SimpleRenderer<BS, DS> {
    pub fn new(blob_service: BS, directory_service: DS) -> Self {
        Self {
            blob_service,
            directory_service,
        }
    }
}

#[async_trait]
impl<BS, DS> NarCalculationService for SimpleRenderer<BS, DS>
where
    BS: BlobService,
    DS: DirectoryService,
{
    async fn calculate_nar(
        &self,
        root_node: &Node,
    ) -> Result<(u64, [u8; 32]), pathinfoservice::Error> {
        Ok(
            calculate_size_and_sha256(root_node, &self.blob_service, &self.directory_service)
                .await?,
        )
    }
}

/// Invoke [write_nar], and return the size and sha256 digest of the produced
/// NAR output.
#[instrument(skip_all)]
pub async fn calculate_size_and_sha256<BS, DS>(
    root_node: &Node,
    blob_service: &BS,
    directory_service: &DS,
) -> Result<(u64, [u8; 32]), RenderError>
where
    BS: BlobService,
    DS: DirectoryService,
{
    let mut digester = Sha256Digester::new();
    let mut nar_size = 0;
    let writer = InspectWriter::new(tokio::io::sink(), |data| {
        nar_size += data.len() as u64;
        digester.update(data);
    });

    write_nar(writer, root_node, blob_service, directory_service).await?;

    Ok((nar_size, digester.finalize().into()))
}
