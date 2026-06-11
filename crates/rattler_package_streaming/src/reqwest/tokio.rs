//! Functionality to stream and extract packages directly from a
//! [`reqwest::Url`] within a [`tokio`] async context.

use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::Arc,
};

use async_spooled_tempfile::{SpooledData, SpooledTempFile};
use fs_err::tokio as tokio_fs;
use futures_util::stream::TryStreamExt;
use rattler_conda_types::package::CondaArchiveType;
use rattler_digest::Sha256Hash;
use reqwest::Response;
use tokio::io::BufReader;
use tokio_util::{either::Either, io::StreamReader};
use tracing;
use url::Url;
use zip::result::ZipError;

use crate::{DownloadReporter, ExtractError, ExtractResult};

/// zip files may use data descriptors to signal that the decompressor needs to
/// seek ahead in the buffer to find the compressed data length.
/// Since we stream the package over a non seek-able HTTP connection, this
/// condition will cause an error during decompression. In this case, we
/// fallback to reading the whole data to a buffer before attempting
/// decompression. Read more in <https://github.com/conda/rattler/issues/794>
const DATA_DESCRIPTOR_ERROR_MESSAGE: &str = "The file length is not available in the local header";

/// The buffer size used for I/O chunking (128KB).
const DEFAULT_BUF_SIZE: usize = 128 * 1024;

/// The amount of package data that is buffered in memory while downloading;
/// larger packages spill to a temporary file.
const SPOOL_MEMORY_LIMIT: usize = 5 * 1024 * 1024;

fn error_for_status(response: reqwest::Response) -> reqwest_middleware::Result<Response> {
    response
        .error_for_status()
        .map_err(reqwest_middleware::Error::Reqwest)
}

async fn get_reader(
    url: Url,
    client: reqwest_middleware::ClientWithMiddleware,
    expected_sha256: Option<Sha256Hash>,
    reporter: Option<Arc<dyn DownloadReporter>>,
) -> Result<impl tokio::io::AsyncRead + Send + Unpin + 'static, ExtractError> {
    if let Some(reporter) = &reporter {
        reporter.on_download_start();
    }

    if url.scheme() == "file" {
        let file =
            tokio_fs::File::open(url.to_file_path().expect("Could not convert to file path"))
                .await
                .map_err(ExtractError::IoError)?;

        Ok(Either::Left(BufReader::new(file)))
    } else {
        // Send the request for the file
        let mut request = client.get(url.clone());

        if let Some(sha256) = expected_sha256 {
            // This is used by the OCI registry middleware to verify the sha256 of the
            // response
            request = request.header("X-Expected-Sha256", hex::encode(sha256));
        }

        let response = request
            .send()
            .await
            .and_then(error_for_status)
            .map_err(ExtractError::ReqwestError)?;

        let total_bytes = response.content_length();
        let mut bytes_received = Box::new(0);
        let byte_stream = response.bytes_stream().inspect_ok(move |frame| {
            *bytes_received += frame.len() as u64;
            if let Some(reporter) = &reporter {
                reporter.on_download_progress(*bytes_received, total_bytes);
            }
        });

        // Get the response as a stream
        Ok(Either::Right(StreamReader::new(byte_stream.map_err(
            |err| {
                if err.is_body() {
                    std::io::Error::new(std::io::ErrorKind::Interrupted, err)
                } else if err.is_decode() {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
                } else {
                    std::io::Error::other(err)
                }
            },
        ))))
    }
}

/// A synchronous reader over fully downloaded package data, either in memory
/// or in an unnamed temporary file.
enum SpoolReader {
    Memory(std::io::Cursor<Vec<u8>>),
    Disk(std::fs::File),
}

impl Read for SpoolReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            SpoolReader::Memory(cursor) => cursor.read(buf),
            SpoolReader::Disk(file) => file.read(buf),
        }
    }
}

impl Seek for SpoolReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            SpoolReader::Memory(cursor) => cursor.seek(pos),
            SpoolReader::Disk(file) => file.seek(pos),
        }
    }
}

