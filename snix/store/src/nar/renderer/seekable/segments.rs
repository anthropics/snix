use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use nix_compat::nar::writer::sync as nar_writer;
use snix_castore::{B3Digest, Directory, Node, blobservice::BlobService};
use std::{
    collections::HashMap,
    io::{self},
    sync::atomic::AtomicU64,
};
use tokio::io::{AsyncBufRead, AsyncReadExt};
use tokio_util::io::{InspectReader, ReaderStream, StreamReader};

/// Contains a list of [Segment]s.
/// For each segment, stores its offset to allow binary search seeking.
pub struct Segments {
    segments: Vec<(u64, Segment)>,
    total_len: u64,
}

impl Segments {
    /// Construct segments that can be used to assemble a NAR,
    /// using the given root_node and directories.
    /// Panics if the directory closure is not complete.
    pub fn from_root_node_and_directories(
        root_node: &Node,
        directories: &HashMap<B3Digest, Directory>,
    ) -> Self {
        let mut segments = Self {
            segments: vec![],
            total_len: 0,
        };

        let mut buf: Vec<u8> = vec![];
        let nar_node = nar_writer::open(&mut buf).expect("Snix bug: failed to open nar_writer");

        walk_node(&mut segments, directories, root_node, nar_node);

        // Flush the final segment
        segments.push(Segment::Literal(std::mem::take(&mut buf)));
        segments
    }

    /// Provides a [AsyncBufRead] into a [Segments] from the given offset.
    ///
    /// A [BlobService] to read blobs from needs to be specified, as well as
    /// the desired concurrency (for segments, not blobs).
    ///
    /// This does not implement AsyncSeek, seeking can be done by calling this
    /// again with another offset.
    pub fn reader_for_offset<'bs, BS: BlobService + Clone + 'bs>(
        &self,
        offset: u64,
        segment_concurrency: usize,
        blob_service: BS,
    ) -> Box<dyn AsyncBufRead + Send + Unpin + 'bs> {
        // find the segment at the selected offset, or right before it
        let (idx, skip_in_segment) = match self
            .segments
            .binary_search_by_key(&offset, |(offset, _)| *offset)
        {
            Ok(idx) => (idx, 0),
            Err(idx) => (idx - 1, offset - self.segments[idx - 1].0),
        };

        let segments: Vec<_> = self.segments[idx..]
            .iter()
            .map(|(_offset, segment)| segment.clone())
            .collect();

        // We might need to skip something from the first segment.
        // Turn the iterator of segments into bytes to skip at the beginning and the Segment itself.
        let items = segments
            .into_iter()
            .zip(std::iter::once((skip_in_segment) as usize).chain(std::iter::repeat(0)));

        // produce a stream of byte chunks
        let bytes_stream = tokio_stream::iter(items)
            .map(move |(segment, skip_in_segment)| {
                let blob_service = blob_service.clone();
                async move {
                    let segment_len = segment.len();
                    if skip_in_segment > 0 {
                        debug_assert!(
                            segment_len > skip_in_segment as u64,
                            "Snix bug: segment size is smaller than bytes to skip"
                        );
                    }
                    match segment {
                        Segment::Literal(mut data) => futures::stream::once(async move {
                            if skip_in_segment > 0 {
                                data.rotate_left(skip_in_segment);
                                data.truncate(data.len() - skip_in_segment);
                            }
                            Ok::<_, io::Error>(Bytes::from(data))
                        })
                        .boxed(),
                        // skip over empty blobs
                        Segment::BlobRef { size: 0, .. } => futures::stream::empty().boxed(),
                        Segment::BlobRef { digest, size } => {
                            async_stream::try_stream! {
                                let blob_reader = blob_service.open_read(&digest).await
                                    .map_err(io::Error::other)?
                                    .ok_or_else(|| {
                                        io::Error::new(
                                            io::ErrorKind::NotFound,
                                            format!("blob {0} not found", &digest),
                                        )
                                    })?;

                                let bytes_read: AtomicU64 = AtomicU64::new(0);
                                let blob_reader = InspectReader::new(blob_reader, |d| {
                                    bytes_read.fetch_add(d.len() as u64, std::sync::atomic::Ordering::Relaxed);
                                });

                                // discard skip_in_segment bytes from the reader
                                let blob_reader = if skip_in_segment > 0 {
                                    let mut limited_rd = blob_reader.take(skip_in_segment as u64);
                                    tokio::io::copy(
                                        &mut limited_rd,
                                        &mut tokio::io::sink(),
                                    ).await?;

                                    limited_rd.into_inner()
                                } else {
                                    blob_reader
                                };

                                // construct a ReaderStream for the rest
                                let mut stream = ReaderStream::new(blob_reader);
                                while let Some(bytes) = stream.try_next().await? {
                                    // `bytes.len()` has already been added to the `bytes_read` counter.
                                    if (bytes_read.load(std::sync::atomic::Ordering::Relaxed)) > size {
                                        Err(io::Error::new(io::ErrorKind::InvalidData, "got more bytes from BlobReader than expected"))?
                                    }
                                    yield bytes;
                                }

                                drop(stream);

                                if bytes_read.into_inner() < size {
                                    Err(io::Error::new(io::ErrorKind::InvalidData, "got less bytes from BlobReader than expected"))?
                                }
                            }
                            .boxed()
                        }
                    }
                }
            })
            .buffered(segment_concurrency)
            .flatten();

