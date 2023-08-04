use std::sync::Arc;
use rattler_conda_types::sparse_index::SparseIndexNames;
use rattler_networking::AuthenticatedClient;
use url::Url;

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
