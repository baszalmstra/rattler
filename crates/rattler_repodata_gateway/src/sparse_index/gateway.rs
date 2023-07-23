use bytes::Bytes;
use elsa::sync::FrozenMap;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use fxhash::{FxHashMap, FxHashSet};
use http_cache_semantics::CachePolicy;
use itertools::Itertools;
use parking_lot::Mutex;
use rattler_conda_types::{
    sparse_index::{sparse_index_filename, SparseIndexRecord},
    Channel, Platform, RepoDataRecord,
};
use rattler_networking::AuthenticatedClient;
use reqwest::{Error, StatusCode};
use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{Arc, Weak},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt},
    io::{AsyncWriteExt, BufReader},
    sync::broadcast,
    try_join,
};
use tokio_stream::{
    wrappers::{BroadcastStream, LinesStream},
    Stream,
};
use tokio_util::io::StreamReader;
use url::Url;

/// An error that can occur when accesing records in the [`Gateway`]
#[allow(missing_docs)]
#[derive(thiserror::Error, Debug, Clone)]
pub enum GatewayError {
    #[error("a pending request was cancelled")]
    Cancelled,

    #[error("deserialization error")]
    EncodingError,

    #[error(transparent)]
    IoError(#[from] Arc<std::io::Error>),

    #[error(transparent)]
    HttpError(#[from] Arc<reqwest::Error>),

    #[error(transparent)]
    CacheError(#[from] Arc<cacache::Error>),
}

impl From<reqwest::Error> for GatewayError {
    fn from(value: Error) -> Self {
        GatewayError::HttpError(Arc::new(value))
    }
}

impl From<io::Error> for GatewayError {
    fn from(value: io::Error) -> Self {
        GatewayError::IoError(Arc::new(value))
    }
}

impl From<cacache::Error> for GatewayError {
    fn from(value: cacache::Error) -> Self {
        GatewayError::CacheError(Arc::new(value))
    }
}

/// An object that allows fetching and caching [`RepoDataRecord`]s.
pub struct Gateway {
    inner: Arc<GatewayInner>,
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct CacheKey {
    channel_idx: usize,
    platform: Platform,
    package_name: String,
}

type FetchResultChannel = broadcast::Sender<Result<(), GatewayError>>;

pub struct GatewayInner {
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channels: Vec<Channel>,

    /// A mapping from platform and package name to its records.
    records: FrozenMap<CacheKey, Vec<RepoDataRecord>>,

    /// A mapping from platform and package name to ongoing requests.
    in_flight: Mutex<FxHashMap<CacheKey, Weak<FetchResultChannel>>>,
}

impl Gateway {
    /// Construct a new gateway from one or more channels.
    pub fn from_channels(
        client: AuthenticatedClient,
        cache_dir: impl Into<PathBuf>,
        channels: impl IntoIterator<Item = Channel>,
    ) -> Self {
        Self {
            inner: Arc::new(GatewayInner {
                client,
                cache_dir: cache_dir.into(),
                channels: channels.into_iter().collect(),
                records: FrozenMap::default(),
                in_flight: Mutex::new(FxHashMap::default()),
            }),
        }
    }

    /// Recursively fetching all [`RepoDataRecord]`s for the specified package names from the given
    /// channels.
    pub async fn find_recursive_records(
        &self,
        platforms: Vec<Platform>,
        package_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<FxHashMap<&Channel, Vec<&RepoDataRecord>>, GatewayError> {
        // Construct a set of packages that we have seen and have been added to the pending list.
        let mut seen: FxHashSet<String> =
            FxHashSet::from_iter(package_names.into_iter().map(Into::into));

        // Construct a queue to store packages in that still need to be processed
        let mut pending = VecDeque::from_iter(seen.iter().cloned());

        // Stores the result
        let mut result: FxHashMap<&Channel, Vec<&RepoDataRecord>> = FxHashMap::default();

        // Keep a list of all pending futures
        let mut pending_futures = FuturesUnordered::new();
        loop {
            // Start fetching the records of any pending packages
            for ((package, platform), (channel_idx, _)) in pending
                .drain(..)
                .cartesian_product(platforms.iter().copied())
                .cartesian_product(self.inner.channels.iter().enumerate())
            {
                let fetch_records_future = self.fetch_records(channel_idx, package, platform);
                pending_futures.push(fetch_records_future);
            }

            // Wait for any pending requests to come in, or if we processed them all, stop the loop.
            let (channel_idx, _, records) = match pending_futures.next().await {
                Some(request) => request?,
                None => break,
            };

            // Iterate over all dependencies in the repodata records and try to get their data as well.
            for record in records {
                for dependency in record.package_record.depends.iter() {
                    let dependency_name = dependency.split_once(' ').unwrap_or((dependency, "")).0;
                    if !seen.contains(dependency_name) {
                        pending.push_back(dependency_name.to_string());
                        seen.insert(dependency_name.to_string());
                    }
                }
            }

            // Add records to the result.
            result
                .entry(&self.inner.channels[channel_idx])
                .or_default()
                .extend(records);
        }

        Ok(result)
    }

    /// Downloads all the records for the package with the given name.
    #[allow(clippy::await_holding_lock)] // This is a false positive. The `in_flight` lock is not held while awaiting. It is dropped on time.
    async fn fetch_records(
        &self,
        channel_idx: usize,
        package_name: String,
        platform: Platform,
    ) -> Result<(usize, Platform, &[RepoDataRecord]), GatewayError> {
        let key = CacheKey {
            channel_idx,
            package_name,
            platform,
        };

        // If we already have the records we can return them immediately.
        if let Some(records) = self.inner.records.get(&key) {
            return Ok((channel_idx, platform, records));
        }

        // Otherwise, we look for an in-flight request
        let mut in_flight = self.inner.in_flight.lock();

        // Now that we acquired the lock, another task may have already written its results
        // in the records map. Check if that's the case while holding on to the lock.
        if let Some(records) = self.inner.records.get(&key) {
            return Ok((channel_idx, platform, records));
        }

        // Check if there is an in flight request for our package
        let mut receiver = if let Some(sender) = in_flight.get(&key).and_then(Weak::upgrade) {
            sender.subscribe()
        } else {
            let (tx, rx) = broadcast::channel(1);
            let tx = Arc::new(tx);
            in_flight.insert(key.clone(), Arc::downgrade(&tx));

            let inner = self.inner.clone();
            let key = key.clone();
            let client = self.inner.client.clone();
            let cache_dir = self.inner.cache_dir.clone();
            tokio::spawn(async move {
                let result = match fetch_from_channel(
                    client,
                    cache_dir,
                    &inner.channels[channel_idx],
                    platform,
                    key.package_name.clone(),
                )
                .await
                {
                    Ok(records) => {
                        // println!("inserting values for {:?}", &key);
                        inner.records.insert(key, records);
                        Ok(())
                    }
                    Err(err) => {
                        // println!("ERROR: {}", &err);
                        Err(err)
                    }
                };

                // Broadcast the result
                let _ = tx.send(result);
            });

            rx
        };

        // Drop the in-flight lock or we will dead-lock while waiting for it to finish.
        drop(in_flight);

        receiver
            .recv()
            .await
            .map_err(|_| GatewayError::Cancelled)
            .map(|result| {
                result.map(|_| {
                    (
                        channel_idx,
                        platform,
                        self.inner.records.get(&key).expect(&format!(
                            "records must be present in the frozen set {key:?}"
                        )),
                    )
                })
            })?
    }
}

async fn fetch_from_channel(
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel: &Channel,
    platform: Platform,
    package_name: String,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    let channel_name: Arc<str> = channel.canonical_name().into();
    let platform_url = channel.platform_url(platform);

    // println!("Started download of {} on {platform}", &package_name);

    if platform_url.scheme() == "file" {
        if let Ok(platform_path) = platform_url.to_file_path() {
            return fetch_from_local_channel(channel_name, &package_name, platform_path).await;
        }
    }

    fetch_from_remote_channel(client, cache_dir, channel_name, &package_name, platform_url).await
}

/// Try to read [`RepoDataRecord`]s from a SparseIndexPackage file on disk.
async fn fetch_from_local_channel(
    channel_name: Arc<str>,
    package_name: &str,
    platform_path: PathBuf,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    let package_path = platform_path.join(sparse_index_filename(&package_name).unwrap());
    let platform_url = Url::from_directory_path(platform_path)
        .expect("platform path must refer to a valid directory");

    // Read the file from disk. If the file is not found we simply return no records.
    let file = match tokio::fs::File::open(&package_path).await {
        Ok(file) => BufReader::new(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(GatewayError::IoError(Arc::new(e))),
    };

    // Deserialize each line individually
    parse_sparse_index_package(file)
        .map_ok(move |record| RepoDataRecord {
            package_record: record.package_record,
            url: platform_url
                .join(&record.file_name)
                .expect("must be able to append a filename"),
            file_name: record.file_name,
            channel: channel_name.clone(),
        })
        .try_collect()
        .await
}

/// Try to read [`RepoDataRecord`]s from a [`SparseIndexPackage`] file at a remote url. Reads from
/// the cache if thats possible.
async fn fetch_from_remote_channel(
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel_name: Arc<str>,
    package_name: &str,
    platform_url: Url,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    // Determine the location of the [`SparseIndexPackage`] file.
    let package_path = sparse_index_filename(package_name).expect("invalid package name");
    let index_url = platform_url
        .join(&package_path.to_string_lossy())
        .expect("invalid package path");

    remote_fetch(client, cache_dir, channel_name, platform_url, index_url).await
}

/// Try to read [`RepoDataRecord`]s from a [`SparseIndexPackage`] file at a remote url. Does not
/// read from the cache but does store the result in the cache.
async fn remote_fetch(
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel_name: Arc<str>,
    platform_url: Url,
    index_url: Url,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    // Construct the request for caching
    let req = client
        .get(index_url.clone())
        .build()
        .expect("failed to create request");

    // Send the request.
    let res = client.get(index_url.clone()).send().await?;

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
    let collect_records_future = parse_sparse_index_package(StreamReader::new(
        BroadcastStream::new(bytes_receiver).map_err(|e| io::Error::new(io::ErrorKind::Other, e)),
    ))
    .map_ok(|record| RepoDataRecord {
        package_record: record.package_record,
        url: platform_url
            .join(&record.file_name)
            .expect("must be able to append a filename"),
        file_name: record.file_name,
        channel: channel_name.clone(),
    })
    .try_collect();

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

/// Given a stream of bytes, parse individual lines as [`SparseIndexRecord`]s.
fn parse_sparse_index_package<R: AsyncBufRead>(
    reader: R,
) -> impl Stream<Item = Result<SparseIndexRecord, GatewayError>> {
    LinesStream::new(reader.lines())
        .map_err(|e| GatewayError::IoError(Arc::new(e)))
        .map_ok(|line| parse_sparse_index_record(line))
        .try_buffered(10)
}

async fn parse_sparse_index_record(line: String) -> Result<SparseIndexRecord, GatewayError> {
    serde_json::from_str::<SparseIndexRecord>(&line).map_err(|_| GatewayError::EncodingError)
    // tokio::task::spawn_blocking(move || {
    //     serde_json::from_str::<SparseIndexRecord>(&line).map_err(|_| GatewayError::EncodingError)
    // })
    // .map_ok_or_else(
    //     |join_err| match join_err.try_into_panic() {
    //         Ok(panic) => {
    //             std::panic::resume_unwind(panic);
    //         }
    //         Err(_) => Err(GatewayError::Cancelled),
    //     },
    //     |record| record,
    // )
    // .await
}

#[cfg(test)]
mod test {}
