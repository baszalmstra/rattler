use std::any::Any;
use std::fs::OpenOptions;
use std::io;
use std::io::{BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};

use futures::TryStreamExt;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::io::StreamReader;
use url::Url;

use crate::utils::{AsyncEncoding, Encoding};
use crate::{Channel, Platform, RepoData};

const REPODATA_CHANNEL_PATH: &str = "repodata.json";

/// An error that may occur when trying the fetch repository data.
#[derive(Debug, Error)]
pub enum RequestRepoDataError {
    #[error("error deserializing repository data: {0}")]
    DeserializeError(#[from] serde_json::Error),

    #[error("error downloading data: {0}")]
    TransportError(#[from] reqwest::Error),

    #[error("{0}")]
    IoError(#[from] io::Error),

    #[error("unsupported scheme'")]
    UnsupportedScheme,

    #[error("invalid path")]
    InvalidPath,

    #[error("the operation was cancelled")]
    Cancelled,
}

impl From<JoinError> for RequestRepoDataError {
    fn from(err: JoinError) -> Self {
        match err.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => RequestRepoDataError::Cancelled,
        }
    }
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
    pub fn set_cache_dir(&mut self, cache_dir: impl Into<PathBuf>) -> &mut Self {
        self.cache_dir = Some(cache_dir.into());
        self
    }

    /// Sets the [`reqwest::Client`] that is used to perform HTTP requests. If this is not called
    /// a new client is created for each request. When performing multiple requests its more
    /// efficient to reuse a single client across multiple requests.
    pub fn set_http_client(&mut self, client: reqwest::Client) -> &mut Self {
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

        // Check the scheme of the url
        match platform_url.scheme() {
            "https" | "http" => {
                // Download the repodata from the subdirectory url
                let http_client = self.http_client.unwrap_or_else(reqwest::Client::new);
                fetch_repodata_from_url(platform_url, http_client).await
            }
            "file" => {
                let path = platform_url
                    .to_file_path()
                    .map_err(|_| RequestRepoDataError::InvalidPath)?;
                fetch_repodata_from_path(&path).await
            }
            _ => Err(RequestRepoDataError::UnsupportedScheme),
        }
    }
}

/// Downloads the repodata from the specified Url. The Url must point to a "repodata.json" file.
/// This function returns both the parsed repodata as well as a file that contains the original
/// repodata.
async fn fetch_repodata_from_url(
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

    // Determine the length of the response in bytes
    let content_size = response.content_length();

    // Get the request as a stream of bytes.
    let encoding = Encoding::from(&response);
    let bytes_stream = response.bytes_stream();
    let mut decoded_byte_stream =
        StreamReader::new(bytes_stream.map_err(|e| io::Error::new(ErrorKind::Other, e)))
            .decode(encoding);

    // Read the bytes to memory
    let mut data = Vec::with_capacity(content_size.unwrap_or(1_073_741_824) as usize);
    decoded_byte_stream.read_to_end(&mut data).await?;

    // Deserialize
    Ok(tokio::task::spawn_blocking(move || serde_json::from_slice(&data)).await??)
}

/// Read the [`RepoData`] from disk.
async fn fetch_repodata_from_path(path: &Path) -> Result<RepoData, RequestRepoDataError> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut bytes = Vec::new();
    BufReader::new(file).read_to_end(&mut bytes)?;
    Ok(tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes)).await??)
}

#[cfg(test)]
mod test {
    use crate::repo_data::fetch::RequestRepoDataBuilder;
    use crate::utils::simple_channel_server::SimpleChannelServer;
    use crate::{Channel, ChannelConfig, Platform};
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_fetch_http() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let channel_path = manifest_dir.join("resources/channels/conda-forge");

        let server = SimpleChannelServer::new(channel_path);
        let url = server.url().to_string();
        let channel = Channel::from_str(url, &ChannelConfig::default()).unwrap();

        let result = RequestRepoDataBuilder::new(channel, Platform::NoArch)
            .request()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_fetch_file() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let channel_path = manifest_dir.join("resources/channels/conda-forge");
        let channel = Channel::from_str(
            &format!("file://{}", channel_path.display()),
            &ChannelConfig::default(),
        )
        .unwrap();

        let result = RequestRepoDataBuilder::new(channel, Platform::NoArch)
            .request()
            .await
            .unwrap();
    }
}
