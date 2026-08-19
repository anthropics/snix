use async_stream::try_stream;
use futures::StreamExt;
use futures::{Stream, stream::BoxStream};
use std::collections::{HashMap, HashSet, hash_map};
use tracing::{Span, trace, warn};

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
pub struct RootToLeavesValidator {
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

impl RootToLeavesValidator {
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

    /// Accepts a directory if previously introduced, or returns an error if it's unknown.
    #[tracing::instrument(level = "trace", skip_all, fields(directory.digest = tracing::field::Empty, directory.size = tracing::field::Empty), err)]
    pub fn try_accept(&mut self, directory: &Directory) -> Result<(), OrderingError> {
        assert!(!self.poison, "Snix bug: RootToLeavesValidator poisoned");

        let size = directory.size();
        let digest = directory.digest();

        Span::current()
            .record("directory.digest", format_args!("{}", &digest))
            .record("directory.size", size);

        // Every incoming directory must already have been introduced.
        match self.referenced_directories.get(&digest) {
            Some(s) if *s == size => {
                if !self.pending_directories.remove(&digest) {
                    warn!("directory received multiple times");
                };

                // Introduce children
                self.introduce_children_of(directory);
                Ok(())
            }
            Some(expected) => {
                self.poison = true;
                Err(OrderingError::WrongSize {
                    digest,
                    referenced: *expected,
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
    pub fn finalize(self) -> Result<(), OrderingError> {
        match self.pending_directories.len() {
            0 => Ok(()),
            1 if self.pending_directories.iter().next().unwrap() == &self.root_digest => {
                Err(OrderingError::EmptySet)
            }
            _ => Err(OrderingError::DirectoriesMissing(self.pending_directories)),
        }
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
        let mut validator = RootToLeavesValidator::new_with_root_digest(root_digest);
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

#[derive(Default)]
/// A struct holding state while consuming a sequence of Directories in
/// Leaves-To-Root order.
///
/// Validates that newly accepted directories only reference directories which
/// have already been accepted before, and that the sizes attached alongside the
/// pointers match the actual sizes.
/// Commonly used when _uploading_ a directory closure _to_ a store.
pub struct LeavesToRootValidator {
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

impl LeavesToRootValidator {
    pub fn new() -> Self {
        Self {
            accepted_directories: Default::default(),
            pending_directories: Default::default(),
            #[cfg(debug_assertions)]
            last_inserted_digest: None,
            poison: false,
        }
    }

    /// Accepts a directory if previously introduced, or returns an error if it's unknown.
    #[tracing::instrument(level = "trace", skip_all, fields(directory.digest = tracing::field::Empty, directory.size = tracing::field::Empty), err)]
    pub fn try_accept(&mut self, directory: &Directory) -> Result<(), OrderingError> {
        assert!(!self.poison, "Snix bug: LeavesToRootValidator poisoned");

        Span::current()
            .record("directory.digest", format_args!("{}", &directory.digest()))
            .record("directory.size", directory.size());

        // every directory referenced must already have been seen.
        // Remove them from pending if still in there.
        for (name, node) in directory.nodes() {
            trace!(%name, ?node, "at node");
            if let Node::Directory { digest, size } = node {
                match self.accepted_directories.get(digest) {
                    Some(s) if s == size => {
                        self.pending_directories.remove(digest);
                    }
                    Some(s) => {
                        self.poison = true;
                        Err(OrderingError::WrongSize {
                            digest: digest.to_owned(),
                            referenced: *size,
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
                warn!("directory received multiple times");
            }
            hash_map::Entry::Vacant(entry) => {
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
    pub fn finalize(mut self) -> Result<(), OrderingError> {
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

#[cfg(test)]
mod tests {
    use super::{LeavesToRootValidator, RootToLeavesValidator};
    use crate::directoryservice::Directory;
    use crate::fixtures::{DIRECTORY_A, DIRECTORY_B, DIRECTORY_C, DIRECTORY_D, DIRECTORY_E};
    use futures::TryStreamExt;
    use rstest::rstest;

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
        let mut validator = LeavesToRootValidator::default();
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
        let mut validator = RootToLeavesValidator::new_with_root_digest(root_digest);
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

    #[test]
    /// This initializes a validator with another root than what we try to upload.
    fn root_to_leaves_root_mismatch() {
        let mut validator = RootToLeavesValidator::new_with_root_digest(DIRECTORY_A.digest());

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

        let validated_stream = RootToLeavesValidator::validate_stream(
            root_digest,
            futures::stream::iter(directories_to_upload.iter().map(|d| (*d).to_owned())),
        );

        let validated_directories: Vec<Directory> = validated_stream
            .try_collect()
            .await
            .expect("stream to collect successfully");

        assert_eq!(directories_to_upload, validated_directories);

        RootToLeavesValidator::validate_stream(root_digest, futures::stream::empty())
            .try_collect::<Vec<_>>()
            .await
            .expect_err("an empty stream to fail");
    }
}
