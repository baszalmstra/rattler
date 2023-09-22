mod subdir;
mod subdir_source;

use crate::utils::cache_map::{CacheMap, CoalescingError};
use rattler_conda_types::{Channel, Platform};
use rattler_networking::AuthenticatedClient;
use std::{path::PathBuf, sync::Arc};

use subdir::Subdir;
pub use subdir_source::{SubdirSource, SubdirSourceError};

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
    subdirs: CacheMap<(Channel, Platform), Box<Subdir>, SubdirSourceError>,
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
    async fn get_or_cache_subdir(
        &self,
        channel: &Channel,
        platform: Platform,
    ) -> Result<&Subdir, CoalescingError<SubdirSourceError>> {
        let key = (channel.clone(), platform);
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
                .map_ok(Subdir::new)
            })
            .await?)
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
        package_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<FxHashMap<&'c Channel, Vec<&RepoDataRecord>>, GatewayError> {
    }
}
