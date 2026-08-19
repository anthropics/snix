use std::{
    collections::HashMap,
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use futures::ready;
use pin_project::pin_project;
use segments::Segments;
use snix_castore::Node;
use snix_castore::directoryservice::DirectoryService;
use snix_castore::{blobservice::BlobService, directoryservice::DirectoryGraphBuilder};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};
use tokio_stream::StreamExt;
use tracing::{instrument, warn};

use crate::nar::RenderError;

mod segments;

#[cfg(test)]
mod test;

/// The number of segments to poll data from concurrently.
const SEGMENT_CONCURRENCY: usize = 24;

pub async fn write_nar<W, BS, DS>(
    mut w: W,
    root_node: &Node,
    blob_service: &BS,
    directory_service: &DS,
) -> Result<(), RenderError>
where
    W: AsyncWrite + Unpin + Send,
    BS: BlobService,
    DS: DirectoryService,
{
    let mut reader = Reader::new(root_node, blob_service, directory_service).await?;
    tokio::io::copy_buf(&mut reader, &mut w)
        .await
        // FUTUREWORK: RenderError makes no sense
        .map_err(RenderError::BlobService)?;

    Ok(())
}

#[pin_project]
pub struct Reader<'bs, BS: BlobService + 'bs> {
    segments: Segments,
    pos: u64,
    blob_service: BS,
    #[pin]
    rd: Box<dyn AsyncBufRead + Send + Unpin + 'bs>,
}

impl<'bs, BS: BlobService + Clone + 'bs> Reader<'bs, BS> {
    /// Creates a new seekable NAR renderer for the given castore root node.
    ///
    /// This function pre-fetches the directory closure using `get_recursive()` and assembles the
    /// NAR structure, except the file contents which are stored as 'holes' with references to a blob
    /// of a specific BLAKE3 digest and known size.
    /// The AsyncRead implementation will then switch between serving the
    /// precomputed literal segments, and the appropriate blob for the file
    /// contents.
    #[instrument(skip(blob_service, directory_service), err)]
    pub async fn new(
        root_node: &Node,
        blob_service: BS,
        directory_service: impl DirectoryService,
        // FUTUREWORK: add concurrency arg
    ) -> Result<Self, RenderError> {
        // If this is a directory, resolve all subdirectories
        let directories = if let Node::Directory { digest, .. } = root_node {
            let mut builder = DirectoryGraphBuilder::new_root_to_leaves(digest.to_owned());
            let mut directories = directory_service.get_recursive(digest).peekable();

            if directories.peek().await.is_none() {
                // The only way we could run into this is by the
                // DirectoryService not having the root directory we asked
                // for, which hints to misconfiguration, so explicitly warn!.
                let err = RenderError::DirectoryNotFound(*digest, "root".into());
                warn!(%err, "tried to render NAR, but DirectoryService didn't contain the root directory");
                Err(err)?;
            }

            while let Some(directory) = directories
                .try_next()
                .await
                .map_err(RenderError::DirectoryService)?
            {
                builder
                    .try_insert(directory)
                    .map_err(RenderError::OrderingError)?;
            }

            let directory_graph = builder.build().map_err(RenderError::OrderingError)?;

            HashMap::from_iter(
                directory_graph
                    // drain order doesn't really matter
                    .drain_leaves_to_root()
                    .map(|d| (d.digest(), d)),
            )
        } else {
            // If the top-level node is a file or a symlink, there is no directory graph.
            Default::default()
        };

        let segments = Segments::from_root_node_and_directories(root_node, &directories);
        let rd = segments.reader_for_offset(0, SEGMENT_CONCURRENCY, blob_service.clone());

        Ok(Self {
            segments,
            pos: 0,
            blob_service,
            rd,
        })
    }

    pub fn nar_size(&self) -> u64 {
        self.segments.total_len()
    }
}

impl<'bs, BS: BlobService> AsyncRead for Reader<'bs, BS> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut tokio::io::ReadBuf,
    ) -> Poll<io::Result<()>> {
        let this = self.project();

        let bytes_read = {
            let filled = buf.filled().len();
            ready!(this.rd.poll_read(cx, buf))?;
            buf.filled().len() - filled
        };
        *this.pos = this
            .pos
            .checked_add(bytes_read as u64)
            .ok_or(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "position > u64::MAX bytes",
            ))?;

        Poll::Ready(Ok(()))
    }
}

impl<'bs, BS: BlobService> AsyncBufRead for Reader<'bs, BS> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.project();
        this.rd.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.project();

        this.rd.consume(amt);
        *this.pos = this
            .pos
            .checked_add(amt as u64)
            .expect("consume would increase pos > u64::MAX bytes");
    }
}

impl<'bs, BS: BlobService + Clone + 'bs> AsyncSeek for Reader<'bs, BS> {
    fn start_seek(self: Pin<&mut Self>, pos: io::SeekFrom) -> io::Result<()> {
        let nar_size = self.nar_size();
        let new_pos = calc_pos(self.pos, nar_size, pos)?;

        if new_pos != self.pos {
            // FUTUREWORK: seek forward small amounts by skipping?
            let mut this = self.project();

            *this.rd = this.segments.reader_for_offset(
                new_pos,
                SEGMENT_CONCURRENCY,
                this.blob_service.clone(),
            );
            *this.pos = new_pos;
        }

        Ok(())
    }
    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

/// For a given nar_size and current position, returns the position that seek_from would seek to.
fn calc_pos(cur_pos: u64, nar_size: u64, seek_from: SeekFrom) -> std::io::Result<u64> {
    let new_pos = match seek_from {
        SeekFrom::Start(p) => p,
        SeekFrom::End(p) => nar_size.checked_sub_signed(p).ok_or(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tried to seek before beginning of NAR",
        ))?,
        SeekFrom::Current(p) => cur_pos.checked_add_signed(p).ok_or(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "tried to seek way past end of NAR",
        ))?,
    };

    if new_pos > nar_size {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "tried to seek past end of NAR",
        ))
    } else {
        Ok(new_pos)
    }
}
