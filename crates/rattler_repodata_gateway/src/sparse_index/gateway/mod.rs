// mod local;
// mod remote;
mod http;
mod source;

use crate::sparse_index::gateway::source::SubdirSourceError;
use crate::utils::{CoalescingError, FrozenCoalescingMap};
use ::http::StatusCode;
use futures::stream::FuturesUnordered;
use futures::{stream, StreamExt, TryFutureExt, TryStreamExt};
use fxhash::{FxHashMap, FxHashSet};
use itertools::Itertools;
use rattler_conda_types::{sparse_index::SparseIndexRecord, Channel, Platform, RepoDataRecord};
use rattler_networking::AuthenticatedClient;
use reqwest::Error;
use source::SubdirSource;
use std::collections::VecDeque;
use std::{io, path::PathBuf, sync::Arc};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio_stream::{wrappers::LinesStream, Stream};
use url::Url;

/// An error that can occur when accesing records in the [`Gateway`]
#[allow(missing_docs)]
#[derive(thiserror::Error, Debug, Clone)]
pub enum GatewayError {
    #[error("a request was cancelled")]
    Cancelled,

    #[error("deserialization error")]
    EncodingError,

    #[error(transparent)]
    IoError(#[from] Arc<std::io::Error>),

    #[error(transparent)]
    HttpError(#[from] Arc<reqwest::Error>),

    #[error("HTTP status error ({0}) for {1}")]
    HttpStatus(StatusCode, Url),

    #[error(transparent)]
    CacheError(#[from] Arc<cacache::Error>),

    #[error(transparent)]
    SubDirError(#[from] Arc<SubdirSourceError>),
}

impl<E: Into<GatewayError>> From<CoalescingError<E>> for GatewayError {
    fn from(value: CoalescingError<E>) -> Self {
        match value {
            CoalescingError::CacheError(e) => e.into(),
            CoalescingError::Cancelled => GatewayError::Cancelled,
        }
    }
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

/// An object that allows fetching and caching [`RepoDataRecord`]s from various sources.
pub struct Gateway {
    inner: Arc<GatewayInner>,
}

pub struct GatewayInner {
    /// The client to use to download remote files
    client: AuthenticatedClient,

    /// The directory to store caches
    cache_dir: PathBuf,

    /// A mapping of all channel subdirs this instance keeps track of and the data we know about
    /// their contents.
    subdirs: FrozenCoalescingMap<(Channel, Platform), Box<Subdir>, GatewayError>,
}

impl Gateway {
    /// Construct a new gateway from one or more channels.
    pub fn new(client: AuthenticatedClient, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(GatewayInner {
                client,
                cache_dir: cache_dir.into(),
                subdirs: Default::default(),
            }),
        }
    }

    /// Retrieve the specified subdirectory.
    async fn subdir(&self, channel: &Channel, platform: Platform) -> Result<&Subdir, GatewayError> {
        let key = (channel.clone(), platform);
        let inner = self.inner.as_ref();
        Ok(inner
            .subdirs
            .get_or_cache(&key, || {
                Subdir::new(
                    inner.client.clone(),
                    inner.cache_dir.clone(),
                    channel.clone(),
                    platform,
                )
                .map_err(Arc::new)
                .map_err(GatewayError::from)
                .map_ok(Box::new)
            })
            .await?)
    }

    /// Recursively fetches all [`RepoDataRecord]`s for the specified package names from the given
    /// channels.
    pub async fn find_recursive_records<'c>(
        &self,
        channels: impl IntoIterator<Item = &'c Channel>,
        platforms: Vec<Platform>,
        package_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<FxHashMap<&'c Channel, Vec<&RepoDataRecord>>, GatewayError> {
        // Get all the different channels and platforms
        let channels: Vec<_> = channels.into_iter().collect();
        let platforms = platforms;

        // Get all subdirs
        let subdirs: Vec<(&'c Channel, &Subdir)> = stream::iter(
            channels
                .iter()
                .copied()
                .cartesian_product(platforms.iter().copied()),
        )
        .map(|(channel, platform)| {
            self.subdir(channel, platform)
                .map_ok(move |subdir| (channel, subdir))
        })
        .buffer_unordered(10)
        .try_collect()
        .await?;

        // Construct a set of packages that we have seen and have been added to the pending list.
        let mut seen: FxHashSet<String> =
            FxHashSet::from_iter(package_names.into_iter().map(Into::into));

        // Construct a queue to store packages in that still need to be processed
        let mut pending = VecDeque::from_iter(seen.iter().cloned());

        // Stores the result
        let mut result: FxHashMap<&'c Channel, Vec<&RepoDataRecord>> = FxHashMap::default();

        // Keep a list of all pending futures
        let mut total_requests = 0;
        let mut total_packages_from_prefetch = 0;
        let mut pending_futures = FuturesUnordered::new();
        let mut pending_for_execution = VecDeque::new();
        loop {
            // Start fetching the records of any pending packages
            while let Some(pkg_name) = pending.pop_front() {
                // Create tasks to fetch records from all subdirs
                for (channel, subdir) in subdirs.iter() {
                    let fetch_records_future = subdir
                        .get_or_cache_records(pkg_name.clone())
                        .map_ok(move |records| (*channel, records));
                    pending_for_execution.push_back(fetch_records_future);
                    total_requests += 1;
                }

                // Find any dependencies that we can start prefetching before the records are
                // fetched.
                for (_, subdir) in subdirs.iter() {
                    for dep_name in subdir.prefetch_hints(&pkg_name) {
                        if !seen.contains(&dep_name) {
                            pending.push_back(dep_name.to_owned());
                            seen.insert(dep_name.to_owned());
                            total_packages_from_prefetch += 1;
                        }
                    }
                }
            }

            // Make sure there are no more than 50 requests at a time.
            while !pending_for_execution.is_empty() {
                if pending_futures.len() < 100 {
                    pending_futures.push(pending_for_execution.pop_front().unwrap());
                } else {
                    break;
                }
            }

            // Wait for any pending requests to come in, or if we processed them all, stop the loop.
            let (channel, records) = match pending_futures.next().await {
                Some(request) => request?,
                None => break,
            };

            // Add the dependencies of all the records.
            for record in records.iter() {
                for dependency in record.package_record.depends.iter() {
                    let dep_name = dependency.split_once(' ').unwrap_or((dependency, "")).0;
                    if !seen.contains(dep_name) {
                        pending.push_back(dep_name.to_owned());
                        seen.insert(dep_name.to_owned());
                    }
                }
            }

            // Add records to the result.
            result.entry(channel).or_default().extend(records);
        }

        println!("Total requests: {}", total_requests);
        println!("Total packages: {}", seen.len());
        println!(
            "Total packages from prefetch: {}",
            total_packages_from_prefetch
        );

        Ok(result)
    }
}

/// Keeps track of a single channel subdirectory and all the packages we retrieved from it so far.
struct Subdir {
    /// Where to get the data from.
    source: Arc<SubdirSource>,

    /// Records per package
    records: FrozenCoalescingMap<String, Vec<RepoDataRecord>, GatewayError>,
}

impl Subdir {
    /// Constructs a new subdir from a channel.
    pub async fn new(
        client: AuthenticatedClient,
        cache_dir: PathBuf,
        channel: Channel,
        platform: Platform,
    ) -> Result<Subdir, SubdirSourceError> {
        let source = SubdirSource::new(client, cache_dir, channel, platform).await?;
        Ok(Self {
            source: Arc::new(source),
            records: Default::default(),
        })
    }

    /// Getch the records from the source and cache them locally.
    pub async fn get_or_cache_records(
        &self,
        package_name: String,
    ) -> Result<&[RepoDataRecord], GatewayError> {
        Ok(self
            .records
            .get_or_cache(&package_name, || {
                let pkg_name = package_name.clone();
                let source = self.source.clone();
                async move {
                    match source.as_ref() {
                        SubdirSource::LocalSparseIndex(local) => {
                            local.fetch_records(&pkg_name).await
                        }
                        SubdirSource::RemoteSparseIndex(remote) => {
                            remote.fetch_records(&pkg_name).await
                        }
                    }
                }
            })
            .await?)
    }

    /// Returns hints on which packages to prefetch for package with the given name. This method
    /// should be used to determine which dependent packages to fetch without actually fetching
    /// the metadata of the package.
    ///
    /// Package records will still be fetched and inspected so the package names returned from this
    /// function may be incorrect.
    pub fn prefetch_hints(&self, package_name: &str) -> Vec<String> {
        match self.source.as_ref() {
            SubdirSource::LocalSparseIndex(_) => vec![],
            SubdirSource::RemoteSparseIndex(source) => source.prefetch_hints(package_name),
        }
    }
}

/// Given a stream of bytes, parse individual lines as [`SparseIndexRecord`]s.
fn parse_sparse_index_package_stream<R: AsyncBufRead>(
    reader: R,
) -> impl Stream<Item = Result<SparseIndexRecord, GatewayError>> {
    // Decompress the reader
    let decoded_stream =
        BufReader::new(async_compression::tokio::bufread::ZstdDecoder::new(reader));

    LinesStream::new(decoded_stream.lines())
        .map_err(|e| GatewayError::IoError(Arc::new(e)))
        .map_ok(parse_sparse_index_record)
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
