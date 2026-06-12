//! Functionality to stream and extract packages directly from a
//! [`reqwest::Url`] within a [`tokio`] async context.

use std::{path::Path, sync::Arc};

use fs_err::tokio as tokio_fs;
use futures_util::stream::TryStreamExt;
use rattler_conda_types::package::CondaArchiveType;
use rattler_digest::Sha256Hash;
use reqwest::Response;
use tokio::io::BufReader;
use tokio_util::{either::Either, io::StreamReader};
use url::Url;

use crate::{
    DownloadReporter, ExtractError, ExtractResult,
    spooled_pipe::{SpooledPipeReader, copy_to_pipe, spooled_pipe},
};

/// The buffer size used to chunk the download into the spooled pipe (128KB).
const DEFAULT_BUF_SIZE: usize = 128 * 1024;

/// The amount of package data that is buffered in memory while downloading;
/// data past this limit spills to an unnamed temporary file.
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
) -> Result<impl tokio::io::AsyncRead + Unpin, ExtractError> {
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

/// Streams `reader` into a spooled pipe while `extract` consumes the other
/// end synchronously on a blocking thread, so the download and the
/// extraction overlap.
///
/// The two halves are deliberately decoupled: the download never waits for
/// the extractor (data the extractor has not consumed yet is buffered in
/// memory up to [`SPOOL_MEMORY_LIMIT`] and spills to a temporary file beyond
/// that). Reading from the network stream on the extraction thread directly
/// would stall the HTTP/2 stream whenever the extractor falls behind, which
/// servers answer with stream resets under concurrent load. The extractor in
/// turn only ever touches plain memory and a plain [`std::fs::File`], never
/// the async runtime.
///
/// Both halves always run to completion so no detached extraction keeps
/// writing to the destination after this function returns. A download error
/// is forwarded into the pipe (failing the extraction at the point the data
/// ran out) and takes precedence as it is the root cause and remains
/// retryable.
async fn download_and_extract<F>(
    reader: impl tokio::io::AsyncRead + Unpin,
    reporter: Option<Arc<dyn DownloadReporter>>,
    extract: F,
) -> Result<ExtractResult, ExtractError>
where
    F: FnOnce(&mut SpooledPipeReader) -> Result<ExtractResult, ExtractError> + Send + 'static,
{
    let (writer, mut pipe_reader) = spooled_pipe(SPOOL_MEMORY_LIMIT);

    let download = async {
        copy_to_pipe(reader, writer, DEFAULT_BUF_SIZE)
            .await
            .map_err(ExtractError::IoError)?;
        if let Some(reporter) = &reporter {
            reporter.on_download_complete();
        }
        Ok(())
    };

    let extraction = async {
        match tokio::task::spawn_blocking(move || extract(&mut pipe_reader)).await {
            Ok(result) => result,
            Err(err) => {
                if let Ok(panic) = err.try_into_panic() {
                    std::panic::resume_unwind(panic);
                }
                Err(ExtractError::Cancelled)
            }
        }
    };

    match futures_util::future::join(download, extraction).await {
        (_, Ok(result)) => Ok(result),
        (Err(download_error), Err(_)) => Err(download_error),
        (Ok(()), Err(extract_error)) => Err(extract_error),
    }
}

/// Extracts the contents a `.tar.bz2` package archive from the specified remote
/// location.
///
/// The package data is decompressed and written to disk synchronously on a
/// blocking thread while it is being downloaded, which avoids the per-file
/// overhead of asynchronous filesystem operations.
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
    let destination = destination.to_owned();
    download_and_extract(reader, reporter, move |pipe| {
        crate::read::extract_tar_bz2(pipe, &destination)
    })
    .await
}

/// Extracts the contents a `.conda` package archive from the specified remote
/// location.
///
/// The package data is decompressed and written to disk synchronously on a
/// blocking thread while it is being downloaded, which avoids the per-file
/// overhead of asynchronous filesystem operations. Archives that use zip
/// data descriptors cannot be extracted in a streaming fashion
/// (<https://github.com/conda/rattler/issues/794>); for those the buffered
/// package data is re-read from the start instead of being downloaded a
/// second time.
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
    let destination = destination.to_owned();
    download_and_extract(reader, reporter, move |pipe| {
        crate::read::extract_conda(pipe, &destination)
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