/// Drains the given reader into a spooled temporary buffer (in memory up to
/// [`SPOOL_MEMORY_LIMIT`], on disk beyond that) and returns a synchronous
/// reader over the downloaded data, positioned at the start.
///
/// Downloading to a local buffer first means the HTTP stream is always
/// consumed at network speed. If extraction were to read from the network
/// stream directly, a slow extractor would stall the HTTP/2 stream long
/// enough for servers to reset it (`stream error: unexpected internal
/// error`), particularly when many packages are fetched concurrently on a
/// saturated CPU.
async fn download_to_spool(
    reader: impl tokio::io::AsyncRead + Send + Unpin + 'static,
) -> Result<SpoolReader, ExtractError> {
    let mut spool = SpooledTempFile::new(SPOOL_MEMORY_LIMIT);
    let mut buffered = BufReader::with_capacity(DEFAULT_BUF_SIZE, reader);
    tokio::io::copy_buf(&mut buffered, &mut spool)
        .await
        .map_err(ExtractError::IoError)?;

    match spool.into_inner().await.map_err(ExtractError::IoError)? {
        SpooledData::InMemory(mut cursor) => {
            cursor.set_position(0);
            Ok(SpoolReader::Memory(cursor))
        }
        SpooledData::OnDisk(file) => {
            let mut file = file.into_std().await;
            file.seek(SeekFrom::Start(0))
                .map_err(ExtractError::IoError)?;
            Ok(SpoolReader::Disk(file))
        }
    }
}

/// Runs the provided extraction closure on a blocking thread.
async fn run_blocking_extract<F>(f: F) -> Result<ExtractResult, ExtractError>
where
    F: FnOnce() -> Result<ExtractResult, ExtractError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(err) => {
            if let Ok(panic) = err.try_into_panic() {
                std::panic::resume_unwind(panic);
            }
            Err(ExtractError::Cancelled)
        }
    }
}

/// Extracts the contents a `.tar.bz2` package archive from the specified remote
/// location.
///
/// The package data is first downloaded into a spooled buffer (in memory for
/// small packages, a temporary file for large ones), after which
/// decompression, hashing and writing the files to disk happen synchronously
/// on a blocking thread. This avoids the per-file overhead of asynchronous
/// filesystem operations which dominates extraction time for archives with
/// many files.
///
/// ```rust,no_run
/// # #[tokio::main]
/// # async fn main() {
/// # use std::path::Path;
/// use url::Url;
/// use reqwest::Client;
/// use reqwest_middleware::ClientWithMiddleware;
/// use rattler_package_streaming::reqwest::tokio::extract_tar_bz2;
/// let _ = extract_tar_bz2(
///     ClientWithMiddleware::from(Client::new()),
///     Url::parse("https://conda.anaconda.org/conda-forge/win-64/python-3.11.0-hcf16a7b_0_cpython.tar.bz2").unwrap(),
///     Path::new("/tmp"),
///     None,
///     None)
///     .await
///     .unwrap();
/// # }
/// ```
pub async fn extract_tar_bz2(
    client: reqwest_middleware::ClientWithMiddleware,
    url: Url,
    destination: &Path,
    expected_sha256: Option<Sha256Hash>,
    reporter: Option<Arc<dyn DownloadReporter>>,
) -> Result<ExtractResult, ExtractError> {
    let reader = get_reader(url.clone(), client, expected_sha256, reporter.clone()).await?;
    let spool = download_to_spool(reader).await?;
    if let Some(reporter) = &reporter {
        reporter.on_download_complete();
    }

    // Extract from the local buffer on a blocking thread. Skip the MD5
    // computation when we already have a sha256 hash to verify.
    let compute_md5 = expected_sha256.is_none();
    let destination = destination.to_owned();
    run_blocking_extract(move || {
        crate::read::extract_tar_bz2_with_options(
            std::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, spool),
            &destination,
            compute_md5,
        )
    })
    .await
}

