use super::{parse_sparse_index_package_stream, GatewayError};
use futures::TryStreamExt;
use rattler_conda_types::sparse_index::sparse_index_filename;
use rattler_conda_types::RepoDataRecord;
use std::{path::PathBuf, sync::Arc};
use tokio::io::BufReader;
use url::Url;

/// Try to read [`RepoDataRecord`]s from a SparseIndexPackage file on disk.
pub async fn fetch_from_local_channel(
    channel_name: Arc<str>,
    package_name: &str,
    platform_path: PathBuf,
) -> Result<Vec<RepoDataRecord>, GatewayError> {
    let package_path = platform_path.join(sparse_index_filename(&package_name).unwrap());
    let platform_url = Url::from_directory_path(platform_path)
        .expect("platform path must refer to a valid directory");

    // Read the file from disk. If the file is not found we simply return no records.
    let file = match tokio::fs::File::open(&package_path).await {
        Ok(file) => BufReader::new(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(GatewayError::IoError(Arc::new(e))),
    };

    // Deserialize each line individually
    parse_sparse_index_package_stream(file)
        .map_ok(move |record| RepoDataRecord {
            package_record: record.package_record,
            url: platform_url
                .join(&record.file_name)
                .expect("must be able to append a filename"),
            file_name: record.file_name,
            channel: channel_name.clone(),
        })
        .try_collect()
        .await
}
