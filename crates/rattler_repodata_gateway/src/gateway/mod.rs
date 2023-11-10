mod source;
mod subdir;

use crate::utils::cache_map::{CacheMap, CoalescingError};
use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryFutureExt};
use itertools::Itertools;
use rattler_conda_types::{Channel, PackageName, Platform, RepoDataRecord};
use rattler_networking::AuthenticatedClient;
use std::collections::{HashMap, HashSet, VecDeque};
use std::{path::PathBuf, sync::Arc};
use thiserror::Error;

pub use source::{SubdirSource, SubdirSourceError};
pub use subdir::FetchRecordsError;
use subdir::Subdir;

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
    subdirs: CacheMap<(Channel, Platform), Box<Option<Subdir>>, SubdirSourceError>,
}

#[derive(Debug, Error)]
pub enum GatewayError<'c> {
    #[error(transparent)]
    FetchRecordsError(#[from] FetchRecordsError),

    // TODO: Better error
    #[error("failed to fetch channel data")]
    SubdirSourceError(&'c Channel, Platform, SubdirSourceError),

    #[error("the operation was cancelled")]
    Cancelled,
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

    /// Returns the [`Subdir`] instance for the given channel and platform.
    ///
    /// This function caches any existing `Subdir`. If multiple requests are made for the same
    /// subdir the requests are coalesced and a single `Subdir` instance is returned.
    ///
    /// If the repodata for the subdir could not be found (e.g. it is missing) an error is only
    /// reported if the platform is [`Platform::NoArch`]. In all other cases `None` is returned.
    async fn get_or_cache_subdir(
        &self,
        channel: &Channel,
        platform: Platform,
    ) -> Result<Option<&Subdir>, CoalescingError<SubdirSourceError>> {
        let key = (channel.clone(), platform);
        let canonical_name = channel.canonical_name();
        let inner = self.inner.as_ref();
        Ok(inner
            .subdirs
            .get_or_cache(&key, || {
                SubdirSource::new(
                    inner.client.clone(),
                    inner.cache_dir.clone(),
                    channel.clone(),
                    platform,
                )
                .map_ok_or_else(
                    move |err| match err {
                        SubdirSourceError::NotFound(_) if platform != Platform::NoArch => {
                            tracing::info!(
                                "ignoring missing repodata for {canonical_name}/{platform}",
                            );
                            Ok(None)
                        }
                        e => Err(e),
                    },
                    |source| Ok(Some(Subdir::new(source))),
                )
                .map_ok(Box::new)
            })
            .await?
            .as_ref())
    }

    /// Fetches all [`RepoDataRecord]`s for the specified package names from the given channels and
    /// for the specified platforms. Dependencies of the packages are fetched recursively.
    ///
    /// This function returns references to the records. The [`Gateway`] caches the records. If a
    /// seconds requests includes the same record the same references are returned.
    pub async fn find_recursive_records<'c>(
        &self,
        channels: impl IntoIterator<Item = &'c Channel>,
        platforms: impl IntoIterator<Item = Platform>,
        package_names: impl IntoIterator<Item = PackageName>,
    ) -> Result<HashMap<&'c Channel, Vec<&RepoDataRecord>>, GatewayError<'c>> {
        let platforms = platforms.into_iter().collect_vec();
        let channels = channels.into_iter().collect_vec();

        // Get all the subdirectories involved in the search. This only creates the requests for
        // the subdirs but doesnt actually wait for the objects to be created. Since the duration of
        // fetching repodata might differ significantly between different subdirs we want to be able
        // to start fetching records from subdirs as soon as possible.
        let subdirs = channels
            .iter()
            .cartesian_product(platforms.iter().cloned())
            .map(|(&channel, subdir)| (async_once_cell::OnceCell::new(), channel, subdir))
            .collect_vec();

        // Construct a set of packages that we have seen and have been added to the pending list.
        let mut seen: HashSet<PackageName> = HashSet::from_iter(package_names.into_iter());

        // Construct a queue to store packages in that still need to be processed
        let mut pending = VecDeque::from_iter(seen.iter().cloned());

        // Stores the result
        let mut result: HashMap<&'c Channel, Vec<&RepoDataRecord>> = Default::default();

        // A list of currently executing futures
        let mut pending_futures = FuturesUnordered::new();
        loop {
            // Start processing any pending package names.
            while let Some(pending) = pending.pop_front() {
                // Create tasks to fetch records from all subdirs
                for (cell, channel, platform) in subdirs.iter() {
                    let pending = pending.clone();
                    pending_futures.push(async move {
                        match cell
                            .get_or_try_init(self.get_or_cache_subdir(channel, *platform))
                            .await
                        {
                            Ok(Some(subdir)) => {
                                subdir
                                    .get_or_cache_records(&pending)
                                    .map_err(GatewayError::FetchRecordsError)
                                    .map_ok(|records| (*channel, records))
                                    .await
                            }
                            Ok(None) => Ok((*channel, &[][..])),
                            Err(CoalescingError::CacheError(error)) => {
                                Err(GatewayError::SubdirSourceError(channel, *platform, error))
                            }
                            Err(_) => Err(GatewayError::Cancelled),
                        }
                    });
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
                    let dep_name = PackageName::new_unchecked(
                        dependency.split_once(' ').unwrap_or((dependency, "")).0,
                    );
                    if !seen.contains(&dep_name) {
                        pending.push_back(dep_name.clone());
                        seen.insert(dep_name);
                    }
                }
            }

            // Add records to the result.
            result.entry(channel).or_default().extend(records);
        }

        Ok(result)
    }
}

