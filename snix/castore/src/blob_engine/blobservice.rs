//! Exposes any [BlobService] as a [BlobEngine], via the [BlobServiceEngine] adapter.

use tonic::async_trait;

use crate::{B3Digest, blob_engine::BlobEngine, blobservice::BlobService};

/// Adapter exposing any [BlobService] as a [BlobEngine].
///
/// This only exists for the duration of the migration from [BlobService] to
/// [BlobEngine], and will be removed once all [BlobService] users have been
/// migrated.
#[derive(Clone)]
pub struct BlobServiceEngine<T>(pub T);

#[async_trait]
impl<T> BlobEngine for BlobServiceEngine<T>
where
    T: BlobService,
{
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        BlobService::has(&self.0, digest)
            .await
            .map_err(|err| super::Error(Box::new(err)))
    }

    async fn open_read<'a>(
        &'a self,
        digest: &B3Digest,
        _size_hint: Option<u64>,
    ) -> Result<Option<Box<dyn crate::blobservice::BlobReader + 'a>>, super::Error> {
        BlobService::open_read(&self.0, digest)
            .await
            .map_err(|err| super::Error(Box::new(err)))
    }

    async fn open_write<'a>(&'a self) -> Box<dyn crate::blobservice::BlobWriter + 'a> {
        BlobService::open_write(&self.0).await
    }
}
