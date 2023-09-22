use crate::sparse::SparseRepoData;
use rattler_conda_types::{Channel, Platform};
use rattler_networking::AuthenticatedClient;
use std::any::Any;
use std::path::PathBuf;
use thiserror::Error;
use tokio::task::JoinError;
use url::Url;

pub enum SubdirSource {
    // LocalSparseIndex(local::LocalSparseIndex),
    // RemoteSparseIndex(remote::RemoteSparseIndex),
    SparseRepoData(SparseRepoData),
}

#[derive(Debug, Error)]
pub enum SubdirSourceError {
    #[error("{0} does not refer to a valid path")]
    InvalidPath(Url),

    #[error("unknown protocol for {0}. Only `http`, `https`, or `file` schemes")]
    InvalidUrl(Url),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("'{0}' does not contain any repodata")]
    PathDoesNotContainRepoData(PathBuf),

    #[error("the operation was cancelled")]
    Cancelled,
}

impl From<JoinError> for SubdirSourceError {
    fn from(value: JoinError) -> Self {
        match value.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => SubdirSourceError::Cancelled,
        }
    }
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
            return Ok(Self::from_path(root, channel, platform));
        }

        // Http based scheme?
        if platform_url.scheme() == "http" || platform_url.scheme() == "https" {
            unreachable!()
        }

        Err(SubdirSourceError::InvalidUrl(platform_url))
    }

    /// This asynchronous function creates a new instance. The function acts differently based on
    /// whether the provided path is a file or a directory.
    ///
    /// If the `path` refers to a directory, the function checks if the directory contains a file
    /// called "repodata.json". If it does not, it triggers a
    /// `SubdirSourceError::PathDoesNotContainRepoData` error.
    ///
    /// If the path refers to a file containing a "repodata.json", the function sparsely reads the
    /// contents of the repodata file which can be used to quickly answer specific queries about the
    /// data.
    pub async fn from_path(
        path: PathBuf,
        channel: Channel,
        platform: Platform,
    ) -> Result<Self, SubdirSourceError> {
        // If the path refers to a directory make sure it contains repodata.
        let repodata_path = if path.is_dir() {
            let repodata_path = path.join("repodata.json");
            if !repodata_path.is_file() {
                return Err(SubdirSourceError::PathDoesNotContainRepoData(path));
            } else {
                repodata_path
            }
        } else {
            path
        };

        // Sparsely read the contents of the repodata.
        let sparse_repo_data = tokio::task::spawn_blocking(move || {
            SparseRepoData::new(channel, platform, repodata_path, None)
        })
        .await??;

        Ok(SubdirSource::SparseRepoData(sparse_repo_data))
    }
}
