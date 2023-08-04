use crate::sparse_index::gateway::parse_sparse_index_package;
use crate::sparse_index::GatewayError;
use futures::TryStreamExt;
use rattler_conda_types::sparse_index::sparse_index_filename;
use rattler_conda_types::RepoDataRecord;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::BufReader;
use url::Url;

/// A local directory containing a sparse index.
pub struct LocalSparseIndex {
    pub root: PathBuf,
    pub channel_name: Arc<str>,
}

impl LocalSparseIndex {
    /// Fetch information about the specified package.
    pub async fn fetch_records(
        &self,
        package_name: &str,
    ) -> Result<Vec<RepoDataRecord>, GatewayError> {
        let package_path = self
            .root
            .join(sparse_index_filename(&package_name).unwrap());
        let platform_url = Url::from_directory_path(&self.root)
            .expect("platform path must refer to a valid directory");

        // Read the file from disk. If the file is not found we simply return no records.
        let file = match tokio::fs::File::open(&package_path).await {
            Ok(file) => BufReader::new(file),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(GatewayError::IoError(Arc::new(e))),
        };

        // Deserialize each line individually
        parse_sparse_index_package(self.channel_name.clone(), platform_url, file).await
    }
}
