use std::path::PathBuf;

use thiserror::Error;
use url::Url;

use crate::{Channel, Platform, RepoData};

const REPODATA_CHANNEL_PATH: &str = "repodata.json";

/// An error that may occur when trying the fetch repository data.
#[derive(Clone, Error)]
pub enum RequestRepoDataError {
    #[error("error deserializing repository data: {0}")]
    DeserializeError(#[from] serde_json::Error),

    #[error("error downloading data: {0}")]
    TransportError(#[from] reqwest::Error),
}

/// A struct to construct and perform a request to fetch repodata from a certain channel
/// subdirectory.
///
/// TODO: More info
pub struct RequestRepoDataBuilder {
    /// The channel to download from
    channel: Channel,

    /// The platform within the channel (also sometimes called the subdir)
    platform: Platform,

    /// The directory to store the cache
    cache_dir: Option<PathBuf>,

    /// An optional [`reqwest::Client`] that is used to perform the request. When performing
    /// multiple requests its useful to reuse a single client.
    http_client: Option<reqwest::Client>,
}

impl RequestRepoDataBuilder {
    /// Constructs a new builder to request repodata for the given channel and platform.
    pub fn new(channel: Channel, platform: Platform) -> Self {
        Self {
            channel,
            platform,
            cache_dir: None,
            http_client: None,
        }
    }

    /// Sets the directory that will be used for caching requests.
    pub fn set_cache_dir(&mut self, cache_dir: impl Into<PathBuf>) -> &mut self {
        self.cache_dir = Some(cache_dir.into());
        self
    }

    /// Sets the [`reqwest::Client`] that is used to perform HTTP requests. If this is not called
    /// a new client is created for each request. When performing multiple requests its more
    /// efficient to reuse a single client across multiple requests.
    pub fn set_http_client(&mut self, client: reqwest::Client) -> &mut self {
        self.http_client = Some(client);
        self
    }

    /// Consumes self and starts an async request to fetch the repodata.
    pub async fn request(self) -> Result<RepoData, RequestRepoDataError> {
        // Get the url to the subdirectory index. Note that the subdirectory is the platform name.
        let platform_url = self
            .channel
            .platform_url(self.platform)
            .join(REPODATA_CHANNEL_PATH)
            .expect("repodata.json is a valid json path");

        // Download the repodata from the subdirectory url
        let http_client = self.http_client.unwrap_or_else(reqwest::Client::new);
        request_repodata_from_url(platform_url, http_client).await
    }
}

/// Downloads the repodata from the specified Url. The Url must point to a "repodata.json" file.
async fn request_repodata_from_url(
    url: Url,
    client: reqwest::Client,
) -> Result<RepoData, RequestRepoDataError> {
    let response = client
        .get(url)
        // We can handle g-zip encoding which is often used. We could also set this option on the
        // client, but that will disable all download progress messages.
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .send()
        .await?
        .error_for_status()?;

    let is_gzip_encoded = is_response_encoded_with(&response, "gzip");

    
}

/// Returns true if the response is encoded as the specified encoding.
fn is_response_encoded_with(response: &reqwest::Response, encoding_str: &str) -> bool {
    let headers = response.headers();
    headers
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .any(|enc| enc == encoding_str)
        || headers
        .get_all(reqwest::header::TRANSFER_ENCODING)
        .iter()
        .any(|enc| enc == encoding_str)
}

