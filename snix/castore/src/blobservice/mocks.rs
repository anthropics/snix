use std::{io, pin::pin, task::Poll};

use tonic::async_trait;

use crate::B3Digest;

/// A BlobWriter simply accepting all data written to it.
/// Used for testing purposes.
pub struct TestBlobWriter {
    buf: Vec<u8>,
    closed: bool,
}

impl Default for TestBlobWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl TestBlobWriter {
    pub fn new() -> Self {
        Self {
            buf: vec![],
            closed: false,
        }
    }
}

impl tokio::io::AsyncWrite for TestBlobWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        pin!(self).buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[async_trait]
impl super::BlobWriter for TestBlobWriter {
    async fn close(&mut self) -> io::Result<B3Digest> {
        if self.closed {
            Err(io::Error::other("already closed"))
        } else {
            Ok(blake3::hash(&self.buf).into())
        }
    }
}
