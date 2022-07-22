use std::io;
use std::io::{ErrorKind, Seek, SeekFrom};
use std::path::PathBuf;

use async_compression::tokio::bufread::GzipDecoder;
use futures::{AsyncReadExt, TryFutureExt, TryStreamExt};
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio_util::io::StreamReader;
use url::Url;

use crate::{Channel, Platform, RepoData};

const REPODATA_CHANNEL_PATH: &str = "repodata.json";

/// An error that may occur when trying the fetch repository data.
#[derive(Debug, Error)]
pub enum RequestRepoDataError {
    #[error("error deserializing repository data: {0}")]
    DeserializeError(#[from] serde_json::Error),

    #[error("error downloading data: {0}")]
    TransportError(#[from] reqwest::Error),

    #[error("unable to create temporary file: {0}")]
    CreateTemporaryFileError(io::Error),

    #[error("{0}")]
    IoError(#[from] io::Error),
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

        // Download the repodata from the subdirectory url
        let http_client = self.http_client.unwrap_or_else(reqwest::Client::new);
        Ok(request_repodata_from_url(platform_url, http_client)
            .await?
            .0)
    }
}

/// Downloads the repodata from the specified Url. The Url must point to a "repodata.json" file.
/// This function returns both the parsed repodata as well as a file that contains the original
/// repodata.
async fn request_repodata_from_url(
    url: Url,
    client: reqwest::Client,
) -> Result<(RepoData, NamedTempFile), RequestRepoDataError> {
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

    // Determine if the contents is gzip encoded or not
    let is_gzip_encoded = is_response_encoded_with(&response, "gzip");

    // Get the request as a stream of bytes.
    let byte_stream = response
        .bytes_stream()
        .map_err(|e| std::io::Error::new(ErrorKind::Other, e));
    let mut byte_stream_reader = StreamReader::new(byte_stream);

    // Construct a file to store the data to.
    let temp_file = NamedTempFile::new().map_err(RequestRepoDataError::CreateTemporaryFileError)?;

    // Decode the stream if the stream is compressed and write the result to the file
    {
        let mut async_file = tokio::fs::File::from_std(temp_file.as_file().try_clone()?);
        if is_gzip_encoded {
            let mut decoder = GzipDecoder::new(byte_stream_reader);
            tokio::io::copy(&mut decoder, &mut async_file).await?;
        } else {
            tokio::io::copy(&mut byte_stream_reader, &mut async_file).await?;
        }
    }

    // Re-use the same file handle to read the data as json
    let mut file = temp_file.as_file();
    file.seek(SeekFrom::Start(0))?;

    // Finally read the data to json
    let repo_data: Value = serde_json::from_reader(std::io::BufReader::new(file))?;

    Ok((serde_json::value::from_value(repo_data)?, temp_file))
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

#[cfg(test)]
mod test {
    use crate::repo_data::fetch::RequestRepoDataBuilder;
    use crate::{Channel, ChannelConfig, Platform};

    #[tokio::test]
    async fn test_fetch() {
        let channel = Channel::from_str("conda-forge", &ChannelConfig::default())
            .expect("should be possible");

        let result = RequestRepoDataBuilder::new(channel, Platform::Linux64)
            .request()
            .await
            .unwrap();

        dbg!(result);
    }
}