#[cfg(test)]
mod test {
    use crate::gateway::Gateway;
    use crate::sparse::load_repo_data_recursively;
    use itertools::Itertools;
    use rattler_conda_types::{Channel, ChannelConfig, PackageName, Platform};
    use rattler_networking::AuthenticatedClient;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;
    use url::Url;

    fn test_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data")
    }
    fn conda_forge_channel() -> Channel {
        let url = Url::from_directory_path(test_dir().join("channels/conda-forge/")).unwrap();
        Channel::from_url(url, None, &ChannelConfig::default())
    }

    #[tokio::test]
    async fn test_gateway() {
        let cache_dir = TempDir::new().unwrap();
        let gateway = Gateway::new(AuthenticatedClient::default(), cache_dir.path());
        let channel = conda_forge_channel();

        let records = gateway
            .find_recursive_records(
                [&channel],
                [Platform::NoArch, Platform::Linux64],
                [PackageName::new_unchecked("python")],
            )
            .await
            .unwrap();

        let records = records
            .into_values()
            .flat_map(|r| {
                r.into_iter()
                    .map(|r| format!("{}/{}", r.package_record.subdir, r.file_name))
            })
            .sorted();

        println!("Records {}", records.len());

        insta::assert_snapshot!(records.into_iter().join("\n"));
    }

    #[tokio::test]
    async fn test_sparse() {
        let channel = conda_forge_channel();
        let records = load_repo_data_recursively(
            [
                (
                    channel.clone(),
                    "noarch",
                    test_dir().join("channels/conda-forge/noarch/repodata.json"),
                ),
                (
                    channel.clone(),
                    "linux-64",
                    test_dir().join("channels/conda-forge/linux-64/repodata.json"),
                ),
            ],
            ["python".parse().unwrap()],
            None,
        )
        .await
        .unwrap();

        let records = records
            .into_iter()
            .flat_map(|r| {
                r.into_iter()
                    .map(|r| format!("{}/{}", r.package_record.subdir, r.file_name))
            })
            .sorted();

        println!("Records {}", records.len());

        insta::assert_snapshot!(records.into_iter().join("\n"));
    }
}