        Box::new(StreamReader::new(bytes_stream))
    }

    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    fn push(&mut self, elem: Segment) {
        let new_total_len = self.total_len() + elem.len();
        self.segments.push((self.total_len(), elem));
        self.total_len = new_total_len;
    }
}

#[derive(Clone, Debug)]
pub enum Segment {
    // Literal bytes, aka 'NAR framing' around blob pointers
    Literal(Vec<u8>),
    // A pointer to a blob, by its digest. Also stores the size, so we can compute the segment length.
    BlobRef {
        digest: B3Digest,
        // FUTUREWORK: maybe drop this and Segment::len(),
        // where we want BlobRef size we can peek at the offset in the next segment.
        size: u64,
    },
}

impl Segment {
    pub fn len(&self) -> u64 {
        match self {
            Segment::Literal(data) => data.len() as u64,
            Segment::BlobRef { size, .. } => *size,
        }
    }
}

/// Used during construction.
/// Recursively walks the node and its children, and pushes new segments.
///
/// The function is infallible, as:
///  - we only write to buffers
///  - all castore `PathComponent` and `SymlinkTarget` can be expressed in NAR
///  - the passed directory closure is complete
fn walk_node(
    segments: &mut Segments,
    directories: &HashMap<B3Digest, Directory>,
    node: &Node,
    // Includes a reference to the current segment's buffer
    nar_node: nar_writer::Node<'_, Vec<u8>>,
) {
    match node {
        snix_castore::Node::Symlink { target } => {
            nar_node
                .symlink(target.as_ref())
                .expect("Snix bug: failed to write symlink as NAR");
        }
        snix_castore::Node::File {
            digest,
            size,
            executable,
        } => {
            let (buf, skip) = nar_node
                .file_manual_write(*executable, *size)
                .expect("Snix bug: failed to write framing before file node as NAR");

            // Flush the segment up until the beginning of the blob
            segments.push(Segment::Literal(std::mem::take(buf)));

            // Insert the blob segment
            segments.push(Segment::BlobRef {
                digest: *digest,
                size: *size,
            });

            // Close the file node
            // We **intentionally** do not write the file contents anywhere.
            // Instead we have stored the blob reference in a Data::Blob segment,
            // and the poll_read implementation will take care of serving the
            // appropriate blob at this offset.
            skip.close(buf)
                .expect("Snix bug: failed to close NAR file node");
        }
        snix_castore::Node::Directory { digest, .. } => {
            let directory = directories
                .get(digest)
                .expect("Snix bug: referenced directory missing from directory closure");

            // start a directory node
            let mut nar_node_directory = nar_node
                .directory()
                .expect("Snix bug: failed to write directory node as NAR");

            // for each node in the directory, create a new entry with its name,
            // and then recurse on that entry.
            for (name, node) in directory.nodes() {
                let child_node = nar_node_directory
                    .entry(name.as_ref())
                    .expect("Snix bug: failed to write NAR entry");

                walk_node(segments, directories, node, child_node);
            }

            // close the directory
            nar_node_directory
                .close()
                .expect("Snix bug: failed to close directory node");
        }
    }
}
