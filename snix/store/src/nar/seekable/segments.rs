use nix_compat::nar::writer::sync as nar_writer;
use snix_castore::{B3Digest, Directory, Node};
use std::collections::HashMap;

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

    pub fn segments_with_offsets(&self) -> &[(u64, Segment)] {
        &self.segments
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

#[derive(Debug)]
pub enum Segment {
    // Literal bytes, aka 'NAR framing' around blob pointers
    Literal(Vec<u8>),
    // A pointer to a blob, by its digest. Also stores the size, so we can compute the segment length.
    BlobRef { digest: B3Digest, size: u64 },
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
