//! Functionality to stream and extract packages directly from a
//! [`reqwest::Url`] within a [`tokio`] async context.

use std::{io::Write, path::Path, sync::Arc};

use futures_util::stream::TryStreamExt;
use rattler_conda_types::package::CondaArchiveType;
use rattler_digest::Sha256Hash;
use reqwest::Response;
use tempfile::SpooledTempFile;
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;
use url::Url;
use zip::result::ZipError;

use crate::{
    DownloadReporter, ExtractError, ExtractResult,
    read::{
        DATA_DESCRIPTOR_ERROR_MESSAGE, ZIP_LOCAL_HEADER_LEN, local_header_uses_data_descriptor,
    },
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

/// A [`DownloadReporter`] for a download that has started: constructing one
/// emits `on_download_start`, and consuming it with
/// [`StartedDownloadReporter::complete`] emits `on_download_complete`. This
/// keeps the started-state in the type instead of spread over the code paths
/// that report progress or completion.
#[derive(Clone)]
struct StartedDownloadReporter(Option<Arc<dyn DownloadReporter>>);

impl StartedDownloadReporter {
    /// Reports the start of the download and returns the reporter for the
    /// remainder of the download lifecycle.
    fn start(reporter: Option<Arc<dyn DownloadReporter>>) -> Self {
        if let Some(reporter) = &reporter {
            reporter.on_download_start();
        }
        Self(reporter)
    }

    /// Reports download progress.
    fn on_progress(&self, bytes_received: u64, total_bytes: Option<u64>) {
        if let Some(reporter) = &self.0 {
            reporter.on_download_progress(bytes_received, total_bytes);
        }
    }

    /// Reports the completion of the download.
    fn complete(self) {
        if let Some(reporter) = &self.0 {
            reporter.on_download_complete();
        }
    }
}

async fn get_reader(
    url: Url,
    client: reqwest_middleware::ClientWithMiddleware,
    expected_sha256: Option<Sha256Hash>,
    reporter: StartedDownloadReporter,
) -> Result<impl tokio::io::AsyncRead + Unpin, ExtractError> {
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
        reporter.on_progress(*bytes_received, total_bytes);
    });

    // Get the response as a stream
    Ok(StreamReader::new(byte_stream.map_err(|err| {
        if err.is_body() {
            std::io::Error::new(std::io::ErrorKind::Interrupted, err)
        } else if err.is_decode() {
            std::io::Error::new(std::io::ErrorKind::InvalidData, err)
        } else {
            std::io::Error::other(err)
        }
    })))
}

/// Extracts a `file://` URL by reading the (seekable) package straight from
/// disk on a blocking thread; piping a local file through the download
/// machinery would only add a redundant copy of the data.
async fn extract_local_file<F>(
    url: &Url,
    reporter: StartedDownloadReporter,
    extract: F,
) -> Result<ExtractResult, ExtractError>
where
    F: FnOnce(&Path) -> Result<ExtractResult, ExtractError> + Send + 'static,
{
    let path = url.to_file_path().expect("Could not convert to file path");
    let result = match tokio::task::spawn_blocking(move || extract(&path)).await {
        Ok(result) => result?,
        Err(err) => {
            if let Ok(panic) = err.try_into_panic() {
                std::panic::resume_unwind(panic);
            }
            return Err(ExtractError::Cancelled);
        }
    };
    reporter.complete();
    Ok(result)
}

/// Streams `reader` into a spooled pipe while `extract` consumes the other
/// end synchronously on a blocking thread, so the download and the
/// extraction overlap.
///
/// The two halves are deliberately decoupled: the download never waits for
/// the extractor. Data the extractor has not consumed yet is buffered in
/// memory up to [`SPOOL_MEMORY_LIMIT`] and spills to a temporary file beyond
/// that, while data it has consumed is discarded — an extractor that keeps up
/// keeps even large packages entirely off the disk. Reading from the network
/// stream on the extraction thread directly would stall the HTTP/2 stream
/// whenever the extractor falls behind, which servers answer with stream
/// resets under concurrent load. The extractor in turn only ever touches
/// plain memory and a plain [`std::fs::File`], never the async runtime.
///
/// Both halves always run to completion so no detached extraction keeps
/// writing to the destination after this function returns. A download error
/// is forwarded into the pipe (failing the extraction at the point the data
/// ran out) and takes precedence as it is the root cause and remains
/// retryable.
async fn download_and_extract<F>(
    reader: impl tokio::io::AsyncRead + Unpin,
    reporter: StartedDownloadReporter,
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
        reporter.complete();
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
    let reporter = StartedDownloadReporter::start(reporter);

    let destination = destination.to_owned();
    if url.scheme() == "file" {
        return extract_local_file(&url, reporter, move |path| {
            crate::fs::extract_tar_bz2(path, &destination)
        })
        .await;
    }

    let reader = get_reader(url.clone(), client, expected_sha256, reporter.clone()).await?;
    download_and_extract(reader, reporter, move |pipe| {
        crate::read::extract_tar_bz2(pipe, &destination)
    })
    .await
}

