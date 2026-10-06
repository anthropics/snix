//! Custom AWS logic to configure the AWS [object_store].
//! Upstream doesn't support the AWS credential chain.

use std::sync::{Arc, RwLock};
use tonic::async_trait;

/// Wrapper type implementing [object_store::CredentialProvider]
/// for [aws_credential_types::provider::ProvideCredentials].
#[derive(Debug)]
struct AwsConfigCredentialProvider<T: aws_credential_types::provider::ProvideCredentials> {
    inner: T,

    /// Retrieved credentials, alongside their expiry time, if any.
    credential: std::sync::RwLock<
        Option<(
            Arc<object_store::aws::AwsCredential>,
            Option<std::time::SystemTime>,
        )>,
    >,
}

impl<T> AwsConfigCredentialProvider<T>
where
    T: aws_credential_types::provider::ProvideCredentials,
{
    pub fn new(aws_credential_provider: T) -> Self {
        Self {
            inner: aws_credential_provider,
            credential: RwLock::new(None),
        }
    }

    async fn get_new_creds(
        &self,
    ) -> object_store::Result<(
        Arc<object_store::aws::AwsCredential>,
        Option<std::time::SystemTime>,
    )> {
        let credentials =
            self.inner
                .provide_credentials()
                .await
                .map_err(|err| object_store::Error::Generic {
                    store: "S3",
                    source: Box::new(err),
                })?;

        let object_store_credentials = Arc::new(object_store::aws::AwsCredential {
            key_id: credentials.access_key_id().to_owned(),
            secret_key: credentials.secret_access_key().to_owned(),
            token: credentials.session_token().map(|s| s.to_owned()),
        });
        let expiry = credentials.expiry();

        Ok((object_store_credentials, expiry))
    }
}

#[async_trait]
impl<T> object_store::CredentialProvider for AwsConfigCredentialProvider<T>
where
    T: aws_credential_types::provider::ProvideCredentials,
{
    type Credential = object_store::aws::AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<Self::Credential>> {
        if let Some(credential) = self.credential.read().expect("poisoned").as_ref() {
            match credential.1 {
                // creds expired, renewal below
                Some(expiry) if expiry <= std::time::SystemTime::now() => {}
                // not yet expired, or no expiry
                Some(_) | None => {
                    return Ok(credential.0.clone());
                }
            }
        }

        // get new creds
        let (object_store_credential, expiry) = self.get_new_creds().await?;

        let mut c = self.credential.write().expect("poisoned");
        *c = Some((object_store_credential.clone(), expiry));

        Ok(object_store_credential)
    }
}

/// Returns an AWS object store. Contrary to [object_store::parse_url_opts],
/// this one honors the AWS credential chain. It does only support s3:// URLs.
///
/// For opts, only the following keys are allowed:
///
///  - aws_access_key_id
///  - aws_secret_access_key
///  - aws_region
///  - aws_allow_http
///  - aws_endpoint_url
///  - aws_profile
///  - user_agent
///
/// These keys will take priority over anything discovered via the AWS default
/// credential chain.
pub async fn setup_aws_object_store<I, K, V>(
    url: &url::Url,
    opts: I,
) -> Result<object_store::aws::AmazonS3, Box<dyn std::error::Error + Send + Sync + 'static>>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: Into<String> + AsRef<str>,
{
    let bucket_name = url
        .host_str()
        .ok_or_else(|| Box::new(std::io::Error::other("no bucket name set")))?;

    // The AWS SDK config loader.
    let mut config_loader = aws_config::from_env();

    let mut aws_access_key_id: Option<String> = None;
    let mut aws_secret_access_key: Option<String> = None;
    let mut allow_http = false;
    let mut user_agent = Some(crate::USER_AGENT.to_owned());

    for (k, v) in opts.into_iter() {
        match k.as_ref() {
            "aws_access_key_id" => {
                aws_access_key_id = Some(v.into());
            }
            "aws_secret_access_key" => {
                aws_secret_access_key = Some(v.into());
            }
            "aws_region" => {
                config_loader = config_loader.region(aws_config::Region::new(v.into()));
            }
            "aws_allow_http" => {
                let v = v.as_ref();
                if v == "1" || v == "true" {
                    allow_http = true;
                }
            }
            "aws_endpoint_url" => {
                config_loader = config_loader.endpoint_url(v.into());
            }
            "aws_profile" => {
                config_loader = config_loader.profile_name(v.into());
            }
            "user_agent" => {
                user_agent = Some(v.into());
            }
            _ => {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unexpected param: {}", k.as_ref()),
                )));
            }
        }
    }

    match (aws_access_key_id, aws_secret_access_key) {
        (None, None) => {}
        (None, Some(_)) | (Some(_), None) => {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "specified only one of `aws_access_key_id`, `aws_secret_access_key`, need to specify both or none",
            )));
        }
        (Some(aws_access_key_id), Some(aws_secret_access_key)) => {
            config_loader =
                config_loader.credentials_provider(aws_credential_types::Credentials::new(
                    aws_access_key_id,
                    aws_secret_access_key,
                    None,
                    None,
                    "url-params",
                ));
        }
    }

    // FUTUREWORK: can we split this out to make things more testable?
    let sdk_config = config_loader.load().await;

    let sdk_credentials_provider = sdk_config.credentials_provider().ok_or_else(|| {
        Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "couldn't discover AWS credential provider",
        ))
    })?;

    let mut store_builder = object_store::aws::AmazonS3Builder::new()
        .with_credentials(Arc::new(AwsConfigCredentialProvider::new(
            sdk_credentials_provider,
        )))
        .with_bucket_name(bucket_name)
        .with_allow_http(allow_http)
        .with_client_options({
            let mut client_options = object_store::ClientOptions::new().with_allow_http(allow_http);

            if let Some(user_agent) = user_agent {
                client_options = client_options
                    .with_user_agent(object_store::HeaderValue::from_str(&user_agent)?);
            }

            client_options
        });

    if let Some(region) = sdk_config.region() {
        store_builder = store_builder.with_region(region.to_string());
    }

    if let Some(endpoint_url) = sdk_config.endpoint_url() {
        store_builder = store_builder.with_endpoint(endpoint_url);
    }

    Ok(store_builder.build()?)
}
