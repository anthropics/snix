use async_stream::try_stream;
use futures::StreamExt;
use futures::{Stream, stream::BoxStream};
use std::collections::{HashMap, HashSet, hash_map};
use tracing::{debug, trace};

use super::Directory;
use crate::{B3Digest, Node};

/// Emitted when directories are sent in the wrong order
#[derive(thiserror::Error, Debug, Eq, PartialEq)]
pub enum OrderingError {
    #[error("wrong size for digest {digest}, referenced with {referenced}, but got {actual}")]
    WrongSize {
        digest: B3Digest,
        referenced: u64,
        actual: u64,
    },

    #[error("unknown digest {digest} referenced for {path_component} in parent {parent_digest}")]
    UnknownLTR {
        digest: B3Digest,
        parent_digest: B3Digest,
        path_component: crate::PathComponent,
    },

    #[error("unexpected Directory with digest {0} encountered", directory.digest())]
    Unexpected { directory: Directory },

    #[error("some directories missing")]
    DirectoriesMissing(HashSet<B3Digest>),

    #[error("no directories received")]
    EmptySet,
}

impl From<OrderingError> for crate::directoryservice::Error {
    fn from(value: OrderingError) -> Self {
        Self(Box::new(value))
    }
}

/// Common trait implemented by both [RootToLeaves] and [LeavesToRoot].
pub trait OrderValidator {
    /// Succeeds if the directory is acceptable with the chosen insertion order.
    fn try_accept(&mut self, directory: &Directory) -> Result<(), OrderingError>;
    /// Succeeds if the accepted directories are a full closure.
    fn finalize(self) -> Result<(), OrderingError>;
}

/// A struct holding state while consuming a sequence of Directories in
/// Root-To-Leaves order.
///
/// It allows querying whether a certain digest could be acceptable
/// (to be able to skip parsing entirely if present in serialized form)
///
/// Validates that newly received directories are already referenced from
/// the root via existing directories.
/// It also ensures the actual directory sizes are the same as the sizes
/// communicated previously alongside the pointers.
/// Commonly used when _receiving_ a directory closure _from_ a store.
///
/// Internally keeps a list of digests introduced (pointers in previously
/// received directories), to recognize getting sent unrelated directories,
/// as well as a list of introduced, but not yet received digest (to detect
/// still-missing directories).
pub struct RootToLeaves {
    /// the expected root digest
    root_digest: B3Digest,

    /// references of (directory digest, size) we have seen so far.
    referenced_directories: HashMap<B3Digest, u64>,

    /// Directories we still wait to receive, because we heard about them but
    /// didn't accept them yet.
    /// These consist of the root digest and any subset of `referenced_directories`.
    pending_directories: HashSet<B3Digest>,

    /// tracks whether an error has occurred while trying to accept.
    poison: bool,
}

impl RootToLeaves {
    /// Initialize with an expected root directory
    /// That directory should be sent next.
    pub fn new_with_root_digest(root_digest: B3Digest) -> Self {
        Self {
            root_digest,
            referenced_directories: HashMap::default(),
            pending_directories: HashSet::from_iter([root_digest]),
            poison: false,
        }
    }

    /// Checks a directory digest on whether it's introduced.
    /// Particularly useful when receiving directories in canonical protobuf
    /// encoding, so that directories not connected to the root can be rejected
    /// without parsing.
    ///
    /// After parsing, the directory can be passed to [Self::try_accept]
    /// to add its children to the list of expected digests.
    pub fn would_accept(&self, digest: &B3Digest) -> bool {
        assert!(!self.poison, "Snix bug: RootToLeavesValidator poisoned");
        digest == &self.root_digest || self.referenced_directories.contains_key(digest)
    }

    // Adds each child node to introduced_directories and pending_directories.
    fn introduce_children_of(&mut self, directory: &Directory) {
        for (_name, node) in directory.nodes() {
            if let Node::Directory { digest, size } = node {
                // if there's a pointer to a new directory
                if self
                    .referenced_directories
                    .insert(digest.to_owned(), *size)
                    .is_none()
                {
                    self.pending_directories.insert(digest.to_owned());
                }
            }
        }
    }