/// Downloads the entire package into a spooled temporary file (in memory up
/// to [`SPOOL_MEMORY_LIMIT`], an unlinked file beyond it) and then runs
/// `extract` over it on a blocking thread.
///
/// Used for archives that need random access: their entry sizes only exist in
/// the zip central directory at the *end* of the archive, so extraction
/// cannot overlap the download anyway and the complete package has to be
/// local. The sequential writes happen inline on the async task; like the
/// pipe writer they land in memory or the OS page cache and must stay off the
/// blocking pool.
async fn download_to_spool_and_extract<F>(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    reporter: StartedDownloadReporter,
    extract: F,
) -> Result<ExtractResult, ExtractError>
where
    F: FnOnce(SpooledTempFile) -> Result<ExtractResult, ExtractError> + Send + 'static,
{
    let mut spool = SpooledTempFile::new(SPOOL_MEMORY_LIMIT);
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    loop {
        let len = reader.read(&mut buf).await.map_err(ExtractError::IoError)?;
        if len == 0 {
            break;
        }
        spool
            .write_all(&buf[..len])
            .map_err(ExtractError::IoError)?;
    }
    reporter.complete();

    match tokio::task::spawn_blocking(move || extract(spool)).await {
        Ok(result) => result,
        Err(err) => {
            if let Ok(panic) = err.try_into_panic() {
                std::panic::resume_unwind(panic);
            }
            Err(ExtractError::Cancelled)
        }
    }
}

/// Reads the fixed-size portion of the first zip local file header from the
/// stream so the extraction strategy can be decided before any data flows.
async fn peek_local_header(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Vec<u8>> {
    let mut header = vec![0u8; ZIP_LOCAL_HEADER_LEN];
    let mut filled = 0;
    while filled < header.len() {
        let read = reader.read(&mut header[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    header.truncate(filled);
    Ok(header)
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
    let reporter = StartedDownloadReporter::start(reporter);

    let destination = destination.to_owned();
    if url.scheme() == "file" {
        return extract_local_file(&url, reporter, move |path| {
            crate::fs::extract_conda(path, &destination)
        })
        .await;
    }

    let mut reader = get_reader(
        url.clone(),
        client.clone(),
        expected_sha256,
        reporter.clone(),
    )
    .await?;

    // Decide the extraction strategy from the first zip local file header:
    // archives whose entries carry their sizes in trailing data descriptors
    // (<https://github.com/conda/rattler/issues/794>) can only be extracted
    // through the central directory at the end of the archive and therefore
    // need the complete, seekable package locally. Everything else streams
    // through a sequential pipe that needs no seeking at all.
    let header = peek_local_header(&mut reader)
        .await
        .map_err(ExtractError::IoError)?;
    let uses_data_descriptors = local_header_uses_data_descriptor(&header);
    let reader = std::io::Cursor::new(header).chain(reader);

    if uses_data_descriptors {
        return download_to_spool_and_extract(reader, reporter, move |mut spool| {
            crate::read::extract_conda_via_seeking(&mut spool, &destination)
        })
        .await;
    }

    let extract_destination = destination.clone();
    let result = download_and_extract(reader, reporter.clone(), move |pipe| {
        crate::read::extract_conda_via_streaming(pipe, &extract_destination)
    })
    .await;

    match result {
        // A later entry still used a data descriptor (mixed archive); the
        // sequential pipe cannot rewind, so download the package again into
        // a seekable spool.
        Err(ExtractError::ZipError(ZipError::UnsupportedArchive(message)))
            if message.contains(DATA_DESCRIPTOR_ERROR_MESSAGE) =>
        {
            tracing::warn!(
                "the conda package from '{url}' uses zip data descriptors past the first entry; downloading the package again for buffered extraction"
            );
            let reader = get_reader(url, client, expected_sha256, reporter.clone()).await?;
            download_to_spool_and_extract(reader, reporter, move |mut spool| {
                crate::read::extract_conda_via_seeking(&mut spool, &destination)
            })
            .await
        }
        result => result,
    }
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
