use crate::sparse_index::gateway::parse_sparse_index_package;
use crate::sparse_index::GatewayError;
use futures::TryFutureExt;
use http::StatusCode;
use rattler_conda_types::sparse_index::{
    sparse_index_filename, SparseIndexDependencies, SparseIndexNames,
};
use rattler_conda_types::{Channel, Platform, RepoDataRecord};
use rattler_networking::AuthenticatedClient;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::try_join;
use tracing::instrument;
use url::Url;

/// A possible error returned by [`RemoteSparseIndex::new`].
#[derive(Error, Debug)]
pub enum RemoteSparseIndexError {
    #[error("failed to fetch `names` from remote channel at {0}")]
    FetchNames(Url, #[source] FetchNamesError),

    #[error("failed to fetch `dependencies` from remote channel at {0}")]
    FetchDependencies(Url, #[source] FetchDependenciesError),
}

/// A sparse index over http.
pub struct RemoteSparseIndex {
    /// The client to use for fetching records
    client: AuthenticatedClient,

    /// Package names and their corresponding hashes.
    names: SparseIndexNames,

    /// Package dependencies
    dependencies: Option<SparseIndexDependencies>,

    /// The root url (`http(s)?://channel/platform/`)
    root: Url,

    /// The name of the channel
    channel_name: Arc<str>,

    /// The cache directory
    cache_dir: PathBuf,
}

impl RemoteSparseIndex {
    pub async fn new(
        client: AuthenticatedClient,
        cache_dir: PathBuf,
        channel: Channel,
        platform: Platform,
    ) -> Result<Self, RemoteSparseIndexError> {
        let base_url = channel.platform_url(platform);

        // Fetch the `names` and `dependencies` file from the remote
        let (dependencies, names) = try_join!(
            // `dependencies`
            fetch_dependencies(&client, &cache_dir, base_url.clone()).map_err(|source| {
                RemoteSparseIndexError::FetchDependencies(base_url.clone(), source)
            }),
            // `names`
            fetch_names(&client, &cache_dir, base_url.clone())
                .map_err(|source| RemoteSparseIndexError::FetchNames(base_url.clone(), source))
        )?;

        Ok(Self {
            client,
            names,
            root: base_url,
            channel_name: Arc::from(channel.canonical_name()),
            cache_dir,
            dependencies,
        })
    }

    /// Returns true if this source contains information about the specified package
    pub fn contains(&self, package_name: &str) -> bool {
        self.names.names.contains_key(package_name)
    }

    /// Returns hints on which packages to prefetch for package with the given name. This method
    /// should be used to determine which dependent packages to fetch without actually fetching
    /// the metadata of the package.
    ///
    /// Package records will still be fetched and inspected so the package names returned from this
    /// function may be incorrect.
    pub fn prefetch_hints(&self, package_name: &str) -> Vec<String> {
        self.dependencies
            .as_ref()
            .map(|deps| deps.dependencies.get(package_name))
            .flatten()
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }

    /// Fetch information about the specified package.
    #[instrument(skip(self), fields(channel=%self.root))]
    pub async fn fetch_records(
        &self,
        package_name: &str,
    ) -> Result<Vec<RepoDataRecord>, GatewayError> {
        // Check if this subdirectory actually contains the specified package name. If not, we can
        // immediately ignore it.
        if !self.contains(package_name) {
            return Ok(vec![]);
        }

        let fetch_start = Instant::now();

        // Determine the url for the package
        let file_name =
            sparse_index_filename(package_name).expect("package name cannot be invalid");
        let file_url = self
            .root
            .join(&file_name.to_string_lossy())
            .expect("url must be valid");

        // Get the data from the server
        let (status, body) =
            super::super::http::get(&self.client, &self.cache_dir, file_url.clone()).await?;
        if !status.is_success() {
            return Err(GatewayError::HttpStatus(status, file_url));
        }

        let fetch_end = Instant::now();
        println!(
            "fetched '{package_name} from {} in {} ms",
            &self.root,
            (fetch_end - fetch_start).as_millis()
        );

        // Decode the info
        parse_sparse_index_package(self.channel_name.clone(), self.root.clone(), body).await
    }
}

/// An error that can be returned by [`fetch_names`].
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
    cache_dir: &Path,
    root: Url,
) -> Result<SparseIndexNames, FetchNamesError> {
    let names_url = root.join("names").unwrap();
    let (status_code, mut names_body) =
        super::super::http::get(client, cache_dir, names_url.clone())
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

/// An error that can be returned by [`fetch_dependencies`].
#[derive(Error, Debug)]
pub enum FetchDependenciesError {
    #[error(transparent)]
    HttpError(#[from] super::super::http::HttpError),

    #[error(transparent)]
    TransportError(#[from] std::io::Error),

    #[error("http error {0} for {1}")]
    HttpStatus(StatusCode, Url),

    #[error(transparent)]
    ParseError(std::io::Error),
}

/// Fetches the [`SparseIndexDependencies`] from a remote server.
async fn fetch_dependencies(
    client: &AuthenticatedClient,
    cache_dir: &Path,
    root: Url,
) -> Result<Option<SparseIndexDependencies>, FetchDependenciesError> {
    let names_url = root.join("dependencies").unwrap();
    let (status_code, mut names_body) =
        super::super::http::get(client, cache_dir, names_url.clone())
            .await
            .map_err(FetchDependenciesError::from)?;

    // Its Ok if the dependencies file is missing
    if status_code == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    // Any other error is an error
    if !status_code.is_success() {
        return Err(FetchDependenciesError::HttpStatus(status_code, names_url));
    }

    // Parse the file
    let mut names_bytes = Vec::new();
    names_body
        .read_to_end(&mut names_bytes)
        .await
        .map_err(FetchDependenciesError::from)?;
    let names = SparseIndexDependencies::from_bytes(&names_bytes)
        .map_err(FetchDependenciesError::ParseError)?;
    Ok(Some(names))
}