/// Extracts the contents a `.conda` package archive from the specified remote
/// location.
///
/// The package data is first downloaded into a spooled buffer (in memory for
/// small packages, a temporary file for large ones), after which
/// decompression, hashing and writing the files to disk happen synchronously
/// on a blocking thread. This avoids the per-file overhead of asynchronous
/// filesystem operations which dominates extraction time for archives with
/// many files.
///
/// ```rust,no_run
/// # #[tokio::main]
/// # async fn main() {
/// # use std::path::Path;
/// use rattler_package_streaming::reqwest::tokio::extract_conda;
/// use reqwest::Client;
/// use reqwest_middleware::ClientWithMiddleware;
/// use url::Url;
/// let _ = extract_conda(
///     ClientWithMiddleware::from(Client::new()),
///     Url::parse("https://conda.anaconda.org/conda-forge/linux-64/python-3.10.8-h4a9ceb5_0_cpython.conda").unwrap(),
///     Path::new("/tmp"),
///     None,
///     None)
///     .await
///     .unwrap();
/// # }
/// ```
pub async fn extract_conda(
    client: reqwest_middleware::ClientWithMiddleware,
    url: Url,
    destination: &Path,
    expected_sha256: Option<Sha256Hash>,
    reporter: Option<Arc<dyn DownloadReporter>>,
) -> Result<ExtractResult, ExtractError> {
    let reader = get_reader(url.clone(), client, expected_sha256, reporter.clone()).await?;
    let mut spool = download_to_spool(reader).await?;
    if let Some(reporter) = &reporter {
        reporter.on_download_complete();
    }

    // Extract from the local buffer on a blocking thread. Since the package
    // data is buffered locally, the zip data-descriptor fallback
    // (https://github.com/conda/rattler/issues/794) can rewind and retry
    // without downloading the package a second time. Skip the MD5
    // computation when we already have a sha256 hash to verify.
    let compute_md5 = expected_sha256.is_none();
    let destination = destination.to_owned();
    let url_for_log = url.clone();
    run_blocking_extract(move || {
        let result = crate::read::extract_conda_via_streaming_with_options(
            std::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, &mut spool),
            &destination,
            compute_md5,
        );

        match result {
            Err(ExtractError::ZipError(ZipError::UnsupportedArchive(zip_error)))
                if (zip_error.contains(DATA_DESCRIPTOR_ERROR_MESSAGE)) =>
            {
                tracing::warn!(
                    "Failed to stream decompress conda package from '{}' due to the presence of zip data descriptors. Falling back to non streaming decompression",
                    url_for_log
                );
                spool.seek(SeekFrom::Start(0))?;
                crate::read::extract_conda_via_buffering_with_options(
                    std::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, spool),
                    &destination,
                    compute_md5,
                )
            }
            other => other,
        }
    })
    .await
}

/// Extracts the contents a package archive from the specified remote location.
/// The type of package is determined based on the path of the url.
///
/// ```rust,no_run
/// # #[tokio::main]
/// # async fn main() {
/// # use std::path::Path;
/// use url::Url;
/// use rattler_package_streaming::reqwest::tokio::extract;
/// use reqwest::Client;
/// use reqwest_middleware::ClientWithMiddleware;
/// let _ = extract(
///     ClientWithMiddleware::from(Client::new()),
///     Url::parse("https://conda.anaconda.org/conda-forge/linux-64/python-3.10.8-h4a9ceb5_0_cpython.conda").unwrap(),
///     Path::new("/tmp"),
///     None,
///     None)
///     .await
///     .unwrap();
/// # }
/// ```
pub async fn extract(
    client: reqwest_middleware::ClientWithMiddleware,
    url: Url,
    destination: &Path,
    expected_sha256: Option<Sha256Hash>,
    reporter: Option<Arc<dyn DownloadReporter>>,
) -> Result<ExtractResult, ExtractError> {
    match CondaArchiveType::try_from(Path::new(url.path()))
        .ok_or(ExtractError::UnsupportedArchiveType)?
    {
        CondaArchiveType::TarBz2 => {
            extract_tar_bz2(client, url, destination, expected_sha256, reporter).await
        }
        CondaArchiveType::Conda => {
            extract_conda(client, url, destination, expected_sha256, reporter).await
        }
    }
}
