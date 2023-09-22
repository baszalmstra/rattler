use super::SubdirSource;
use crate::utils::cache_map::CacheMap;
use rattler_conda_types::RepoDataRecord;
use std::sync::Arc;
use thiserror::Error;

/// Keeps track of a single channel subdirectory and all the packages we retrieved from it so far.
pub struct Subdir {
    /// Where to get the data from.
    source: Arc<SubdirSource>,

    /// Records per package
    records: CacheMap<String, Vec<RepoDataRecord>, FetchRecordsError>,
}

#[derive(Debug, Clone, Error)]
pub struct FetchRecordsError {}

impl Subdir {
    /// Constructs a new subdir from a source.
    pub fn new(source: SubdirSource) -> Self {
        Self {
            source: Arc::new(source),
            records: Default::default(),
        }
    }
}
