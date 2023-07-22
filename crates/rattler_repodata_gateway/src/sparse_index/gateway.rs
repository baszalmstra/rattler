use elsa::sync::FrozenMap;
use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryStreamExt};
use fxhash::{FxHashMap, FxHashSet};
use itertools::Itertools;
use parking_lot::Mutex;
use rattler_conda_types::sparse_index::{sparse_index_filename, SparseIndexRecord};
use rattler_conda_types::{Channel, Platform, RepoDataRecord};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Weak};
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::sync::broadcast;
use tokio_stream::wrappers::LinesStream;

/// An error that can occur when accesing records in the [`Gateway`]
#[allow(missing_docs)]
#[derive(thiserror::Error, Debug, Clone)]
pub enum GatewayError {
    #[error("a pending request was cancelled")]
    Cancelled,

    #[error("deserialization error")]
    EncodingError,

    #[error(transparent)]
    IoError(Arc<std::io::Error>),
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
    channels: Vec<Channel>,

    /// A mapping from platform and package name to its records.
    records: FrozenMap<CacheKey, Vec<RepoDataRecord>>,

    /// A mapping from platform and package name to ongoing requests.
    in_flight: Mutex<FxHashMap<CacheKey, Weak<FetchResultChannel>>>,
}

impl Gateway {
    /// Construct a new gateway from one or more channels.
    pub fn from_channels(channels: impl IntoIterator<Item = Channel>) -> Self {
        Self {
            inner: Arc::new(GatewayInner {
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
            tokio::spawn(async move {
                let result = match fetch_from_channel(
                    &inner.channels[channel_idx],
                    platform,
                    key.package_name.clone(),
                )
                .await
                {
                    Ok(records) => {
                        inner.records.insert(key, records);
                        Ok(())
                    }
                    Err(err) => Err(err),
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
            .map(|_| {
                (
                    channel_idx,
                    platform,
                    self.inner
                        .records
                        .get(&key)
                        .expect("records must be present in the frozen set"),
                )
            })
    }
}

async fn fetch_from_channel(
    channel: &Channel,
    platform: Platform,
    package: String,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    let package_path = sparse_index_filename(Path::new(&package)).unwrap();

    let index_url = channel
        .platform_url(platform)
        .join(&format!("{}", package_path.display()))
        .unwrap();

    let channel_name: Arc<str> = channel.canonical_name().into();
    let platform_url = channel.platform_url(platform);

    if let Ok(file_path) = index_url.to_file_path() {
        // Read the file from disk.
        let file = match tokio::fs::File::open(&file_path).await {
            Ok(file) => BufReader::new(file),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(GatewayError::IoError(Arc::new(e))),
        };

        // Deserialize each line individually
        LinesStream::new(file.lines())
            .map_err(|e| GatewayError::IoError(Arc::new(e)))
            .and_then(move |line| {
                let platform_url = platform_url.clone();
                let channel_name = channel_name.clone();
                async move {
                    serde_json::from_str::<SparseIndexRecord>(&line)
                        .map(|record| RepoDataRecord {
                            package_record: record.package_record,
                            url: platform_url
                                .join(&record.file_name)
                                .expect("must be able to append a filename"),
                            file_name: record.file_name,
                            channel: channel_name.clone(),
                        })
                        .map_err(|_| GatewayError::EncodingError)
                }
            })
            .try_collect()
            .await
    } else {
        unreachable!("only local disk is supported")
    }
}

#[cfg(test)]
mod test {}
