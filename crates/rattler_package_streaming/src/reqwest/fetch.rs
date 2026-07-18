//! High-level helpers to fetch single files from remote conda packages.
//!
//! Thin wrappers around [`crate::archive::PackageArchive`]: sparse range
//! requests where supported, transparent fallback to a one-time spooled
//! download otherwise. To read multiple files from one package, open a
//! [`crate::archive::PackageArchive`] once instead.

use rattler_conda_types::package::PackageFile;
use reqwest_middleware::ClientWithMiddleware;
use url::Url;

pub use super::full_download::{
    fetch_file_from_remote_full_download, fetch_package_file_full_download,
};
use crate::ExtractError;
use crate::archive::PackageArchive;

/// Fetch and parse a typed [`PackageFile`] from a remote package.
///
/// # Example
///
/// ```rust,no_run
/// # #[tokio::main]
/// # async fn main() {
/// use rattler_conda_types::package::IndexJson;
/// use rattler_package_streaming::reqwest::fetch::fetch_package_file_from_remote_url;
/// use reqwest::Client;
/// use reqwest_middleware::ClientWithMiddleware;
/// use url::Url;
///
/// let client = ClientWithMiddleware::from(Client::new());
/// let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/python-3.10.8-h4a9ceb5_0_cpython.conda").unwrap();
///
/// let index_json: IndexJson = fetch_package_file_from_remote_url(client, url)
///     .await
///     .unwrap();
/// # }
/// ```
pub async fn fetch_package_file_from_remote_url<P: PackageFile>(
    client: ClientWithMiddleware,
    url: Url,
) -> Result<P, ExtractError> {
    PackageArchive::from_url(client, url)
        .await?
        .read_package_file()
        .await
}

/// Fetch the raw bytes for a file path inside a remote package.
/// Returns `Ok(None)` when the path does not exist in the archive.
pub async fn fetch_file_from_remote_url(
    client: ClientWithMiddleware,
    url: Url,
    target_path: &std::path::Path,
) -> Result<Option<Vec<u8>>, ExtractError> {
    PackageArchive::from_url(client, url)
        .await?
        .read_file(target_path)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reqwest::test_server;

    fn test_file() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/clobber/clobber-fd-1-0.1.0-h4616a5c_0.conda")
    }

    #[tokio::test]
    async fn test_fetch_index_json() {
        use rattler_conda_types::package::IndexJson;

        let url = test_server::serve_file(test_file()).await;
        let client = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());

        let index_json: IndexJson = fetch_package_file_from_remote_url(client, url)
            .await
            .unwrap();

        insta::assert_yaml_snapshot!(index_json);
    }

    #[tokio::test]
    async fn test_fetch_about_json() {
        use rattler_conda_types::package::AboutJson;

        let url = test_server::serve_file(test_file()).await;
        let client = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());

        let about_json: AboutJson = fetch_package_file_from_remote_url(client, url)
            .await
            .unwrap();

        insta::assert_yaml_snapshot!(about_json);
    }

    /// tar.bz2 is unsupported by the sparse path, so the archive is spooled.
    #[tokio::test]
    async fn test_fetch_full_download_tar_bz2() {
        use rattler_conda_types::package::IndexJson;

        let tar_bz2 = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/clobber/clobber-1-0.1.0-h4616a5c_0.tar.bz2");
        let url = test_server::serve_file(tar_bz2).await;
        let client = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());

        let index_json: IndexJson = fetch_package_file_from_remote_url(client, url)
            .await
            .unwrap();

        insta::assert_yaml_snapshot!(index_json);
    }

    /// Exercise the streaming `.conda` full-download path directly.
    #[tokio::test]
    async fn test_fetch_full_download_conda() {
        use rattler_conda_types::package::IndexJson;

        let url = test_server::serve_file(test_file()).await;
        let client = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());

        let index_json: IndexJson = fetch_package_file_full_download(&client, &url)
            .await
            .unwrap();

        insta::assert_yaml_snapshot!(index_json);
    }

    #[tokio::test]
    async fn test_fetch_file_from_remote() {
        let url = test_server::serve_file(test_file()).await;
        let client = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());

        let raw = fetch_file_from_remote_url(client, url, std::path::Path::new("info/index.json"))
            .await
            .unwrap()
            .expect("file should exist in archive");

        assert!(!raw.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_file_from_remote_tar_bz2_fallback() {
        let tar_bz2 = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/clobber/clobber-1-0.1.0-h4616a5c_0.tar.bz2");
        let url = test_server::serve_file(tar_bz2).await;
        let client = reqwest_middleware::ClientWithMiddleware::from(reqwest::Client::new());

        let raw = fetch_file_from_remote_url(client, url, std::path::Path::new("info/index.json"))
            .await
            .unwrap()
            .expect("file should exist in archive");

        assert!(!raw.is_empty());
    }
}
