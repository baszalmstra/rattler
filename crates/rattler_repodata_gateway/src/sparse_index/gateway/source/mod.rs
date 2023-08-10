use crate::sparse_index::gateway::source::remote::{RemoteSparseIndex, RemoteSparseIndexError};
use rattler_conda_types::{Channel, Platform};
use rattler_networking::AuthenticatedClient;
use std::path::PathBuf;
use thiserror::Error;
use url::Url;

mod local;
mod remote;

pub enum SubdirSource {
    LocalSparseIndex(local::LocalSparseIndex),
    RemoteSparseIndex(remote::RemoteSparseIndex),
}

#[derive(Debug, Error)]
pub enum SubdirSourceError {
    #[error(transparent)]
    Remote(#[from] RemoteSparseIndexError),

    #[error("{0} does not refer to a valid path")]
    InvalidPath(Url),

    #[error("unknown protocol for {0}. Only `http`, `https`, or `file` schemes")]
    InvalidUrl(Url),
}

impl SubdirSource {
    pub async fn new(
        client: AuthenticatedClient,
        cache_dir: PathBuf,
        channel: Channel,
        platform: Platform,
    ) -> Result<Self, SubdirSourceError> {
        // Determine the type of source of the channel based on the URL scheme.
        let platform_url = channel.platform_url(platform);

        // File based scheme?
        if platform_url.scheme() == "file" {
            let root = platform_url
                .to_file_path()
                .map_err(|_| SubdirSourceError::InvalidPath(platform_url))?;
            return Ok(Self::from_path(root));
        }

        // Http based scheme?
        if platform_url.scheme() == "http" || platform_url.scheme() == "https" {
            return Ok(SubdirSource::RemoteSparseIndex(
                RemoteSparseIndex::new(client, cache_dir, channel, platform).await?,
            ));
        }

        Err(SubdirSourceError::InvalidUrl(platform_url))
    }

    /// Constructs a new instance from a local directory.
    pub fn from_path(path: PathBuf) -> Self {
        SubdirSource::LocalSparseIndex(local::LocalSparseIndex::new(path))
    }
}
