use http::StatusCode;
use rattler_conda_types::sparse_index::SparseIndexNames;
use rattler_conda_types::{Channel, Platform, RepoDataRecord};
use rattler_networking::AuthenticatedClient;
use std::fmt::{Display};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use url::Url;
use crate::sparse_index::GatewayError;

#[derive(Error, Debug)]
pub enum RemoteSparseIndexError {
    #[error("failed to fetch `names` from remote channel at {0}")]
    FetchNames(Url, #[source] FetchNamesError),
}

/// A sparse index over http.
pub struct RemoteSparseIndex {
    /// The client to use for fetching records
    client: AuthenticatedClient,

    /// Package names and their corresponding hashes.
    names: SparseIndexNames,

    /// The root url (`http(s)?://channel/platform/`)
    root: Url,

    /// The name of the channel
    channel_name: Arc<str>,
}

impl RemoteSparseIndex {
    pub async fn new(
        client: AuthenticatedClient,
        cache_dir: PathBuf,
        channel: Channel,
        platform: Platform,
    ) -> Result<Self, RemoteSparseIndexError> {
        let base_url = channel.platform_url(platform);

        // Fetch the `names` file from the remote
        let names = fetch_names(&client, &cache_dir, base_url.clone())
            .await
            .map_err(|source| RemoteSparseIndexError::FetchNames(base_url.clone(), source))?;

        Ok(Self {
            client,
            names,
            root: base_url,
            channel_name: Arc::from(channel.canonical_name()),
        })
    }

    /// Returns true if this source contains information about the specified package
    pub fn contains(&self, package_name: &str) -> bool {
        self.names.names.contains_key(package_name)
    }

    /// Fetch information about the specified package.
    pub async fn fetch_records(
        &self,
        package_name: &str,
    ) -> Result<Vec<RepoDataRecord>, GatewayError> {
        // Check if this subdirectory actually contains the specified package name. If not, we can
        // immediately ignore it.
        if !self.contains(package_name) {
            return Ok(vec![]);
        }


        Ok(vec![])
    }
}

#[derive(Error, Debug)]
pub enum FetchNamesError {
    #[error(transparent)]
    HttpError(#[from] super::super::http::HttpError),

    #[error(transparent)]
    TransportError(#[from] std::io::Error),

    #[error("http error {0} for {1}")]
    HttpStatus(StatusCode, Url),

    #[error(transparent)]
    ParseError(std::io::Error),
}

/// Fetches the [`SparseIndexNames`] from a remote server.
async fn fetch_names(
    client: &AuthenticatedClient,
    cache_dir: &PathBuf,
    root: Url,
) -> Result<SparseIndexNames, FetchNamesError> {
    let names_url = root.join("names").unwrap();
    let (status_code, mut names_body) =
        super::super::http::get(&client, &cache_dir, names_url.clone())
            .await
            .map_err(FetchNamesError::from)?;
    if !status_code.is_success() {
        return Err(FetchNamesError::HttpStatus(status_code, names_url));
    }

    // Parse the file
    let mut names_bytes = Vec::new();
    names_body
        .read_to_end(&mut names_bytes)
        .await
        .map_err(FetchNamesError::from)?;
    let names = SparseIndexNames::from_bytes(&names_bytes).map_err(FetchNamesError::ParseError)?;
    Ok(names)
}
