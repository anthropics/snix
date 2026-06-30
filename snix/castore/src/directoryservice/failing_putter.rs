use crate::{B3Digest, Directory};
pub struct FailingPutter;

#[tonic::async_trait]
impl super::DirectoryPutter for FailingPutter {
    async fn put(&mut self, _directory: Directory) -> Result<(), super::Error> {
        Err(Error::Unimplemented)?
    }
    async fn close(&mut self) -> Result<B3Digest, super::Error> {
        Err(Error::Unimplemented)?
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("puts are unimplemented")]
    Unimplemented,
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}
