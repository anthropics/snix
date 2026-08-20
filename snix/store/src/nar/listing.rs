use futures::TryStreamExt;
use nix_compat::nar::listing::{Listing, ListingEntry, ListingVersion};
use snix_castore::{B3Digest, Directory, Node, directoryservice::DirectoryService};

use snix_castore::directoryservice::order_validator::{self, OrderValidator, RootToLeaves};
use std::{
    collections::{BTreeMap, HashMap},
    sync::atomic::{AtomicU64, Ordering},
};

/// A writer that only counts the number of bytes written so far,
/// updating a &AtomicU64.
/// We can't use `count_write::CountWrite`.
/// As we hand out a &mut Write to the NAR writer, we can't have other read-only
/// references until we drop it, yet we need to be able to access the current
/// offsets to put in the listing scructure we build up at the same time.
struct CountingWriter<'a> {
    bytes_written: &'a AtomicU64,
}

impl std::io::Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes_written
            .fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Accepts a [Node] pointing to the root of a (store) path,
/// and uses the passed [DirectoryService] to expand the entire structure.
/// Then assembles a [Listing].
pub async fn produce_listing<DS>(root_node: &Node, directory_service: &DS) -> Result<Listing, Error>
where
    DS: DirectoryService,
{
    let mut directories: HashMap<B3Digest, Directory> = HashMap::new();

    if let Node::Directory { digest, .. } = root_node {
        let mut directories_stream = directory_service.get_recursive(digest);
        let mut validator = RootToLeaves::new_with_root_digest(digest.to_owned());
        while let Some(directory) = directories_stream.try_next().await? {
            validator.try_accept(&directory)?;
            directories.insert(directory.digest(), directory);
        }
    };

    let bytes_written = AtomicU64::new(0);
    let mut writer = CountingWriter {
        bytes_written: &bytes_written,
    };
    let nar_node = nix_compat::nar::writer::open(&mut writer).expect("writing");

    // bytes_written.fetch_min(20, Ordering::Relaxed);

    Ok(Listing::V1 {
        root: produce_listing_inner(root_node, &bytes_written, nar_node, &|digest| {
            // The lookup is infallible, as we validate the closures, or we never call the function at all.
            directories
                .get(digest)
                .expect("Snix bug: lookup of unknown directory")
        })?,
        version: ListingVersion,
    })
}

fn produce_listing_inner<'d, F>(
    node: &Node,
    bytes_written: &AtomicU64,
    writer_node: nix_compat::nar::writer::Node<'_, impl std::io::Write>,
    get_directory: &'d F,
) -> Result<ListingEntry, Error>
where
    F: Fn(&B3Digest) -> &'d Directory + 'd,
{
    Ok(match node {
        Node::Directory { digest, .. } => {
            let mut writer_entries = writer_node.directory().expect("nar writing");

            let nodes_it = get_directory(digest).nodes();

            let mut child_entries: BTreeMap<String, ListingEntry> = BTreeMap::new();

            for (child_name, child_node) in nodes_it {
                let writer_child_node = writer_entries
                    .entry(child_name.as_ref())
                    .expect("nar writing");
                child_entries.insert(
                    str::from_utf8(child_name.as_ref())
                        .or(Err(Error::PathComponentIsNoString))?
                        .to_owned(),
                    produce_listing_inner(
                        child_node,
                        bytes_written,
                        writer_child_node,
                        get_directory,
                    )?,
                );
            }

            writer_entries.close().expect("nar writing");

            ListingEntry::Directory {
                entries: child_entries,
            }
        }
        Node::File {
            size, executable, ..
        } => {
            let (w, hdl) = writer_node
                .file_manual_write(*executable, *size)
                .expect("nar writing");

            // This is the offset we want to store, before we add to the counter.
            let nar_offset = bytes_written.fetch_add(*size, Ordering::Relaxed);

            hdl.close(w).expect("nar writing");

            ListingEntry::Regular {
                size: *size,
                executable: *executable,
                nar_offset,
            }
        }
        Node::Symlink { target } => {
            writer_node.symlink(target.as_ref()).expect("nar writing");

            ListingEntry::Symlink {
                target: str::from_utf8(target.as_ref())
                    .or(Err(Error::SymlinkTargetIsNoString))?
                    .to_owned(),
            }
        }
    })
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("symlink target is not a string")]
    SymlinkTargetIsNoString,
    #[error("path component is not a string")]
    PathComponentIsNoString,
    #[error("from directoryservice: {0}")]
    DirectoryService(#[from] snix_castore::directoryservice::Error),
    #[error("ordering error from directoryservice: {0}")]
    OrderingError(#[from] order_validator::OrderingError),
}

#[cfg(test)]
mod tests {
    use super::produce_listing;
    use crate::fixtures::{
        CASTORE_NODE_COMPLICATED, CASTORE_NODE_HELLOWORLD, CASTORE_NODE_SYMLINK,
    };
    use rstest::rstest;
    use snix_castore::Directory;
    use snix_castore::fixtures::{DIRECTORY_COMPLICATED, DIRECTORY_WITH_KEEP};
    use snix_castore::{
        Node, directoryservice::DirectoryService, utils::gen_test_directory_service,
    };

    #[tokio::test]
    #[rstest]
    #[case::symlink(&CASTORE_NODE_SYMLINK, vec![],
        r#"{"root":{"target":"/nix/store/somewhereelse","type":"symlink"},"version":1}"#)]
    #[case::blob(&CASTORE_NODE_HELLOWORLD, vec![],
        r#"{"root":{"narOffset":96,"size":12,"type":"regular"},"version":1}"#)]
    #[case::complicated(&CASTORE_NODE_COMPLICATED, vec![&*DIRECTORY_COMPLICATED, &*DIRECTORY_WITH_KEEP],
        r#"{"root":{"entries":{".keep":{"narOffset":232,"size":0,"type":"regular"},"aa":{"target":"/nix/store/somewhereelse","type":"symlink"},"keep":{"entries":{".keep":{"narOffset":760,"size":0,"type":"regular"}},"type":"directory"}},"type":"directory"},"version":1}"#)]
    async fn produce_listing_test(
        #[case] root_node: &Node,
        #[case] directories: Vec<&Directory>,
        #[case] exp_json: &str,
    ) {
        let svc = gen_test_directory_service();
        for directory in directories {
            svc.put(directory.to_owned()).await.expect("must insert");
        }

        let listing = produce_listing(root_node, &svc)
            .await
            .expect("must succeed");
        let actual_json = serde_json::to_string(&listing).expect("must serialize");

        assert_eq!(exp_json, actual_json);
    }
}
