use std::{
    cmp::min,
    collections::HashMap,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use super::RenderError;

use bytes::BufMut;
use snix_castore::Node;
use snix_castore::directoryservice::DirectoryService;
use snix_castore::{
    blobservice::{BlobReader, BlobService},
    directoryservice::DirectoryGraphBuilder,
};

use futures::FutureExt;
use futures::TryStreamExt;
use futures::future::{BoxFuture, FusedFuture, TryMaybeDone};

use tokio::io::AsyncSeekExt;
use tracing::{instrument, warn};

mod segments;
use segments::{Segment, Segments};

pub struct Reader<B: BlobService> {
    segments: Segments,
    position_bytes: u64,
    position_index: usize,
    blob_service: Arc<B>,
    seeking: bool,
    current_blob: TryMaybeDone<BoxFuture<'static, io::Result<Box<dyn BlobReader>>>>,
}

impl<B: BlobService + 'static> Reader<B> {
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
        root_node: Node,
        blob_service: B,
        directory_service: impl DirectoryService,
    ) -> Result<Self, RenderError> {
        // If this is a directory, resolve all subdirectories
        let directories = if let Node::Directory { digest, .. } = root_node {
            let mut builder = DirectoryGraphBuilder::new_root_to_leaves(digest.to_owned());
            let mut directories = directory_service.get_recursive(&digest);
            while let Some(directory) = directories
                .try_next()
                .await
                .map_err(RenderError::DirectoryService)?
            {
                builder
                    .try_insert(directory)
                    .map_err(RenderError::OrderingError)?;
            }

            let directory_graph = builder.build().map_err(|err| {
                if err == snix_castore::directoryservice::OrderingError::EmptySet {
                    // The graph should at least contain the root, if there's no child directories.
                    // The only way we could run into this is by the
                    // DirectoryService not having the root directory we asked
                    // for, which hints to misconfiguration, so explicitly warn!.
                    let err = RenderError::DirectoryNotFound(digest,                    "root".into());
                    warn!(%err, "tried to render NAR, but DirectoryService didn't contain the root directory");
                    err
                } else {
                    RenderError::OrderingError(err)
                }
            })?;

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

        Ok(Self {
            segments: Segments::from_root_node_and_directories(root_node, &directories),
            position_bytes: 0,
            position_index: 0,
            blob_service: blob_service.into(),
            seeking: false,
            current_blob: TryMaybeDone::Gone,
        })
    }

    pub fn stream_len(&self) -> u64 {
        self.segments.total_len()
    }
}

impl<B: BlobService + 'static> tokio::io::AsyncSeek for Reader<B> {
    fn start_seek(mut self: Pin<&mut Self>, pos: io::SeekFrom) -> io::Result<()> {
        let stream_len = Reader::stream_len(&self);

        let this = &mut *self;
        if this.seeking {
            return Err(io::Error::other("Already seeking"));
        }
        this.seeking = true;

        let pos = {
            let (base, offset) = match pos {
                io::SeekFrom::Start(n) => (n, 0),
                io::SeekFrom::End(n) => (stream_len, n),
                io::SeekFrom::Current(n) => (this.position_bytes, n),
            };

            base.saturating_add_signed(offset)
        };

        let prev_position_bytes = this.position_bytes;
        let prev_position_index = this.position_index;

        let segments_with_offsets = this.segments.segments_with_offsets();

        this.position_bytes = min(pos, stream_len);
        this.position_index = match segments_with_offsets
            .binary_search_by_key(&this.position_bytes, |&(off, _)| off)
        {
            Ok(idx) => idx,
            Err(idx) => idx - 1,
        };

        let Some((offset, Segment::BlobRef { digest, .. })) =
            segments_with_offsets.get(this.position_index)
        else {
            // If not seeking into a blob, we clear the active blob reader and then we're done
            this.current_blob = TryMaybeDone::Gone;
            return Ok(());
        };
        let offset_in_segment = this.position_bytes - offset;

        if prev_position_bytes == this.position_bytes {
            // position has not changed. do nothing
        } else if prev_position_index == this.position_index {
            // seeking within the same segment, re-use the blob reader
            let mut prev = std::mem::replace(&mut this.current_blob, TryMaybeDone::Gone);
            this.current_blob = futures::future::try_maybe_done(
                (async move {
                    let mut reader = Pin::new(&mut prev).take_output().unwrap();
                    reader.seek(io::SeekFrom::Start(offset_in_segment)).await?;
                    Ok(reader)
                })
                .boxed(),
            );
        } else {
            // seek to a different segment
            let blob_service = this.blob_service.clone();
            let digest = *digest;
            this.current_blob = futures::future::try_maybe_done(
                (async move {
                    let mut reader =
                        blob_service
                            .open_read(&digest)
                            .await?
                            .ok_or(io::Error::new(
                                io::ErrorKind::NotFound,
                                RenderError::BlobNotFound(digest, Default::default()),
                            ))?;
                    if offset_in_segment != 0 {
                        reader.seek(io::SeekFrom::Start(offset_in_segment)).await?;
                    }
                    Ok(reader)
                })
                .boxed(),
            );
        };

        Ok(())
    }
    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<u64>> {
        let this = &mut *self;

        if !this.current_blob.is_terminated() {
            futures::ready!(this.current_blob.poll_unpin(cx))?;
        }
        this.seeking = false;

        Poll::Ready(Ok(this.position_bytes))
    }
}

