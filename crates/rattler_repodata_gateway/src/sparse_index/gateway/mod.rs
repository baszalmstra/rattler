mod local;
mod remote;
mod source;

use elsa::sync::FrozenMap;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use fxhash::{FxHashMap, FxHashSet};
use itertools::Itertools;
use parking_lot::Mutex;
use rattler_conda_types::{
    sparse_index::{sparse_index_filename, SparseIndexRecord},
    Channel, Platform, RepoDataRecord,
};
use rattler_networking::AuthenticatedClient;
use reqwest::Error;
use source::SubdirSource;
use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::{Arc, Weak},
};
use tokio::{
    io::AsyncWriteExt,
    io::{AsyncBufRead, AsyncBufReadExt},
    sync::broadcast,
};
use tokio_stream::{wrappers::LinesStream, Stream, StreamExt};
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

    #[error("invalid subdir url '{0}'")]
    InvalidSubdirUrl(Url),
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

type FetchResultChannel = Weak<broadcast::Sender<Result<(), GatewayError>>>;
type InFlightSubdirChannel = Weak<broadcast::Sender<Result<(), GatewayError>>>;

pub struct GatewayInner {
    /// The client to use to download remote files
    client: AuthenticatedClient,

    /// The directory to store caches
    cache_dir: PathBuf,

    /// A mapping of all channel subdirs this instance keeps track of and the data we know about
    /// their contents.
    subdirs: FrozenMap<(Channel, Platform), Subdir>,

    /// A mapping of in-flight requests.
    in_flight: Mutex<FxHashMap<(Channel, Platform), InFlightSubdirChannel>>,
}

/// Keeps track of a single channel subdirectory and all the packages we retrieved from it so far.
struct Subdir {
    /// Where to get the data from.
    source: SubdirSource,

    /// Records per package
    records: FrozenMap<String, Vec<RepoDataRecord>>,

    /// Current requests that are in-flight.
    in_flight: Mutex<FxHashMap<String, FetchResultChannel>>,
}

