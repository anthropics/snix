#![deny(missing_docs)]
//! Produces a graph of [Directory],
//!
//! Use [DirectoryGraphBuilder] with the chosen insertion order,
//! call [DirectoryGraphBuilder::try_insert] to insert a Node.
//! Once the whole closure has been inserted, [DirectoryGraphBuilder::build]
//! can be called to return a [DirectoryGraph].
//!
//! This [DirectoryGraph] can then be drained in Root-To-Leaves or
//! Leaves-To-Root order.

use futures::{StreamExt, TryStreamExt};
use petgraph::{
    graph::{DiGraph, NodeIndex},
    visit::{Bfs, DfsPostOrder, Walker},
};
use std::collections::{HashMap, HashSet, hash_map};
use tracing::{debug, instrument, warn};

use crate::directoryservice::{
    DirectoryService, LeavesToRootValidator, RootToLeavesValidator, order_validator::OrderingError,
};
use crate::{B3Digest, Directory, Node};

/// This represents a full (and validated) graph of [Directory] nodes.
/// It can be constructed using [DirectoryGraphBuilder], and is normally used to
/// receive in one or the other insertion order, validate, and then drain in
/// Leaves-To-Root order.
/// If you just want to validate an order without keeping the results,
/// `RootToLeavesValidator` or `LeavesToRootValidator` can be used.
#[derive(Default)]
pub struct DirectoryGraph {
    // A directed graph, using Directory as node weight.
    // Edges point from parents to children.
    graph: DiGraph<Directory, ()>,

    // Points to the root.
    root_idx: NodeIndex,
}

#[derive(PartialEq, Eq, Debug)]
enum DirectoryOrder {
    /// Start with the root.
    /// Validates that newly received directories are already referenced from
    /// the root via existing directories.
    RootToLeaves,
    /// Each directory may only refer to directories already sent previously.
    LeavesToRoot,
}

impl DirectoryGraph {
    /// Drains the graph, returning node weights in the chosen [DirectoryOrder].
    fn drain(self, order: DirectoryOrder) -> impl Iterator<Item = Directory> {
        let order = match order {
            DirectoryOrder::RootToLeaves => {
                // do a BFS traversal of the graph, starting with the root node
                Bfs::new(&self.graph, self.root_idx)
                    .iter(&self.graph)
                    .collect::<Vec<_>>()
            }
            DirectoryOrder::LeavesToRoot => {
                // do a DFS Post-Order traversal of the graph, starting with the root node
                DfsPostOrder::new(&self.graph, self.root_idx)
                    .iter(&self.graph)
                    .collect::<Vec<_>>()
            }
        };

        let (mut nodes, _edges) = self.graph.into_nodes_edges();
        order
            .into_iter()
            .map(move |i| std::mem::take(&mut nodes[i.index()].weight))
    }

    /// Drains the graph in Leaves-To-Root Order.
    #[instrument(level = "trace", skip_all)]
    pub fn drain_leaves_to_root(self) -> impl Iterator<Item = Directory> {
        self.drain(DirectoryOrder::LeavesToRoot)
    }

    /// Drains the graph in Root-To-Leaves Order.
    #[instrument(level = "trace", skip_all)]
    pub fn drain_root_to_leaves(self) -> impl Iterator<Item = Directory> {
        self.drain(DirectoryOrder::RootToLeaves)
    }

    /// Returns the directory at the root of the graph
    pub fn root(&self) -> &Directory {
        self.graph
            .node_weight(self.root_idx)
            .expect("Snix bug: root not found")
    }
}

/// This allows constructing a [DirectoryGraph].
/// After deciding on the insertion order ([Self::new_leaves_to_root] or
/// [Self::new_root_to_leaves] with the expected root digest passed),
/// different [Directory] can be passed to [Self::try_insert].
/// A [Self::build] consumes the builder, returning a validated [DirectoryGraph],
/// or an error.
/// The resulting [DirectoryGraph] can be used to drain the graph in
/// Leaves-To-Root or Root-To-Leaves order.
///
/// It does do the same checks as `RootToLeavesValidator` and `LeavesToRootValidator`
/// (insertion order, completeness, connectivity, correct sizes referenced).
// NOTE: a child is always smaller than its parent
pub struct DirectoryGraphBuilder {
    /// Stores the order validator for the chosen insertion order.
    order_validator: OrderValidator,

