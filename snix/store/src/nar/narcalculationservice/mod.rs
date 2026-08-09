use auto_impl::auto_impl;
use nix_compat::nixhash::Sha256Digester;
use snix_castore::{Node, blobservice::BlobService, directoryservice::DirectoryService};
use tokio_util::io::InspectWriter;
use tonic::async_trait;

use crate::{nar::write_nar, pathinfoservice};

#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait NarCalculationService: Send + Sync {
    /// Return the nar size and nar sha256 digest for a given root node.
    /// This can be used to calculate NAR-based output paths.
    async fn calculate_nar(
        &self,
        root_node: &Node,
    ) -> Result<(u64, [u8; 32]), pathinfoservice::Error>;
}

/// [NarCalculationService] traversing the node and rendering the NAR
/// to calculate NAR hash and size.
pub struct Renderer<BS, DS> {
    blob_service: BS,
    directory_service: DS,
}

impl<BS, DS> Renderer<BS, DS> {
    pub fn new(blob_service: BS, directory_service: DS) -> Self {
        Self {
            blob_service,
            directory_service,
        }
    }
}

#[async_trait]
impl<BS, DS> NarCalculationService for Renderer<BS, DS>
where
    BS: BlobService,
    DS: DirectoryService,
{
    async fn calculate_nar(
        &self,
        root_node: &Node,
    ) -> Result<(u64, [u8; 32]), pathinfoservice::Error> {
        // Invoke [write_nar], and return the size and sha256 digest of the produced NAR output.
        let mut digester = Sha256Digester::new();

        let mut nar_size = 0;
        let writer = InspectWriter::new(tokio::io::sink(), |data| {
            nar_size += data.len() as u64;
            digester.update(data);
        });

        write_nar(
            writer,
            root_node,
            &self.blob_service,
            &self.directory_service,
        )
        .await?;

        Ok((nar_size, digester.finalize().into()))
    }
}
