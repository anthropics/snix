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
use std::collections::HashMap;
use tracing::{instrument, warn};

use crate::directoryservice::{
    DirectoryService,
    order_validator::{self, LeavesToRoot, OrderValidator, RootToLeaves},
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
enum DrainOrder {
    /// Start with the root.
    /// Validates that newly received directories are already referenced from
    /// the root via existing directories.
    RootToLeaves,
    /// Each directory may only refer to directories already sent previously.
    LeavesToRoot,
}

impl DirectoryGraph {
    /// Drains the graph, returning node weights in the chosen [DrainOrder].
    fn drain(self, order: DrainOrder) -> impl Iterator<Item = Directory> {
        let order = match order {
            DrainOrder::RootToLeaves => {
                // do a BFS traversal of the graph, starting with the root node
                Bfs::new(&self.graph, self.root_idx)
                    .iter(&self.graph)
                    .collect::<Vec<_>>()
            }
            DrainOrder::LeavesToRoot => {
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
        self.drain(DrainOrder::LeavesToRoot)
    }

    /// Drains the graph in Root-To-Leaves Order.
    #[instrument(level = "trace", skip_all)]
    pub fn drain_root_to_leaves(self) -> impl Iterator<Item = Directory> {
        self.drain(DrainOrder::RootToLeaves)
    }

    /// Returns the directory at the root of the graph
    pub fn root(&self) -> &Directory {
        self.graph
            .node_weight(self.root_idx)
            .expect("Snix bug: root not found")
    }
}

/// Constructs a [DirectoryGraph] with a chosen insertion order.
///
/// After deciding on the insertion order (OV generic), and calling
/// new (wants the expected root digest in the Root-To-Leaves case),
/// different [Directory] can be passed to [Self::try_insert].
/// A [Self::build] consumes the builder, returning a validated [DirectoryGraph],
/// or an error.
/// The resulting [DirectoryGraph] can be used to drain the graph in
/// Leaves-To-Root or Root-To-Leaves order.
pub struct DirectoryGraphBuilder<OV> {
    /// Stores the order validator for the chosen insertion order.
    order_validator: OV,

    /// A directed graph, using Directory as node weight.
    /// Directories are Options to allow drawing edges
    /// to not-yet-received Directories in Root-To-Leaves order.
    /// Edges point from parents to children.
    graph: DiGraph<Option<Directory>, ()>,

    /// A lookup table from directory digest to node index.
    /// Used to lookup where to draw edges.
    digest_to_node_idx: HashMap<B3Digest, NodeIndex>,
}

impl DirectoryGraphBuilder<LeavesToRoot> {
    /// Constructs a new [DirectoryGraphBuilder] accepting directories in
    /// Leaves-To-Root order.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for DirectoryGraphBuilder<LeavesToRoot> {
    fn default() -> Self {
        Self {
            order_validator: Default::default(),
            graph: Default::default(),
            digest_to_node_idx: Default::default(),
        }
    }
}

impl DirectoryGraphBuilder<RootToLeaves> {
    /// Constructs a new [DirectoryGraphBuilder] accepting directories in
    /// Root-To-Leaves order.
    /// The expected root Directory needs to be passed as an argument,
    /// and is validated to match the one inserted on the first call to
    /// [Self::try_insert].
    pub fn new(root_digest: B3Digest) -> Self {
        Self {
            order_validator: RootToLeaves::new_with_root_digest(root_digest),
            graph: Default::default(),
            digest_to_node_idx: Default::default(),
        }
    }
}

impl<OV> DirectoryGraphBuilder<OV>
where
    OV: OrderValidator,
{
    /// Accepts a directory if previously introduced, or returns an error if it's unknown.
    #[instrument(level = "trace", skip_all, fields(directory.digest = %directory.digest(), directory.size = directory.size()), err)]
    pub fn try_insert(
        &mut self,
        directory: Directory,
    ) -> Result<(), order_validator::OrderingError> {
        // Validates ordering and sizes.
        self.order_validator.try_accept(&directory)?;

        // Ensure we have a NodeIndex for the directory we try to insert
        let self_ix = *self
            .digest_to_node_idx
            .entry(directory.digest())
            .or_insert_with(|| self.graph.add_node(None));

        // If the directory is already in the graph, we don't actually need to pass it by the validator.
        // The order validator already complained about receiving multiple times,
        // so we don't debug!() here again.
        if self.graph[self_ix].is_some() {
            return Ok(());
        }

        // Everything below happens only once for each Directory.

        // draw edges.
        for (_, node) in directory.nodes() {
            let Node::Directory {
                digest: refereced_digest,
                ..
            } = node
            else {
                continue;
            };

            let referenced_ix = *self
                .digest_to_node_idx
                .entry(*refereced_digest)
                .or_insert_with(|| {
                    // NOTE: this only needs to ever populate a None in the Root-To-Leaves case,
                    // but we can rely on the order validator to reject this.
                    self.graph.add_node(None)
                });

            self.graph.add_edge(self_ix, referenced_ix, ());
        }

        // Insert node into the graph.
        self.graph[self_ix] = Some(directory);

        Ok(())
    }

    /// Ensures there's no more directories missing, returns the validated [DirectoryGraph].
    pub fn build(self) -> Result<DirectoryGraph, order_validator::OrderingError> {
        self.order_validator.finalize()?;

        // Construct the final graph which no longer has Option<> around Directory.
        let graph: DiGraph<Directory, ()> = self.graph.map_owned(
            |_ix, mut n| n.take().expect("Snix bug: no pending directories"),
            |_ix, e| e,
        );

        // NOTE: We already know there's only one incomings, else the validator would not have validated
        let mut incomings = graph.externals(petgraph::Incoming);
        let root_idx = incomings
            .next()
            .expect("Snix bug: There must be 1 incoming external");
        debug_assert!(
            incomings.next().is_none(),
            "Snix bug: There must be 1 incoming external"
        );
        Ok(DirectoryGraph { graph, root_idx })
    }
}

#[cfg(feature = "compat-accept-bigger-sizes")]
impl DirectoryGraph {
    /// Returns a new [DirectoryGraph] for which all sizes have been recomputed.
    /// If there's any change, it'll cause referencing Directories to also have
    /// different digests.
    /// Data migration code to remove size calculation introduced in cl/12216 and cl/31479.
    pub fn with_recalculated_sizes(self) -> Self {
        /// Traverses the [Directory], assembling a new [Directory]
        /// while recursing for each [Node::Directory].
        /// Inserts it to `new_closure`, then returns a [crate::Node] which
        /// contains the (possibly updated) digest and size.
        fn fix_sizes_recursive(
            directory: &Directory,
            source: &HashMap<B3Digest, crate::Directory>,
            new_closure: &mut DirectoryGraphBuilder<LeavesToRoot>,
        ) -> crate::Node {
            let new_dir = Directory::try_from_iter(directory.nodes().map(|(path, node)| {
                (
                    path.to_owned(),
                    if let Node::Directory {
                        digest,
                        size: _size,
                    } = node
                    {
                        fix_sizes_recursive(
                            source
                                .get(digest)
                                .expect("Snix bug: digest not found in source"),
                            source,
                            new_closure,
                        )
                    } else {
                        node.to_owned()
                    },
                )
            }))
            .expect("Snix bug: constructed invalid directory");

            let new_digest = new_dir.digest();
            let new_size = new_dir.size();

            new_closure
                .try_insert(new_dir)
                .expect("Snix bug: rewriting produced invalid closure");

            crate::Node::Directory {
                digest: new_digest,
                size: new_size,
            }
        }

        let root_digest = self.root().digest();

        let directories = HashMap::from_iter(
            self.drain_leaves_to_root()
                .map(|directory| (directory.digest(), directory)),
        );

        let mut new_graph = DirectoryGraphBuilder::<LeavesToRoot>::new();

        fix_sizes_recursive(
            directories
                .get(&root_digest)
                .expect("Snix bug: root digest not found"),
            &directories,
            &mut new_graph,
        );

        new_graph
            .build()
            .expect("Snix bug: rewriting produced invalid closure")
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
        let mut builder = DirectoryGraphBuilder::<RootToLeaves>::new(digest.to_owned());
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
    use crate::directoryservice::directory_graph::DirectoryGraphBuilder;
    use crate::directoryservice::order_validator::{LeavesToRoot, OrderValidator, RootToLeaves};
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
    #[case::ltr_empty_graph(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[], false, None)]
    /// Uploading an empty directory should succeed.
    #[case::ltr_empty_directory(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A]))]
    /// Uploading A, then B (referring to A) should succeed.
    #[case::ltr_simple_closure(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_A, &*DIRECTORY_B], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_B]))]
    /// Uploading A, then A, then C (referring to A twice) should succeed.
    /// We pretend to be a dumb client not deduping directories.
    #[case::ltr_same_child(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_A, &*DIRECTORY_A, &*DIRECTORY_C], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_C]))]
    /// Uploading A, then C (referring to A twice) should succeed.
    #[case::ltr_same_child_dedup(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_A, &*DIRECTORY_C], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_C]))]
    /// Uploading A, then C (referring to A twice), then B (itself referring to A) should fail during close,
    /// as B itself would be left unconnected.
    #[case::ltr_unconnected_node(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_A, &*DIRECTORY_C, &*DIRECTORY_B], false, None)]
    /// Uploading B (referring to A) should fail immediately, because A was never uploaded.
    #[case::ltr_dangling_pointer(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_B], true, None)]
    /// Uploading a directory which refers to another Directory with a wrong size should fail.
    #[case::ltr_wrong_size_in_parent(DirectoryGraphBuilder::<LeavesToRoot>::new(), &[&*DIRECTORY_A, &*BROKEN_PARENT_DIRECTORY], true, None)]

    /// Downloading an empty directory should succeed.
    #[case::rtl_empty_directory(DirectoryGraphBuilder::<RootToLeaves>::new(DIRECTORY_A.digest()), &[&*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A]))]
    /// Downlading B, then A (referenced by B) should succeed.
    #[case::rtl_simple_closure(DirectoryGraphBuilder::<RootToLeaves>::new(DIRECTORY_B.digest()), &[&*DIRECTORY_B, &*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_B]))]
    /// Downloading C (referring to A twice), then A should succeed.
    #[case::rtl_same_child_dedup(DirectoryGraphBuilder::<RootToLeaves>::new(DIRECTORY_C.digest()), &[&*DIRECTORY_C, &*DIRECTORY_A], false, Some(vec![&*DIRECTORY_A, &*DIRECTORY_C]))]
    /// Downloading C, then B (both referring to A but not referring to each other) should fail immediately as B has no connection to C (the root)
    #[case::rtl_unconnected_node(DirectoryGraphBuilder::<RootToLeaves>::new(DIRECTORY_C.digest()), &[&*DIRECTORY_C, &*DIRECTORY_B], true, None)]
    /// Downloading a directory which refers to another Directory with a wrong size should fail.
    #[case::rtl_wrong_size_in_parent(DirectoryGraphBuilder::<RootToLeaves>::new(BROKEN_PARENT_DIRECTORY.digest()), &[&*BROKEN_PARENT_DIRECTORY, &*DIRECTORY_A], true, None)]
    fn directory_graph(
        #[case] mut builder: DirectoryGraphBuilder<impl OrderValidator>,
        #[case] directories_to_upload: &[&Directory],
        #[case] exp_fail_upload_last: bool,
        #[case] exp_build: Option<Vec<&Directory>>, // Some(_) if finalize successful, None if not.
    ) {
        let mut it = directories_to_upload.iter().peekable();
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
        let mut builder = DirectoryGraphBuilder::<RootToLeaves>::new(DIRECTORY_B.digest());
        builder
            .try_insert(DIRECTORY_A.clone())
            .expect_err("expect insert of root with wrong digest to fail");
    }
}
