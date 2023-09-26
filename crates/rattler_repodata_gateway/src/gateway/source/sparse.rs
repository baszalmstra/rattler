use super::SubdirSourceError;
use crate::gateway::FetchRecordsError;
use crate::sparse::SparseRepoData;
use rattler_conda_types::{Channel, PackageName, Platform, RepoDataRecord};
use std::path::PathBuf;
use std::sync::Arc;

pub struct SparseRepoDataSource {
    data: Arc<SparseRepoData>,
}

impl SparseRepoDataSource {
    /// Construct a new [`SparseRepoDataSource`] from a path that points to a `repodata.json` file
    /// and the associated channel and platform data.
    pub async fn new(
        channel: Channel,
        platform: Platform,
        path: PathBuf,
    ) -> Result<Self, SubdirSourceError> {
        let data = tokio::task::spawn_blocking(move || {
            SparseRepoData::new(channel, platform.as_str(), path, None)
        })
        .await??;

        Ok(Self {
            data: Arc::new(data),
        })
    }

    /// Load records from the source without caching. Ownership of the records is returned to the
    /// caller.
    pub async fn fetch_records(
        &self,
        package_name: &PackageName,
    ) -> Result<Vec<RepoDataRecord>, FetchRecordsError> {
        let sparse = self.data.clone();
        let package_name = package_name.clone();
        tokio::task::spawn_blocking(move || sparse.load_records(&package_name))
            .await?
            .map_err(Into::into)
    }
}
