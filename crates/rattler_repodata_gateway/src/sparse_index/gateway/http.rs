use crate::sparse_index::GatewayError;
use futures::{StreamExt, TryStreamExt};
use http::StatusCode;
use http_cache_semantics::CachePolicy;
use rattler_networking::AuthenticatedClient;
use std::path::Path;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::either::Either;
use tokio_util::io::StreamReader;
use url::Url;

#[derive(Error, Debug)]
pub enum HttpError {
    #[error(transparent)]
    Transport(#[from] reqwest::Error),

    #[error(transparent)]
    Cache(#[from] cacache::Error),

    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

impl From<HttpError> for GatewayError {
    fn from(value: HttpError) -> Self {
        match value {
            HttpError::Transport(err) => err.into(),
            HttpError::Cache(err) => err.into(),
            HttpError::IoError(err) => err.into(),
        }
    }
}

/// Performs a get request against the specified `url`. Returns data from the cache if possible.
pub async fn get(
    client: &AuthenticatedClient,
    cache_dir: &Path,
    url: Url,
) -> Result<(StatusCode, impl AsyncBufRead), HttpError> {
    // Try to read the info from the cache
    // if let Some((policy, cached_data)) = get_from_cache(cache_dir, url.clone()).await {
    //
    // }
    fetch_and_cache(client, cache_dir, url).await
}

/// Read any cache entry for the specified `url`. Returns both the cache policy from the last cached
/// request as well as an async reader to read the contents of the cache.
async fn get_from_cache(cache_dir: &Path, url: Url) -> Option<(CachePolicy, impl AsyncBufRead)> {
    // Open the file for reading again
    let reader = cacache::Reader::open(cache_dir, url).await.ok()?;
    let mut buf_reader = BufReader::new(reader);

    // Parse the cache policy from the file
    let cache_policy_len = buf_reader.read_u64().await.ok()?;
    let mut cache_policy_bytes = Vec::new();
    (&mut buf_reader)
        .take(cache_policy_len)
        .read_to_end(&mut cache_policy_bytes)
        .await
        .ok()?;

    Some((bincode::deserialize(&cache_policy_bytes).ok()?, buf_reader))
}

/// Performs a `GET` request on the specified `url`. Caches the result if that is possible according
/// to the status code (must be OK) and the cache policy of the response.
///
/// If the response is cached it is first written to disk and the response object will point to the
/// file on disk instead.
///
/// TODO: In the future we might want to return an object that writes to disk while the data is
///   streamed or something like that.
async fn fetch_and_cache(
    client: &AuthenticatedClient,
    cache_dir: &Path,
    url: Url,
) -> Result<(StatusCode, impl AsyncBufRead), HttpError> {
    let (client, request) = client.get(url.clone()).build_split();
    let request = request?;
    let response = client.execute(request.try_clone().unwrap()).await?;
    let status_code = response.status();

    let cache_policy = CachePolicy::new(&request, &response);
    if status_code == StatusCode::OK && cache_policy.is_storable() {
        // Write the policy and bytes of the stream to a cache file.
        let mut writer = cacache::Writer::create(cache_dir, url).await?;
        let mut cache_policy_bytes = bincode::serialize(&cache_policy).unwrap();
        writer.write_u64(cache_policy_bytes.len() as u64).await?;
        writer.write_all(&cache_policy_bytes).await?;

        let mut bytes = response.bytes_stream();
        while let Some(bytes) = bytes.next().await {
            writer.write_all(&bytes?).await?;
        }

        let integrity = writer.commit().await?;

        // Open the file for reading again
        let reader = cacache::Reader::open_hash(cache_dir, integrity).await?;
        let mut buf_reader = BufReader::new(reader);

        // There is no proper way to seek in this reader, so simply read back the data and ignore
        // it.
        let _len = buf_reader.read_u64().await;
        buf_reader.read_exact(&mut cache_policy_bytes).await?;

        Ok((status_code, Either::Left(buf_reader)))
    } else {
        let bytes = response
            .bytes_stream()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err));
        Ok((status_code, Either::Right(StreamReader::new(bytes))))
    }
}
