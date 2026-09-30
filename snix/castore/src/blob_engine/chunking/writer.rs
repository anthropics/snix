use std::pin::pin;

use pin_project_lite::pin_project;
use tokio::io::AsyncWrite;
use tonic::async_trait;
use tracing::Span;

use crate::{B3Digest, blobstore::BlobStore};

pin_project! {
/// Wrapper to implementing [crate::blobservice::BlobWriter] for
/// [crate::blobstore::BlobWriter] (persisting [BlobMeta] in a [BlobStore])
pub(super) struct BlobWriterWrapper<BS>{
    inner: Option<(crate::blobstore::BlobWriter, BS)>
}
}

impl<BS> BlobWriterWrapper<BS> {
    pub fn new(blob_writer: crate::blobstore::BlobWriter, blob_service: BS) -> Self {
        Self {
            inner: Some((blob_writer, blob_service)),
        }
    }
}

impl<BS> AsyncWrite for BlobWriterWrapper<BS>
where
    BS: BlobStore,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let (writer, _) = self.project().inner.as_mut().ok_or(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "writer closed",
        ))?;

        pin!(writer).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let (writer, _) = self.project().inner.as_mut().ok_or(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "writer closed",
        ))?;

        pin!(writer).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let (writer, _) = self.project().inner.as_mut().ok_or(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "writer closed",
        ))?;

        pin!(writer).poll_shutdown(cx)
    }
}

#[async_trait]
impl<BS: BlobStore> crate::blobservice::BlobWriter for BlobWriterWrapper<BS> {
    #[tracing::instrument(skip_all, err, fields(blob.digest=tracing::field::Empty))]
    async fn close(&mut self) -> std::io::Result<B3Digest> {
        let (writer, blob_store) = self.inner.take().ok_or(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "writer closed",
        ))?;

        let (blob_meta, blob_digest) = writer.close().await?;

        Span::current().record("blob.digest", blob_digest.to_string());

        if !blob_store
            .has(&blob_digest)
            .await
            .map_err(std::io::Error::other)?
        {
            blob_store
                .put(&blob_digest, blob_meta)
                .await
                .map_err(std::io::Error::other)?;
        } else {
            tracing::debug!("blob already exists, skipping upload")
        }

        return Ok(blob_digest);
    }
}
