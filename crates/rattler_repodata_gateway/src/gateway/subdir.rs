use super::SubdirSource;
use crate::utils::cache_map::{CacheMap, CoalescingError};
use rattler_conda_types::{PackageName, RepoDataRecord};
use std::sync::Arc;
use thiserror::Error;
use tokio::task::JoinError;

/// Keeps track of a single channel subdirectory and all the packages we retrieved from it so far.
pub struct Subdir {
    /// Where to get the data from.
    source: Arc<SubdirSource>,

    /// Records per package
    records: CacheMap<PackageName, Vec<RepoDataRecord>, FetchRecordsError>,
}

#[derive(Debug, Error)]
pub enum FetchRecordsError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("the operation was cancelled")]
    Cancelled,
}

impl From<JoinError> for FetchRecordsError {
    fn from(value: JoinError) -> Self {
        match value.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => FetchRecordsError::Cancelled,
        }
    }
}

impl Subdir {
    /// Constructs a new subdir from a source.
    pub fn new(source: SubdirSource) -> Self {
        Self {
            source: Arc::new(source),
            records: Default::default(),
        }
    }

    /// Returns all the records associated with the specified package
    pub async fn get_or_cache_records(
        &self,
        package_name: &PackageName,
    ) -> Result<&[RepoDataRecord], FetchRecordsError> {
        let pkg_name = package_name.clone();
        let source = self.source.clone();
        self.records
            .get_or_cache(package_name, move || async move {
                match source.as_ref() {
                    SubdirSource::SparseRepoData(source) => source.fetch_records(&pkg_name).await,
                }
            })
            .await
            .map_err(|err| match err {
                CoalescingError::CacheError(err) => err,
                _ => FetchRecordsError::Cancelled,
            })
    }
}