impl<B: BlobService + 'static> tokio::io::AsyncRead for Reader<B> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut tokio::io::ReadBuf,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let segments_with_offsets = this.segments.segments_with_offsets();

        let Some(&(offset, ref segment)) = segments_with_offsets.get(this.position_index) else {
            return Poll::Ready(Ok(())); // EOF
        };

        let prev_read_buf_pos = buf.filled().len();
        match segment {
            Segment::Literal(data) => {
                let offset_in_segment = this.position_bytes - offset;
                let offset_in_segment = usize::try_from(offset_in_segment).unwrap();
                let remaining_data = data.len() - offset_in_segment;
                let read_size = std::cmp::min(remaining_data, buf.remaining());
                buf.put(&data[offset_in_segment..offset_in_segment + read_size]);
            }
            Segment::BlobRef { size, .. } => {
                futures::ready!(this.current_blob.poll_unpin(cx))?;
                this.seeking = false;
                let blob = Pin::new(&mut this.current_blob)
                    .output_mut()
                    .expect("missing blob");
                futures::ready!(Pin::new(blob).poll_read(cx, buf))?;
                let read_length = buf.filled().len() - prev_read_buf_pos;
                let maximum_expected_read_length = (offset + size) - this.position_bytes;
                let is_eof = read_length == 0;
                let too_much_returned = read_length as u64 > maximum_expected_read_length;
                match (is_eof, too_much_returned) {
                    (true, false) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "blob short read",
                        )));
                    }
                    (false, true) => {
                        buf.set_filled(prev_read_buf_pos);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "blob continued to yield data beyond end",
                        )));
                    }
                    _ => {}
                }
            }
        };
        let new_read_buf_pos = buf.filled().len();
        this.position_bytes += (new_read_buf_pos - prev_read_buf_pos) as u64;

        let prev_position_index = this.position_index;
        let segments_with_offsets = this.segments.segments_with_offsets();
        while segments_with_offsets
            .get(this.position_index)
            .is_some_and(|&(offset, ref segment)| (this.position_bytes - offset) >= segment.len())
        {
            this.position_index += 1;
        }
        if prev_position_index != this.position_index {
            let Some((_offset, Segment::BlobRef { digest, .. })) =
                segments_with_offsets.get(this.position_index)
            else {
                // If the next segment is not a blob, we clear the active blob reader and then we're done
                this.current_blob = TryMaybeDone::Gone;
                return Poll::Ready(Ok(()));
            };

            // The next segment is a blob, open the BlobReader
            let blob_service = this.blob_service.clone();
            let digest = *digest;
            this.current_blob = futures::future::try_maybe_done(
                (async move {
                    let reader = blob_service
                        .open_read(&digest)
                        .await?
                        .ok_or(io::Error::new(
                            io::ErrorKind::NotFound,
                            RenderError::BlobNotFound(digest, Default::default()),
                        ))?;
                    Ok(reader)
                })
                .boxed(),
            );
        }

        Poll::Ready(Ok(()))
    }
}
