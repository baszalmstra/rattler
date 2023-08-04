use super::GatewayError;
use bytes::Bytes;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use http::{HeaderMap, StatusCode};
use http_cache_semantics::{BeforeRequest, CachePolicy};
use rattler_conda_types::RepoDataRecord;
use rattler_networking::AuthenticatedClient;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::{io::AsyncWriteExt, sync::broadcast, try_join};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::io::StreamReader;
use url::Url;

pub async fn remote_fetch(
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel_name: Arc<str>,
    platform_url: Url,
    index_url: Url,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    // Try to read from the cache
    match cacache::Reader::open(&cache_dir, index_url.clone()).await {
        Ok(cache_entry) => {
            read_records_from_cache(
                cache_entry,
                client.clone(),
                cache_dir.clone(),
                channel_name.clone(),
                platform_url.clone(),
                index_url.clone(),
            )
            .await
        }
        Err(cacache::Error::EntryNotFound(_, _)) => {
            // println!("cached MISS for {}", &index_url);
            fetch_and_cache(
                client,
                cache_dir,
                channel_name,
                platform_url,
                index_url,
                Default::default(),
            )
            .await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn read_records_from_cache(
    reader: cacache::Reader,
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel_name: Arc<str>,
    platform_url: Url,
    index_url: Url,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    let mut reader = BufReader::new(reader);

    // Read the cache line from the cached file and convert to a cache policy
    let mut cache_line = String::new();
    reader.read_line(&mut cache_line).await?;
    let cache_policy: CachePolicy =
        serde_json::from_str(&cache_line).map_err(|_| GatewayError::EncodingError)?;

    // Construct the request we would fire off and determine what course of action to take based on
    // the previously cached value.
    let req = client.get(index_url.clone()).build().unwrap();
    match cache_policy.before_request(&req, SystemTime::now()) {
        BeforeRequest::Fresh(_) => {
            // println!("cached HIT for {}", &index_url);
            // We can completely reuse the cached value.
            parse_sparse_index_package(channel_name, platform_url, reader).await
        }
        BeforeRequest::Stale { request, .. } => {
            println!("cached STALE for {}", &index_url);
            // The resource is stale, but it might still be fresh. We'll have to contact the server
            // to find out though.
            let url = request.uri.to_string().parse().unwrap();
            fetch_and_cache(
                client,
                cache_dir,
                channel_name,
                platform_url,
                url,
                request.headers,
            )
            .await
        }
    }
}

/// Try to read [`RepoDataRecord`]s from a [`SparseIndexPackage`] file at a remote url. Does not
/// read from the cache but does store the result in the cache.
pub async fn fetch_and_cache(
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel_name: Arc<str>,
    platform_url: Url,
    index_url: Url,
    headers: HeaderMap,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    // Construct the request for caching
    let mut req = client.get(index_url.clone());

    // Add the additional header
    for (name, value) in headers {
        if let Some(name) = name {
            req = req.header(name, value)
        }
    }

    // Execute the request
    let (client, req) = req.build_split();
    let req = req.unwrap();
    let res = client.execute(req.try_clone().unwrap()).await?;

    // Special case: 404.
    // If the file is not found we simply assume there are no records for the package
    if res.status() == StatusCode::NOT_FOUND {
        return Ok(vec![]);
    }

    // Filter out any other error cases
    let res = res.error_for_status()?;

    // Create a stream for the bytes with some backpressure.
    let (bytes_sender, bytes_receiver) = broadcast::channel::<Bytes>(100);

    // Construct a cache policy for the request
    let cache_policy = CachePolicy::new(&req, &res);
    let cache_future = if cache_policy.is_storable() {
        // Write the contents the cache
        write_to_cache(
            cache_dir,
            index_url,
            cache_policy,
            BroadcastStream::new(bytes_sender.subscribe())
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e)),
        )
        .left_future()
    } else {
        futures::future::ready(Ok(())).right_future()
    };

    // Decode the records on a background task
    let collect_records_future = parse_sparse_index_package(
        channel_name,
        platform_url,
        StreamReader::new(
            BroadcastStream::new(bytes_receiver)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e)),
        ),
    );

    // Stream the bytes from the response
    let copy_bytes_future = res
        .bytes_stream()
        .map_err(|e| GatewayError::from(e))
        .try_for_each(move |bytes| {
            let bytes_sender = bytes_sender.clone();
            async move {
                let _ = bytes_sender.send(bytes);
                Ok(())
            }
        });

    Ok(try_join!(collect_records_future, copy_bytes_future, cache_future)?.0)
}

/// Writes the given bytes to the cache and prepends the file with the cache policy.
async fn write_to_cache(
    cache_dir: PathBuf,
    index_url: Url,
    cache_policy: CachePolicy,
    mut bytes_stream: impl Stream<Item = io::Result<Bytes>> + Unpin,
) -> Result<(), GatewayError> {
    cacache::Writer::create(cache_dir, index_url)
        .map_err(GatewayError::from)
        .and_then(move |mut writer| async move {
            writer
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(&cache_policy)
                            .expect("failed to convert cache policy to json")
                    )
                    .as_bytes(),
                )
                .await?;

            // Receive bytes and write them to disk
            while let Some(bytes) = bytes_stream.next().await {
                let bytes = bytes?;
                writer.write_all(&bytes).await?;
            }

            writer.commit().await?;
            Ok(())
        })
        .await
}

/// Reads from the reqwest cache.
async fn read_from_cache(
    cache_dir: PathBuf,
    index_url: Url,
) -> Option<(CachePolicy, impl AsyncBufRead)> {
    let Ok(reader) = cacache::Reader::open(cache_dir, index_url) else { return None };

    // Read the cache line from the cached file and convert to a cache policy
    let mut cache_line = String::new();
    reader.read_line(&mut cache_line).await?;
    let Ok(cache_policy) = serde_json::from_str::<CachePolicy>(&cache_line) else { return None };

    Some((cache_policy, reader))
}
