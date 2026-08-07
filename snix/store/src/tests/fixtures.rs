use rstest::fixture;
use snix_castore::{
    blobservice::{BlobService, MemoryBlobService},
    directoryservice::DirectoryService,
};
use std::sync::Arc;

#[fixture]
pub(crate) fn blob_service() -> Arc<dyn BlobService> {
    Arc::from(MemoryBlobService::default())
}

#[fixture]
pub(crate) fn directory_service() -> Arc<dyn DirectoryService> {
    Arc::new(snix_castore::utils::gen_test_directory_service())
}
