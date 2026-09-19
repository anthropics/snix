use futures::{StreamExt, stream::BoxStream};
use tracing::instrument;

use super::BuildService;
use crate::buildservice::{BuildFailure, BuildRequest, BuildUpdate};

#[derive(Default)]
pub struct DummyBuildService {}

impl BuildService for DummyBuildService {
    #[instrument(skip(self))]
    fn do_build(&self, request: BuildRequest) -> BoxStream<'_, BuildUpdate> {
        futures::stream::once(async {
            BuildUpdate::BuildFailure(BuildFailure::Other {
                message: "builds are not supported with DummyBuildService".to_string(),
            })
        })
        .boxed()
    }
}