    /// A directed graph, using Directory as node weight.
    /// Edges point from parents to children.
    graph: DiGraph<Directory, ()>,

    /// A lookup table from directory digest to node index.
    /// Used to lookup where to draw edges.
    digest_to_node_idx: HashMap<B3Digest, NodeIndex>,
}

/// Stores the order validator for the chosen [DirectoryOrder]
enum OrderValidator {
    RootToLeaves {
        validator: RootToLeavesValidator,
        /// For each digest, tracks nodes that referred to it.
        /// This is to draw edges after when finalizing.
        referencing_node_idxs: HashMap<B3Digest, HashSet<NodeIndex>>,
    },
    /// In the leaves-to-root case we only need to lookup references of
    /// Directories we already received, so edges can be created
    /// directly.
    LeavesToRoot(LeavesToRootValidator),
}

impl DirectoryGraphBuilder {
    /// Constructs a new [DirectoryGraphBuilder] accepting directories in
    /// Leaves-To-Root order.
    pub fn new_leaves_to_root() -> Self {
        Self {
            order_validator: OrderValidator::LeavesToRoot(LeavesToRootValidator::default()),
            graph: Default::default(),
            digest_to_node_idx: Default::default(),
        }
    }

    /// Constructs a new [DirectoryGraphBuilder] accepting directories in
    /// Root-To-Leaves order.
    /// The expected root Directory needs to be passed as an argument,
    /// and is validated to match the one inserted on the first call to
    /// [Self::try_insert].
    pub fn new_root_to_leaves(root_digest: B3Digest) -> Self {
        Self {
            order_validator: OrderValidator::RootToLeaves {
                validator: RootToLeavesValidator::new_with_root_digest(root_digest),
                referencing_node_idxs: Default::default(),
            },
            graph: Default::default(),
            digest_to_node_idx: Default::default(),
        }
    }

