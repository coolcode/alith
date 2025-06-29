use super::error::map_serialization_error;
use super::{
    config::ApiConfigTrait,
    error::{ClientError, WrappedError, map_deserialization_error},
};
use bytes::Bytes;
use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, Clone)]
pub struct ApiClient<C: ApiConfigTrait> {
    http_client: reqwest::Client,
    pub config: C,
    pub backoff: backoff::ExponentialBackoff,
}

impl<C: ApiConfigTrait> ApiClient<C> {
    pub fn new(config: C) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            config,
            backoff: backoff::ExponentialBackoffBuilder::new()
                .with_max_elapsed_time(Some(std::time::Duration::from_secs(60)))
                .build(),
        }
    }

    /// Make a POST request to {path} and deserialize the response body
    pub async fn post<I, O>(&self, path: &str, request: I) -> Result<O, ClientError>
    where
        I: Serialize + std::fmt::Debug,
        O: DeserializeOwned,
    {
        // Log the serialized request
        let request_maker = || async {
            let serialized_request =
                serde_json::to_string(&request).map_err(map_serialization_error)?;
            crate::trace!("Serialized post request: {}", serialized_request);
            let request_builder = self
                .http_client
                .post(self.config.url(path))
                .headers(self.config.headers())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(serialized_request);
            Ok(request_builder.build()?)
        };
        self.execute(request_maker).await
    }

    /// Make a GET request to {path} and deserialize the response body
    pub async fn get<O>(&self, path: &str) -> Result<O, ClientError>
    where
        O: DeserializeOwned,
    {
        let request_maker = || async {
            crate::trace!("Get request: {}", path);
            let request_builder = self
                .http_client
                .get(self.config.url(path))
                .headers(self.config.headers());

            // crate::trace!("Serialized post request: {:?}", request_builder); // This will log API keys!
            Ok(request_builder.build()?)
        };
        self.execute(request_maker).await
    }

    /// Execute a HTTP request and retry on rate limit
    ///
    /// request_maker serves one purpose: to be able to create request again
    /// to retry API call after getting rate limited. request_maker is async because
    /// reqwest::multipart::Form is created by async calls to read files for uploads.
    async fn execute_raw<M, Fut>(&self, request_maker: M) -> Result<Bytes, ClientError>
    where
        M: Fn() -> Fut,
        Fut: core::future::Future<Output = Result<reqwest::Request, ClientError>>,
    {
        let client = self.http_client.clone();

        backoff::future::retry(self.backoff.clone(), || async {
            let request = request_maker().await.map_err(backoff::Error::Permanent)?;
            let response = client
                .execute(request)
                .await
                .map_err(ClientError::Reqwest)
                .map_err(backoff::Error::Permanent)?;

            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .map_err(ClientError::Reqwest)
                .map_err(backoff::Error::Permanent)?;

            // Deserialize response body from either error object or actual response object
            if !status.is_success() {
                let wrapped_error: WrappedError = serde_json::from_slice(bytes.as_ref())
                    .map_err(|e| map_deserialization_error(e, bytes.as_ref()))
                    .map_err(backoff::Error::Permanent)?;

                if status.as_u16() == 429
                    // API returns 429 also when:
                    // "You exceeded your current quota, please check your plan and billing details."
                    && wrapped_error.error.r#type != Some("insufficient_quota".to_string())
                {
                    // Rate limited retry...
                    tracing::warn!("Rate limited: {}", wrapped_error.error.message);
                    return Err(backoff::Error::Transient {
                        err: ClientError::ApiError(wrapped_error.error),
                        retry_after: None,
                    });
                } else if status.as_u16() == 503 {
                    return Err(backoff::Error::Transient {
                        err: ClientError::ServiceUnavailable {
                            message: wrapped_error.error.message,
                        },
                        retry_after: None,
                    });
                } else {
                    return Err(backoff::Error::Permanent(ClientError::ApiError(
                        wrapped_error.error,
                    )));
                }
            }

            Ok(bytes)
        })
        .await
    }

    /// Execute a HTTP request and retry on rate limit
    ///
    /// request_maker serves one purpose: to be able to create request again
    /// to retry API call after getting rate limited. request_maker is async because
    /// reqwest::multipart::Form is created by async calls to read files for uploads.
    async fn execute<O, M, Fut>(&self, request_maker: M) -> Result<O, ClientError>
    where
        O: DeserializeOwned,
        M: Fn() -> Fut,
        Fut: core::future::Future<Output = Result<reqwest::Request, ClientError>>,
    {
        let bytes = self.execute_raw(request_maker).await?;

        // Deserialize once into a generic Value
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| map_deserialization_error(e, &bytes))?;

        // Log the pretty-printed JSON
        let pretty_json = serde_json::to_string_pretty(&value).map_err(map_serialization_error)?;
        crate::trace!("Serialized response: {}", pretty_json);

        // Convert the Value into the target type
        let response: O =
            serde_json::from_value(value).map_err(|e| map_deserialization_error(e, &bytes))?;

        Ok(response)
    }
}
