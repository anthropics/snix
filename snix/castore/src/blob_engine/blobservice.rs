//! Implements [BlobEngine] for all [BlobService].

use tonic::async_trait;

use crate::{B3Digest, blob_engine::BlobEngine, blobservice::BlobService};

#[async_trait]
impl<T> BlobEngine for T
where
    T: BlobService,
{
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        BlobService::has(self, digest)
            .await
            .map_err(|err| super::Error(Box::new(err)))
    }

    async fn open_read<'a>(
        &'a self,
        digest: &B3Digest,
        _size_hint: Option<u64>,
    ) -> Result<Option<Box<dyn crate::blobservice::BlobReader + 'a>>, super::Error> {
        BlobService::open_read(self, digest)
            .await
            .map_err(|err| super::Error(Box::new(err)))
    }

    async fn open_write<'a>(&'a self) -> Box<dyn crate::blobservice::BlobWriter + 'a> {
        BlobService::open_write(self).await
    }
}
