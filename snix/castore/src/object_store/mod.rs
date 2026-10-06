//! Shared helpers for constructing [object_store::ObjectStore].

use object_store::{ObjectStore, ObjectStoreScheme, path::Path};
use url::Url;

#[cfg(feature = "cloud")]
mod aws;

/// Constructs an [ObjectStore] from a URL and additional object store options.
///
/// If the `cloud` feature is enabled and an S3 URL is passed,
/// it uses [aws::setup_aws_object_store] for setup.
/// This code honors the AWS credential chain, contrary to stock [object_store].
///
/// `opts` is the same as what [object_store::parse_url_opts] accepts.
pub(crate) async fn setup_object_store<I, K, V>(
    object_store_url: &Url,
    opts: I,
) -> Result<(Box<dyn ObjectStore>, Path), Box<dyn std::error::Error + Send + Sync>>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Into<String> + AsRef<str>,
{
    let (object_store_scheme, path) = ObjectStoreScheme::parse(object_store_url)?;

    match object_store_scheme {
        #[cfg(feature = "cloud")]
        ObjectStoreScheme::AmazonS3 => {
            // In the AWS case, we only support s3:// URLs.
            if object_store_url.scheme() != "s3" {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "only s3://-style URLs supported",
                )));
            }

            let store = aws::setup_aws_object_store(object_store_url, opts).await?;
            Ok((Box::new(store) as Box<dyn ObjectStore>, path))
        }
        _ => Ok(object_store::parse_url_opts(object_store_url, opts)?),
    }
}