    /// This receives a stream of Directories, validating them to be in Root-To-Leaves order.
    /// The expected root digest needs to be passed in.
    /// If the order is correct, they are yielded wrapped in an Ok().
    /// If not, we yield an error.
    pub fn validate_stream<'s, S>(
        root_digest: B3Digest,
        directories: S,
    ) -> BoxStream<'s, Result<Directory, OrderingError>>
    where
        S: Stream<Item = Directory> + Send + 's,
    {
        let mut validator = RootToLeaves::new_with_root_digest(root_digest);
        let mut directories = directories.boxed();

        Box::pin(try_stream! {
            while let Some(directory) = directories.next().await {
                        validator.try_accept(&directory)?;
                        yield directory;
            }
            validator.finalize()?;
        })
    }
}

impl OrderValidator for RootToLeaves {
    /// Accepts a directory if previously introduced, or returns an error if it's unknown.
    #[tracing::instrument(level = "trace", skip_all, fields(directory.digest = %directory.digest(), directory.size = directory.size()), err)]
    fn try_accept(&mut self, directory: &Directory) -> Result<(), OrderingError> {
        assert!(!self.poison, "Snix bug: RootToLeavesValidator poisoned");

        let size = directory.size();
        let digest = directory.digest();

        // Every incoming directory must already have been introduced.
        match self.referenced_directories.get(&digest) {
            #[cfg(feature = "compat-accept-bigger-sizes")]
            Some(size_referenced) if (size..=directory.size_max()).contains(size_referenced) => {
                if !self.pending_directories.remove(&digest) {
                    debug!("directory received multiple times");
                };

                if *size_referenced != size {
                    debug!(directory.size_referenced=%size_referenced, "directory was referenced with a larger size (legacy size calculation)");
                }

                // Introduce children
                self.introduce_children_of(directory);
                Ok(())
            }
            #[cfg(not(feature = "compat-accept-bigger-sizes"))]
            Some(size_referenced) if size == *size_referenced => {
                if !self.pending_directories.remove(&digest) {
                    debug!("directory received multiple times");
                };

                // Introduce children
                self.introduce_children_of(directory);
                Ok(())
            }
            Some(size_referenced) => {
                self.poison = true;
                Err(OrderingError::WrongSize {
                    digest,
                    referenced: *size_referenced,
                    actual: size,
                })
            }
            // The root may be inserted even if's not in self.referenced_directories.
            None if digest == self.root_digest => {
                // Introduce children
                self.introduce_children_of(directory);
                self.pending_directories.remove(&self.root_digest);
                Ok(())
            }
            None => {
                self.poison = true;
                Err(OrderingError::Unexpected {
                    directory: directory.clone(),
                })
            }
        }
    }

    /// Must be called after accepting the last Directory
    /// Ensures there's no more pending directories.
    #[tracing::instrument(level = "trace", skip_all, err)]
    fn finalize(self) -> Result<(), OrderingError> {
        match self.pending_directories.len() {
            0 => Ok(()),
            1 if self.pending_directories.iter().next().unwrap() == &self.root_digest => {
                Err(OrderingError::EmptySet)
            }
            _ => Err(OrderingError::DirectoriesMissing(self.pending_directories)),
        }
    }
}

#[derive(Default)]
/// A struct holding state while consuming a sequence of Directories in
/// Leaves-To-Root order.
///
/// Validates that newly accepted directories only reference directories which
/// have already been accepted before, and that the sizes attached alongside the
/// pointers match the actual sizes.
/// Commonly used when _uploading_ a directory closure _to_ a store.
pub struct LeavesToRoot {
    #[cfg(feature = "compat-accept-bigger-sizes")]
    /// tracks inserted directories, and their sizes observed.
    /// size is tracked as a range, as this could be a closure with legacy sizes.
    accepted_directories: HashMap<B3Digest, std::ops::RangeInclusive<u64>>,

    #[cfg(not(feature = "compat-accept-bigger-sizes"))]
    /// tracks inserted directories, and their sizes observed.
    accepted_directories: HashMap<B3Digest, u64>,