    /// Accepts a directory if previously introduced, or returns an error if it's unknown.
    #[instrument(level = "trace", skip_all, fields(directory.digest = %directory.digest(), directory.size = directory.size()), err)]
    pub fn try_insert(&mut self, directory: Directory) -> Result<(), OrderingError> {
        // If the directory is already in the graph, we don't actually need to pass it by the validator.
        let entry = match self.digest_to_node_idx.entry(directory.digest()) {
            hash_map::Entry::Occupied(_) => {
                debug!("directory received multiple times");
                return Ok(());
            }
            hash_map::Entry::Vacant(vacant_entry) => vacant_entry,
        };

        // Collect a list of referenced directory digests.
        let referenced_digests = directory
            .nodes()
            .filter_map(|(_, n)| match n {
                Node::Directory { digest, .. } => Some(digest.to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();

        match &mut self.order_validator {
            OrderValidator::RootToLeaves {
                validator,
                referencing_node_idxs,
            } => {
                validator.try_accept(&directory)?;

                // Insert node
                let node_idx = self.graph.add_node(directory);
                entry.insert_entry(node_idx);

                // Insert into referencing_node_idxs
                for referenced_digest in referenced_digests {
                    referencing_node_idxs
                        .entry(referenced_digest)
                        .or_default()
                        .insert(node_idx);
                }
            }
            OrderValidator::LeavesToRoot(validator) => {
                validator.try_accept(&directory)?;

                // Insert node
                let node_idx = self.graph.add_node(directory);
                entry.insert_entry(node_idx);

                // draw edges
                for referenced_directory_digest in referenced_digests {
                    self.graph.add_edge(node_idx, *self.digest_to_node_idx.get(&referenced_directory_digest).expect("Snix bug: referenced directory digest not found in digest_to_node_idx"), ());
                }
            }
        }

        Ok(())
    }

    /// Ensures there's no more directories missing, returns the validated [DirectoryGraph].
    pub fn build(mut self) -> Result<DirectoryGraph, OrderingError> {
        match self.order_validator {
            OrderValidator::RootToLeaves {
                validator,
                referencing_node_idxs,
            } => {
                validator.finalize()?;

                // draw edges with info from referencing_node_idxs
                for (directory_to_digest, directories_from) in referencing_node_idxs {
                    for directory_from in directories_from.iter() {
                        self.graph.add_edge(
                            *directory_from,
                            *self
                                .digest_to_node_idx
                                .get(&directory_to_digest)
                                .expect("Snix bug: digest not found in digest_to_node_idx"),
                            (),
                        );
                    }
                }
                Ok(DirectoryGraph {
                    graph: self.graph,
                    // 1. petgraph invariant: adding nodes or edges does not alter indices
                    // 2. DirectoryGraph RTL invariant: we only add nodes and edges
                    // 3. petgraph invariant: nodes are compactly numbered [0, n)
                    // 4. DirectoryGraph RTL invariant: the root is inserted first
                    // ∴ the root node is always index 0
                    root_idx: NodeIndex::new(0),
                })
            }
            OrderValidator::LeavesToRoot(leaves_to_root_validator) => {
                leaves_to_root_validator.finalize()?;
                let incomings = self.graph.externals(petgraph::Incoming).collect::<Vec<_>>();

                // NOTE: We already know there's only one incomings, else the validator would not have validated
                assert_eq!(1, incomings.len(), "Snix bug: There must be 1 incomings");

                Ok(DirectoryGraph {
                    graph: self.graph,
                    root_idx: incomings[0],
                })
            }
        }
    }
}

/// Extension trait to get a [DirectoryGraph] from a [DirectoryService], and insert into it.
#[tonic::async_trait]
pub trait DirectoryServiceGraphExt {
    /// Queries the [DirectoryService] for the [DirectoryGraph] with the given root digest.
    async fn get_directory_graph(
        &self,
        digest: &B3Digest,
    ) -> Result<Option<DirectoryGraph>, super::Error>;

    /// Inserts the given [DirectoryGraph] into the [DirectoryService].
    async fn put_directory_graph(
        &self,
        directory_graph: DirectoryGraph,
    ) -> Result<B3Digest, super::Error>;

    // FUTUREWORK: get_recursive_validated to get a stream wrapped with order validator?
}

#[tonic::async_trait]
impl<T> DirectoryServiceGraphExt for T
where
    T: DirectoryService,
{
    /// Queries the DirectoryService for the directory graph with the given root digest.
    async fn get_directory_graph(
        &self,
        digest: &B3Digest,
    ) -> Result<Option<DirectoryGraph>, super::Error> {
        let mut builder = DirectoryGraphBuilder::new_root_to_leaves(digest.to_owned());
        let mut directories = std::pin::pin!(self.get_recursive(digest).peekable());

        if directories.as_mut().peek().await.is_none() {
            return Ok(None);
        }

        while let Some(directory) = directories.try_next().await? {
            builder.try_insert(directory)?;
        }

        Ok(Some(builder.build()?))
    }

    /// Inserts the given [DirectoryGraph] into the service.
    async fn put_directory_graph(
        &self,
        directory_graph: DirectoryGraph,
    ) -> Result<B3Digest, super::Error> {
        let mut putter = self.put_multiple_start();

        for directory in directory_graph.drain_leaves_to_root() {
            putter.put(directory).await?;
        }

        Ok(putter.close().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::DirectoryOrder;
    use crate::directoryservice::directory_graph::DirectoryGraphBuilder;
    use crate::fixtures::{DIRECTORY_A, DIRECTORY_B, DIRECTORY_C};
    use crate::{Directory, Node};
    use rstest::rstest;
    use std::sync::LazyLock;

    pub static BROKEN_PARENT_DIRECTORY: LazyLock<Directory> = LazyLock::new(|| {
        Directory::try_from_iter([(
            "foo".try_into().unwrap(),
            Node::Directory {
                digest: DIRECTORY_A.digest(),
                size: DIRECTORY_A.size() + 42, // wrong!
            },
        )])
        .unwrap()
    });

    #[rstest]
    /// Uploading no directories at all should fail, the empty graph is invalid.
    #[case::ltr_empty_graph(DirectoryOrder::LeavesToRoot, &[], false, None)]
    /// Uploading an empty directory should succeed.
    #[case::ltr_empty_directory(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A]))]
    /// Uploading A, then B (referring to A) should succeed.
    #[case::ltr_simple_closure(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_A, &*DIRECTORY_B], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_B]))]
    /// Uploading A, then A, then C (referring to A twice) should succeed.
    /// We pretend to be a dumb client not deduping directories.
    #[case::ltr_same_child(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_A, &*DIRECTORY_A, &*DIRECTORY_C], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_C]))]
    /// Uploading A, then C (referring to A twice) should succeed.
    #[case::ltr_same_child_dedup(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_A, &*DIRECTORY_C], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_C]))]
    /// Uploading A, then C (referring to A twice), then B (itself referring to A) should fail during close,
    /// as B itself would be left unconnected.
    #[case::ltr_unconnected_node(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_A, &*DIRECTORY_C, &*DIRECTORY_B], false, None)]
    /// Uploading B (referring to A) should fail immediately, because A was never uploaded.
    #[case::ltr_dangling_pointer(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_B], true, None)]
    /// Uploading a directory which refers to another Directory with a wrong size should fail.
    #[case::ltr_wrong_size_in_parent(DirectoryOrder::LeavesToRoot, &[&*DIRECTORY_A, &*BROKEN_PARENT_DIRECTORY], true, None)]

    /// Downloading an empty directory should succeed.
    #[case::rtl_empty_directory(DirectoryOrder::RootToLeaves, &[&*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A]))]
    /// Downlading B, then A (referenced by B) should succeed.
    #[case::rtl_simple_closure(DirectoryOrder::RootToLeaves, &[&*DIRECTORY_B, &*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_B]))]
    /// Downloading C (referring to A twice), then A should succeed.
    #[case::rtl_same_child_dedup(DirectoryOrder::RootToLeaves, &[&*DIRECTORY_C, &*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_C]))]
    /// Downloading C, then B (both referring to A but not referring to each other) should fail immediately as B has no connection to C (the root)
    #[case::rtl_unconnected_node(DirectoryOrder::RootToLeaves, &[&*DIRECTORY_C, &*DIRECTORY_B], true, None)]
    /// Downloading a directory which refers to another Directory with a wrong size should fail.
    #[case::rtl_wrong_size_in_parent(DirectoryOrder::RootToLeaves, &[&*BROKEN_PARENT_DIRECTORY, &*DIRECTORY_A], true, None)]
    fn directory_graph(
        #[case] insertion_order: DirectoryOrder,
        #[case] directories_to_upload: &[&Directory],
        #[case] exp_fail_upload_last: bool,
        #[case] exp_build: Option<Vec<&Directory>>, // Some(_) if finalize successful, None if not.
    ) {
        let mut it = directories_to_upload.iter().peekable();

        let mut builder = match insertion_order {
            // in the RTL case, pull the first element from directories_to_upload and initialize with it
            DirectoryOrder::RootToLeaves => DirectoryGraphBuilder::new_root_to_leaves(
                it.peek()
                    .expect("directories_to_upload to not be empty")
                    .digest(),
            ),
            DirectoryOrder::LeavesToRoot => DirectoryGraphBuilder::new_leaves_to_root(),
        };

        while let Some(d) = it.next() {
            if it.peek().is_none() /* is last */ && exp_fail_upload_last {
                builder
                    .try_insert((*d).to_owned())
                    .expect_err("last insert to fail");
            } else {
                builder
                    .try_insert((*d).to_owned())
                    .expect("insert to succeed");
            }
        }

        if exp_fail_upload_last {
            return;
        }

        if let Some(exp_drain_ltr) = exp_build {
            let directory_graph = builder.build().expect("build to succeed");

            // drain
            let drained_ltr = directory_graph.drain_leaves_to_root().collect::<Vec<_>>();

            assert_eq!(
                exp_drain_ltr
                    .iter()
                    .map(|d| (*d).to_owned())
                    .collect::<Vec<_>>(),
                drained_ltr
            );
        } else {
            assert!(builder.build().is_err(), "expected build to fail");
        }
    }

    #[test]
    /// Inserting a first directory into [DirectoryGraphBuilder] that has a
    /// different digest than what was specified in `new_root_to_leaves` should fail.
    fn rtl_wrong_digest() {
        let mut builder = DirectoryGraphBuilder::new_root_to_leaves(DIRECTORY_B.digest());
        builder
            .try_insert(DIRECTORY_A.clone())
            .expect_err("expect insert of root with wrong digest to fail");
    }
}