impl Gateway {
    /// Construct a new gateway from one or more channels.
    pub fn new(client: AuthenticatedClient, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(GatewayInner {
                client,
                cache_dir: cache_dir.into(),
                subdirs: Default::default(),
                in_flight: Default::default(),
            }),
        }
    }

    /// Retrieve the specified subdirectory.
    async fn subdir(&self, channel: &Channel, platform: Platform) -> Result<&Subdir, GatewayError> {
        let key = (channel.clone(), platform.clone());
        let inner = self.inner.as_ref();

        // Fast path: did we already fetch everything we need to know about this subdir?
        if let Some(subdir) = inner.subdirs.get(&key) {
            return Ok(subdir);
        }

        // Check if there is an in-flight request
        let mut in_flight = inner.in_flight.lock();

        // Now that we acquired the lock, another task may have already written its results
        // in the subdirs map. Check if that's the case while holding on to the lock.
        if let Some(subdir) = inner.subdirs.get(&key) {
            return Ok(subdir);
        }

        // Check if there is an in flight request
        let mut receiver = if let Some(sender) = in_flight.get(&key).and_then(Weak::upgrade) {
            sender.subscribe()
        } else {
            // Create a new sender over which we can send results to other channels.
            let (tx, rx) = broadcast::channel(1);
            let tx = Arc::new(tx);
            in_flight.insert(key.clone(), Arc::downgrade(&tx));

            // Spawn a task to fetch the subdir
            let inner = self.inner.clone();
            let channel = channel.clone();
            tokio::spawn(async move {
                // Construct a new `Subdir`
                let subdir = Subdir::new(
                    inner.client.clone(),
                    inner.cache_dir.clone(),
                    channel.clone(),
                    platform,
                )
                .await;

                // Store the subdirectory in the inner data structure.
                let result = match subdir {
                    Ok(subdir) => {
                        inner.subdirs.insert((channel, platform), subdir);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };

                let _ = tx.send(result);
            });

            rx
        };

        // Drop the in-flight lock or we will dead-lock while waiting for it to finish.
        drop(in_flight);

        // Wait for the task to finish
        let result = receiver.recv().await.map_err(|_| GatewayError::Cancelled)?;

        // Get the result from the frozen set.
        result.map(|_| {
            inner
                .subdirs
                .get(&key)
                .expect(&format!("subdir must be present in the frozen map"))
        })
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
        let mut total_requests = 0;
        let mut pending_futures = FuturesUnordered::new();
        let mut pending_for_execution = VecDeque::new();
        loop {
            // Start fetching the records of any pending packages
            for ((package, platform), (channel_idx, _)) in pending
                .drain(..)
                .cartesian_product(platforms.iter().copied())
                .cartesian_product(self.inner.channels.iter().enumerate())
            {
                let fetch_records_future = self.fetch_records(channel_idx, package, platform);
                pending_for_execution.push_back(fetch_records_future);
                total_requests += 1;
            }

            // Make sure there are no more than 50 requests at a time.
            while !pending_for_execution.is_empty() {
                if pending_futures.len() < 50 {
                    pending_futures.push(pending_for_execution.pop_front().unwrap());
                } else {
                    break;
                }
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

        println!("Total requests: {}", total_requests);
        println!("Total packages: {}", seen.len());

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

impl Subdir {
    /// Constructs a new subdir from a channel.
    pub async fn new(
        client: AuthenticatedClient,
        cache_dir: PathBuf,
        channel: Channel,
        platform: Platform,
    ) -> Result<Subdir, GatewayError> {
        let source = SubdirSource::new(channel, platform).await?;
        Ok(Self {
            source,
            records: Default::default(),
            in_flight: Default::default(),
        })
    }

    /// Fetch the records from the source
    async fn fetch_records_inner(&self, package_name: &str) -> Result<Vec<RepoDataRecord>, GatewayError> {
        match &self.source {
            SubdirSource::LocalSparseIndex(local) => local.fetch_records(package_name).await,
            SubdirSource::RemoteSparseIndex(_) => unreachable!(),
        }
    }

    /// Fetch
}

/// Fetch the [`RepoDataRecords`] for a named packaged that are part of the specified channel and
/// platform. If no such records exist (because the package only has entries for another platform
/// for example), this method returns an empty `Vec`.
async fn fetch_from_channel(
    client: AuthenticatedClient,
    cache_dir: PathBuf,
    channel: &Channel,
    platform: Platform,
    package_name: String,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    let channel_name: Arc<str> = channel.canonical_name().into();
    let platform_url = channel.platform_url(platform);

    // If the channel resides on the filesystem, we read it directly from there.
    if platform_url.scheme() == "file" {
        if let Ok(platform_path) = platform_url.to_file_path() {
            return local::fetch_from_local_channel(channel_name, &package_name, platform_path)
                .await;
        }
    }

    // Otherwise, we have to perform an http request
    fetch_from_remote_channel(client, cache_dir, channel_name, &package_name, platform_url).await
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

    remote::remote_fetch(client, cache_dir, channel_name, platform_url, index_url).await
}

/// Given a stream of bytes, parse individual lines as [`SparseIndexRecord`]s.
fn parse_sparse_index_package_stream<R: AsyncBufRead>(
    reader: R,
) -> impl Stream<Item = Result<SparseIndexRecord, GatewayError>> {
    LinesStream::new(reader.lines())
        .map_err(|e| GatewayError::IoError(Arc::new(e)))
        .map_ok(|line| parse_sparse_index_record(line))
        .try_buffered(10)
}

/// Given a stream of bytes, collect them into a Vec of [`SparseIndexRecord`]s.
async fn parse_sparse_index_package<R: AsyncBufRead>(
    channel_name: Arc<str>,
    platform_url: Url,
    reader: R,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    parse_sparse_index_package_stream(reader)
        .map_ok(|record| RepoDataRecord {
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

async fn parse_sparse_index_record(line: String) -> Result<SparseIndexRecord, GatewayError> {
    serde_json::from_str::<SparseIndexRecord>(&line).map_err(|_| GatewayError::EncodingError)
}

#[cfg(test)]
mod test {}