    /// tracks seen directories which are not yet referenced by parents.
    /// (root candidates)
    pending_directories: HashSet<B3Digest>,

    /// Tracks the last received digest
    #[cfg(debug_assertions)]
    last_inserted_digest: Option<B3Digest>,

    /// tracks whether [Self::finalize] has been called,
    /// or an error has occurred while trying to accept.
    poison: bool,
}

impl LeavesToRoot {
    pub fn new() -> Self {
        Self {
            accepted_directories: Default::default(),
            pending_directories: Default::default(),
            #[cfg(debug_assertions)]
            last_inserted_digest: None,
            poison: false,
        }
    }

    /// This receives a stream of Directories, validating them to be in Leaves-To-Root order.
    /// If the order is correct, they are yielded wrapped in an Ok().
    /// If not, we yield an error.
    pub fn validate_stream<'s, S>(directories: S) -> BoxStream<'s, Result<Directory, OrderingError>>
    where
        S: Stream<Item = Directory> + Send + 's,
    {
        let mut directories = directories.boxed();
        let mut validator = Self::new();

        Box::pin(try_stream! {
            while let Some(directory) = directories.next().await {
                validator.try_accept(&directory)?;
                yield directory;
            }

            validator.finalize()?;
        })
    }
}

impl OrderValidator for LeavesToRoot {
    /// Accepts a directory if previously introduced, or returns an error if it's unknown.
    #[tracing::instrument(level = "trace", skip_all, fields(directory.digest = %directory.digest(), directory.size = directory.size()), err)]
    fn try_accept(&mut self, directory: &Directory) -> Result<(), OrderingError> {
        assert!(!self.poison, "Snix bug: LeavesToRootValidator poisoned");

        // every directory referenced must already have been seen.
        // Remove them from pending if still in there.
        for (name, node) in directory.nodes() {
            trace!(%name, ?node, "at node");
            if let Node::Directory {
                digest,
                size: referenced_size,
            } = node
            {
                match self.accepted_directories.get(digest) {
                    #[cfg(feature = "compat-accept-bigger-sizes")]
                    Some(size_range) if size_range.contains(referenced_size) => {
                        let minimal_size = size_range.start();
                        if referenced_size != minimal_size {
                            debug!(
                                directory.size_referenced=%referenced_size,
                                directory.size_referenced_minimal=%minimal_size,
                                "directory was referenced with a larger size (legacy size calculation)"
                            );
                        }
                        self.pending_directories.remove(digest);
                    }
                    #[cfg(not(feature = "compat-accept-bigger-sizes"))]
                    Some(size) if size == referenced_size => {
                        self.pending_directories.remove(digest);
                    }
                    Some(s) => {
                        self.poison = true;
                        Err(OrderingError::WrongSize {
                            digest: digest.to_owned(),
                            referenced: *referenced_size,
                            #[cfg(feature = "compat-accept-bigger-sizes")]
                            actual: *s.start(),
                            #[cfg(not(feature = "compat-accept-bigger-sizes"))]
                            actual: *s,
                        })?
                    }
                    None => {
                        self.poison = true;
                        Err(OrderingError::UnknownLTR {
                            digest: digest.to_owned(),
                            parent_digest: directory.digest(),
                            path_component: name.to_owned(),
                        })?
                    }
                }
            }
        }

        // All elements were checked to only refer to directories previously seen,
        // we can accept the directory, and add it to pending.
        let directory_digest = directory.digest();
        match self.accepted_directories.entry(directory_digest) {
            hash_map::Entry::Occupied(_) => {
                debug!("directory received multiple times");
            }
            hash_map::Entry::Vacant(entry) => {
                #[cfg(feature = "compat-accept-bigger-sizes")]
                entry.insert(directory.size()..=directory.size_max());
                #[cfg(not(feature = "compat-accept-bigger-sizes"))]
                entry.insert(directory.size());

                #[cfg(debug_assertions)]
                {
                    self.last_inserted_digest = Some(directory_digest)
                }
                self.pending_directories.insert(directory_digest);
            }
        }

        Ok(())
    }

    /// Should be called before Drop, to ensure there's no introduced but unsent
    /// directories.
    #[tracing::instrument(level = "trace", skip_all, err)]
    #[allow(unused_mut)]
    fn finalize(mut self) -> Result<(), OrderingError> {
        assert!(!self.poison, "Snix bug: LeavesToRootValidator poisoned");

        if self.accepted_directories.is_empty() {
            return Err(OrderingError::EmptySet);
        }

        // At the end, there may only be one unreferenced directory
        // (which is the root)
        if self.pending_directories.len() != 1 {
            Err(OrderingError::DirectoriesMissing(
                self.pending_directories.clone(),
            ))?
        }
        #[cfg(debug_assertions)]
        {
            let last_inserted_digest = self
                .last_inserted_digest
                .expect("Snix bug: have dangling_directories, but no last_inserted_digest");
            self.pending_directories
                .get(&last_inserted_digest)
                .expect("Snix bug: dangling directory is not last inserted one");
            self.poison = true;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{LeavesToRoot, OrderValidator, RootToLeaves};
    use crate::Node;
    use crate::directoryservice::Directory;
    use crate::fixtures::{
        DIRECTORY_A, DIRECTORY_B, DIRECTORY_C, DIRECTORY_D, DIRECTORY_E, DIRECTORY_WITH_KEEP,
    };
    use futures::TryStreamExt;
    use rstest::rstest;
    use tracing_test::traced_test;

    #[rstest]
    /// Uploading an empty directory should succeed.
    #[case::empty_directory(&[&*DIRECTORY_A], false, false)]
    /// Uploading A, then B (referring to A) should succeed.
    #[case::simple_closure(&[&*DIRECTORY_A, &*DIRECTORY_B], false, false)]
    /// Uploading A, then A, then C (referring to A twice) should succeed.
    /// We pretend to be a dumb client not deduping directories.
    #[case::same_child(&[&*DIRECTORY_A, &*DIRECTORY_A, &*DIRECTORY_C], false, false)]
    /// Uploading A, then C (referring to A twice) should succeed.
    #[case::same_child_dedup(&[&*DIRECTORY_A, &*DIRECTORY_C], false, false)]
    /// Uploading A, then C (referring to A twice), then B (itself referring to A) should fail during close,
    /// as B itself would be left unconnected.
    #[case::unconnected_node(&[&*DIRECTORY_A, &*DIRECTORY_C, &*DIRECTORY_B], false, true)]
    /// Uploading B (referring to A) should fail immediately, because A was never uploaded.
    #[case::dangling_pointer(&[&*DIRECTORY_B], true, false)]
    /// An empty set is disallowed.
    #[case::empty(&[], false, true)]
    fn leaves_to_root(
        #[case] directories_to_upload: &[&Directory],
        #[case] exp_fail_upload_last: bool,
        #[case] exp_fail_finalize: bool,
    ) {
        let mut validator = LeavesToRoot::default();
        let mut it = directories_to_upload.iter().peekable();

        while let Some(d) = it.next() {
            if it.peek().is_none() /* is last */ && exp_fail_upload_last {
                validator
                    .try_accept(d)
                    .expect_err("last try_accept to fail");
            } else {
                assert!(validator.try_accept(d).is_ok(), "try_accept to succeed");
            }
        }

        if !exp_fail_upload_last {
            if !exp_fail_finalize {
                validator.finalize().expect("finalize to succeed");
            } else {
                let _ = validator.finalize();
            }
        }
    }

    #[rstest]
    /// Downloading an empty directory should succeed.
    #[case::empty_directory(&[&*DIRECTORY_A], false)]
    /// Downlading B, then A (referenced by B) should succeed.
    #[case::simple_closure(&[&*DIRECTORY_B, &*DIRECTORY_A], false)]
    /// Downloading C (referring to A twice), then A should succeed.
    #[case::same_child_dedup(&[&*DIRECTORY_C, &*DIRECTORY_A], false)]
    /// Downloading C, then A twice should succeed.
    #[case::same_child_redundant(&[&*DIRECTORY_C, &*DIRECTORY_A, &*DIRECTORY_A], false)]
    /// Downloading C, then A should succeed, even if we receive C twice
    #[case::with_root_sent_twice(&[&*DIRECTORY_C, &*DIRECTORY_C, &*DIRECTORY_A], false)]
    /// Downloading E -> D -> A,B should succeed.
    #[case::more_levels(&[&*DIRECTORY_E, &*DIRECTORY_D, &*DIRECTORY_A, &*DIRECTORY_B], false)]
    /// Downloading C, then B (both referring to A but not referring to each other) should fail immediately as B has no connection to C (the root)
    #[case::unconnected_node(&[&*DIRECTORY_C, &*DIRECTORY_B], true)]
    fn root_to_leaves(
        #[case] directories_to_upload: &[&Directory],
        #[case] exp_fail_upload_last: bool,
    ) {
        let root_digest = directories_to_upload[0].digest();
        let mut validator = RootToLeaves::new_with_root_digest(root_digest);
        let mut it = directories_to_upload.iter().peekable();

        while let Some(d) = it.next() {
            if it.peek().is_none() /* is last */ && exp_fail_upload_last {
                assert!(
                    !validator.would_accept(&d.digest()),
                    "would_accept not expected to accept last failing element"
                );

                validator
                    .try_accept(d)
                    .expect_err("last try_accept to fail");
            } else {
                assert!(
                    validator.would_accept(&d.digest()),
                    "would_accept expected to accept directory"
                );
                assert!(validator.try_accept(d).is_ok(), "try_accept to succeed");
            }
        }

        if !exp_fail_upload_last {
            validator.finalize().expect("finalize to succeed");
        }
    }

    // Producing a list of Directory using legacy sizes.
    // Starts with the root.
    fn legacy_size_dirs() -> Vec<Directory> {
        let a = DIRECTORY_WITH_KEEP.to_owned();
        assert_eq!(a.size(), 1);
        #[cfg(feature = "compat-accept-bigger-sizes")]
        assert_eq!(a.size_max(), 2);

        // For b, we use the size calculation used between cl/31479 and cl/31564.
        let b = crate::Directory::try_from_iter([
            (
                "symlink".try_into().unwrap(),
                Node::Symlink {
                    target: "somewhereelse".try_into().unwrap(),
                },
            ),
            (
                "dir".try_into().unwrap(),
                Node::Directory {
                    digest: DIRECTORY_WITH_KEEP.digest(),
                    size: 1,
                },
            ),
        ])
        .unwrap();

        assert_eq!(3, b.size());
        #[cfg(feature = "compat-accept-bigger-sizes")]
        assert_eq!(5, b.size_max());
        let b_size = 1 + 2 + 1; // 4

        let root = crate::Directory::try_from_iter([
            (
                "a".try_into().unwrap(),
                Node::Directory {
                    digest: a.digest(),
                    // This is a.size_max().
                    size: 2,
                },
            ),
            (
                "b".try_into().unwrap(),
                Node::Directory {
                    digest: b.digest(),
                    size: b_size,
                },
            ),
        ])
        .unwrap();

        vec![root, b, a]
    }

    #[test]
    #[traced_test]
    /// Ensure directories with legacy sizes are still accepted by the RootToLeavesValidator.
    fn root_to_leaves_legacy_size() {
        let dirs = legacy_size_dirs();

        let mut validator = RootToLeaves::new_with_root_digest(dirs[0].digest());
        validator.try_accept(&dirs[0]).expect("to accept root");

        if cfg!(feature = "compat-accept-bigger-sizes") {
            validator.try_accept(&dirs[1]).expect("to accept b");
            validator.try_accept(&dirs[2]).expect("to accept leaf a");
            validator.finalize().expect("to finalize");

            assert!(logs_contain("legacy size calculation"));
        } else {
            validator
                .try_accept(&dirs[1])
                .expect_err("to reject b due to wrong size used in root");
        }
    }

    #[test]
    #[traced_test]
    /// Ensure directories with legacy sizes are still accepted by the LeavesToRootValidator.
    fn leaves_to_root_legacy_size() {
        let dirs = legacy_size_dirs();

        let mut validator = LeavesToRoot::new();
        validator.try_accept(&dirs[2]).expect("to accept leaf a");
        validator.try_accept(&dirs[1]).expect("to accept b");

        if cfg!(feature = "compat-accept-bigger-sizes") {
            validator.try_accept(&dirs[0]).expect("to accept root");
            validator.finalize().expect("to finalize");

            assert!(logs_contain("legacy size calculation"));
        } else {
            validator
                .try_accept(&dirs[0])
                .expect_err("to reject root due to referring to a with wrong size");
        }
    }

    #[test]
    /// Ensures directories referring to sizes < .size are rejected.
    /// This is independent of the `compat-accept-bigger-sizes` feature.
    fn reject_too_small_size() {
        assert_eq!(1, DIRECTORY_B.size());

        // create a root which refers to b with a size < b.size()
        let root = Directory::try_from_iter([(
            "b".try_into().unwrap(),
            Node::Directory {
                digest: DIRECTORY_B.digest(),
                size: 0,
            },
        )])
        .unwrap();

        let mut validator = RootToLeaves::new_with_root_digest(root.digest());
        validator.try_accept(&root).expect("should accept root");
        validator
            .try_accept(&DIRECTORY_B)
            .expect_err("should reject B due to wrong size");

        let mut validator = LeavesToRoot::new();
        validator.try_accept(&DIRECTORY_A).expect("should accept A");
        validator.try_accept(&DIRECTORY_B).expect("should accept B");
        validator
            .try_accept(&root)
            .expect_err("should reject root due to referring by wrong size");
    }

    #[test]
    /// Ensures directories referring to sizes > .size_max are rejected.
    /// This is independent of the `compat-accept-bigger-sizes` feature.
    fn reject_too_big_size() {
        #[cfg(feature = "compat-accept-bigger-sizes")]
        assert_eq!(2, DIRECTORY_B.size_max());

        // create a root which refers to b with a size > b.size_max()
        let root = Directory::try_from_iter([(
            "b".try_into().unwrap(),
            Node::Directory {
                digest: DIRECTORY_B.digest(),
                size: 3,
            },
        )])
        .unwrap();

        let mut validator = RootToLeaves::new_with_root_digest(root.digest());
        validator.try_accept(&root).expect("should accept root");
        validator
            .try_accept(&DIRECTORY_B)
            .expect_err("should reject B due to wrong size");

        let mut validator = LeavesToRoot::new();
        validator.try_accept(&DIRECTORY_A).expect("should accept A");
        validator.try_accept(&DIRECTORY_B).expect("should accept B");
        validator
            .try_accept(&root)
            .expect_err("should reject root due to referring by wrong size");
    }

    #[test]
    /// This initializes a validator with another root than what we try to upload.
    fn root_to_leaves_root_mismatch() {
        let mut validator = RootToLeaves::new_with_root_digest(DIRECTORY_A.digest());

        validator
            .try_accept(&DIRECTORY_B)
            .expect_err("shouldn't accept wrong first directory");
        validator.finalize().expect_err("expect finalize to fail");
    }

    #[tokio::test]
    async fn root_to_leaves_stream() {
        let directories_to_upload = vec![
            DIRECTORY_E.to_owned(),
            DIRECTORY_D.to_owned(),
            DIRECTORY_A.to_owned(),
            DIRECTORY_B.to_owned(),
        ];
        let root_digest = directories_to_upload[0].digest();

        let validated_stream = RootToLeaves::validate_stream(
            root_digest,
            futures::stream::iter(directories_to_upload.iter().map(|d| (*d).to_owned())),
        );

        let validated_directories: Vec<Directory> = validated_stream
            .try_collect()
            .await
            .expect("stream to collect successfully");

        assert_eq!(directories_to_upload, validated_directories);

        RootToLeaves::validate_stream(root_digest, futures::stream::empty())
            .try_collect::<Vec<_>>()
            .await
            .expect_err("an empty stream to fail");
    }
}
