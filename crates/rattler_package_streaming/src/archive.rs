//! Read individual files from conda packages, local or remote, with as few
//! HTTP requests as possible.
//!
//! A [`PackageArchive`] is opened once and queried many times. Remote
//! `.conda` archives on range-capable servers are opened with a single
//! request for the archive tail (ZIP central directory and usually the whole
//! info section); reads then cost at most one streaming ranged request per
//! touched section, aborted once the last requested file has been read.
//! `.tar.bz2` archives and servers without range support transparently fall
//! back to downloading the archive once into a temporary spool file.
//!
//! # Example
//!
//! ```rust,no_run
//! # #[tokio::main]
//! # async fn main() {
//! use rattler_conda_types::package::PathsJson;
//! use rattler_package_streaming::archive::PackageArchive;
//! use reqwest::Client;
//! use reqwest_middleware::ClientWithMiddleware;
//! use url::Url;
//!
//! let client = ClientWithMiddleware::from(Client::new());
//! let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/python-3.12.7-hc5c86c4_0_cpython.conda").unwrap();
//!
//! // One HTTP range request.
//! let archive = PackageArchive::from_url(client, url).await.unwrap();
//!
//! // Usually free: the info section often sits inside the cached tail.
//! let paths: PathsJson = archive.read_package_file().await.unwrap();
//!
//! // One streaming pass over the payload, aborted after the last hit.
//! let files = archive
//!     .read_files(paths.paths.iter().map(|entry| entry.relative_path.clone()))
//!     .await
//!     .unwrap();
//! # drop(files);
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_compression::tokio::bufread::{BzDecoder, ZstdDecoder};
use async_http_range_reader::{
    AsyncHttpRangeReader, AsyncHttpRangeReaderError, CheckSupportMethod,
};
use async_zip::Compression;
use async_zip::base::read::seek::ZipFileReader;
use futures_util::TryStreamExt;
use http::HeaderMap;
use http::header::{ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use rattler_conda_types::package::{CondaArchiveType, PackageFile};
use reqwest_middleware::ClientWithMiddleware;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::io::StreamReader;
use tracing::debug;
use url::Url;

use crate::ExtractError;

/// Bytes fetched from the end of a remote archive on open: enough for the
/// ZIP central directory, with the surplus acting as a cache that often
/// contains the entire info section.
const TAIL_SIZE: u64 = 64 * 1024;

/// Buffer size used for the decompression pipelines.
const STREAM_BUF_SIZE: usize = 128 * 1024;

/// Signature of a ZIP local file header (`PK\x03\x04`).
const LOCAL_HEADER_MAGIC: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];

/// Cap for upfront buffer allocations based on (untrusted) tar header sizes.
const MAX_PREALLOC: u64 = 4 * 1024 * 1024;

/// The two sections of a conda package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// Package metadata: everything under `info/`. Stored in the
    /// `info-*.tar.zst` member of a `.conda` archive.
    Info,
    /// The package payload. Stored in the `pkg-*.tar.zst` member of a
    /// `.conda` archive.
    Content,
}

impl Section {
    /// Returns the section a path inside the package belongs to.
    pub fn containing(path: &Path) -> Section {
        match normalize(path).components().next() {
            Some(std::path::Component::Normal(first)) if first == "info" => Section::Info,
            _ => Section::Content,
        }
    }

    /// The file name prefix of the ZIP member holding this section.
    fn zip_prefix(self) -> &'static str {
        match self {
            Section::Info => "info-",
            Section::Content => "pkg-",
        }
    }
}

/// How a [`PackageArchive`] accesses the underlying archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveAccess {
    /// Remote archive read sparsely with HTTP range requests.
    Sparse,
    /// Local file on disk.
    Local,
    /// Remote archive that was downloaded once into a temporary spool file
    /// (server without range support, or a `.tar.bz2` archive).
    Spooled,
}

/// Byte span of a stored ZIP member inside a `.conda` archive.
///
/// `end` is the offset of the next member's local header (or the central
/// directory for the last member), which is a robust upper bound for the
/// member's data regardless of local-header extra field quirks.
#[derive(Debug, Clone)]
struct MemberSpan {
    name: String,
    /// Offset of the member's local file header.
    header_offset: u64,
    /// Size of the stored (uncompressed) member data.
    size: u64,
    /// Exclusive upper bound of the member's bytes in the archive.
    end: u64,
}

enum Backend {
    Sparse {
        client: ClientWithMiddleware,
        url: Url,
        /// Strong `ETag` (or `Last-Modified`) captured at open time; sent as
        /// `If-Range` on section requests so servers that honor it reject
        /// reads from a concurrently republished archive. Best-effort:
        /// servers may ignore `If-Range`.
        validator: Option<http::HeaderValue>,
        tail_offset: u64,
        tail: Vec<u8>,
        members: Vec<MemberSpan>,
    },
    LocalConda {
        path: PathBuf,
        members: Vec<MemberSpan>,
        /// Present when the archive was spooled from a remote; keeps the
        /// temporary file alive and distinguishes `Spooled` from `Local`.
        temp: Option<tempfile::TempPath>,
    },
    LocalTarBz2 {
        path: PathBuf,
        temp: Option<tempfile::TempPath>,
    },
}

/// A conda package archive that can be opened once and read many times.
///
/// Cloning is cheap; clones share the parsed archive index and (for spooled
/// archives) the temporary file.
#[derive(Clone)]
pub struct PackageArchive {
    backend: Arc<Backend>,
}

/// A boxed reader used for the section decompression pipelines.
type DynReader = Box<dyn AsyncRead + Send + Unpin>;

/// A tar entry yielded by [`SectionStream::next_entry`]. Exposes `path()`,
/// `header()` and implements [`AsyncRead`] for the entry body.
pub type SectionEntry = tokio_tar::Entry<tokio_tar::Archive<DynReader>>;

impl PackageArchive {
    /// Opens a remote package archive with a single range request, falling
    /// back to a one-time spooled download for `.tar.bz2` archives and
    /// servers without range support.
    pub async fn from_url(client: ClientWithMiddleware, url: Url) -> Result<Self, ExtractError> {
        let archive_type = CondaArchiveType::try_from(Path::new(url.path()))
            .ok_or(ExtractError::UnsupportedArchiveType)?;

        if archive_type == CondaArchiveType::Conda {
            match Self::open_sparse(client.clone(), url.clone()).await {
                Ok(archive) => return Ok(archive),
                Err(err) if sparse_unsupported(&err) => {
                    debug!(
                        "sparse access unavailable ({err}), falling back to spooled full download"
                    );
                }
                Err(err) => return Err(err),
            }
        }

        Self::open_spooled(client, url, archive_type).await
    }

    /// Opens a package archive from a local file.
    pub async fn from_path(path: impl AsRef<Path>) -> Result<Self, ExtractError> {
        let path = path.as_ref();
        let archive_type =
            CondaArchiveType::try_from(path).ok_or(ExtractError::UnsupportedArchiveType)?;
        Self::open_local(path.to_owned(), archive_type, None).await
    }

    /// Returns how this handle accesses the archive.
    pub fn access(&self) -> ArchiveAccess {
        match &*self.backend {
            Backend::Sparse { .. } => ArchiveAccess::Sparse,
            Backend::LocalConda { temp: None, .. } | Backend::LocalTarBz2 { temp: None, .. } => {
                ArchiveAccess::Local
            }
            Backend::LocalConda { .. } | Backend::LocalTarBz2 { .. } => ArchiveAccess::Spooled,
        }
    }

    /// Reads a single file from the package, or `None` if the path does not
    /// exist.
    ///
    /// Contents are not cached: every call streams the containing section
    /// again up to the requested file. Prefer [`PackageArchive::read_files`]
    /// with one batch over repeated calls.
    pub async fn read_file(&self, path: impl AsRef<Path>) -> Result<Option<Vec<u8>>, ExtractError> {
        let path = normalize(path.as_ref());
        let mut result = self.read_files([path.clone()]).await?;
        Ok(result.remove(&path).flatten())
    }

    /// Reads multiple files in one pass per touched section (sections are
    /// fetched concurrently), aborting each stream after its last requested
    /// file. Maps every requested path to its contents, or `None` when
    /// absent.
    ///
    /// Calls are independent and may run concurrently, but contents are not
    /// cached: a repeated call streams its sections again, so batch all
    /// needed paths into a single call where possible.
    pub async fn read_files(
        &self,
        paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Result<HashMap<PathBuf, Option<Vec<u8>>>, ExtractError> {
        let paths: Vec<PathBuf> = paths.into_iter().map(|p| normalize(&p.into())).collect();
        if paths.is_empty() {
            return Ok(HashMap::new());
        }

        // A .tar.bz2 archive is one flat tar: serve everything in a single
        // unfiltered pass. Grouping per section here would decompress the
        // whole bz2 stream once per section.
        if matches!(&*self.backend, Backend::LocalTarBz2 { .. }) {
            let mut stream = self.tar_bz2_stream(None).await?;
            return scan_stream(&mut stream, paths).await;
        }

        let mut groups: HashMap<Section, Vec<PathBuf>> = HashMap::new();
        for path in paths {
            groups
                .entry(Section::containing(&path))
                .or_default()
                .push(path);
        }

        let passes = groups.into_iter().map(|(section, group)| async move {
            let mut stream = self.stream(section).await?;
            scan_stream(&mut stream, group).await
        });
        let results = futures::future::try_join_all(passes).await?;

        Ok(results.into_iter().flatten().collect())
    }

    /// Reads and parses a typed [`PackageFile`] (e.g. `IndexJson`,
    /// `PathsJson`) from the package.
    pub async fn read_package_file<P: PackageFile>(&self) -> Result<P, ExtractError> {
        let bytes = self
            .read_file(P::package_path())
            .await?
            .ok_or(ExtractError::MissingComponent)?;
        P::from_slice(&bytes)
            .map_err(|e| ExtractError::ArchiveMemberParseError(P::package_path().to_owned(), e))
    }

    /// Lists the paths of all files in one section.
    ///
    /// For [`Section::Info`] this is usually served from the cached archive
    /// tail without extra requests. For [`Section::Content`] it streams the
    /// entire section; prefer reading `info/paths.json` when only paths are
    /// needed.
    pub async fn list_files(&self, section: Section) -> Result<Vec<PathBuf>, ExtractError> {
        let mut stream = self.stream(section).await?;
        let mut paths = Vec::new();
        while let Some(entry) = stream.next_entry().await? {
            if entry.header().entry_type().is_file() {
                paths.push(entry.path().map_err(ExtractError::IoError)?.into_owned());
            }
        }
        Ok(paths)
    }

    /// Streams the tar entries of one section. Unread entries are skipped
    /// cheaply; dropping the stream aborts any underlying HTTP transfer.
    ///
    /// Every call opens a new independent forward-only stream (for remote
    /// archives: a new request).
    pub async fn stream(&self, section: Section) -> Result<SectionStream, ExtractError> {
        match &*self.backend {
            Backend::Sparse { .. } | Backend::LocalConda { .. } => {
                let raw = self.conda_section_reader(section).await?;
                let decoder =
                    ZstdDecoder::new(tokio::io::BufReader::with_capacity(STREAM_BUF_SIZE, raw));
                Ok(SectionStream::new(Box::new(decoder), None))
            }
            // `read_files` bypasses the filter with `tar_bz2_stream(None)`
            // to serve both sections from a single pass.
            Backend::LocalTarBz2 { .. } => self.tar_bz2_stream(Some(section)).await,
        }
    }

    // ---------------------------------------------------------------------
    // opening
    // ---------------------------------------------------------------------

    /// Opens a remote `.conda` archive sparsely, without full-download fallback.
    pub(crate) async fn open_sparse(
        client: ClientWithMiddleware,
        url: Url,
    ) -> Result<Self, ExtractError> {
        // One suffix range request: fetches the last TAIL_SIZE bytes and
        // reveals the total archive size.
        let (reader, headers) = AsyncHttpRangeReader::new(
            client.clone(),
            url.clone(),
            CheckSupportMethod::NegativeRangeRequest(TAIL_SIZE),
            HeaderMap::default(),
        )
        .await?;
        // A weak ETag must not be sent in `If-Range` (RFC 9110 §13.1.5);
        // fall back to `Last-Modified` in that case.
        let validator = headers
            .get(ETAG)
            .filter(|v| !v.as_bytes().starts_with(b"W/"))
            .or_else(|| headers.get(LAST_MODIFIED))
            .cloned();
        let size = reader.len();
        debug!("opened remote archive ({size} bytes) with a {TAIL_SIZE} byte tail request");

        // Parse the central directory. The needed bytes are already cached
        // from the tail request; if the central directory is unusually large
        // the range reader transparently fetches the difference.
        let buf_reader = futures::io::BufReader::new(reader.compat());
        let zip = ZipFileReader::new(buf_reader).await?;
        let members = collect_members(zip.file(), size)?;

        // Recover the range reader and keep a copy of the tail bytes so
        // members that live inside the tail (usually the info section) can be
        // served without further requests.
        let mut reader = zip.into_inner().into_inner().into_inner();
        let tail_offset = size.saturating_sub(TAIL_SIZE);
        let mut tail = vec![0u8; (size - tail_offset) as usize];
        reader
            .seek(SeekFrom::Start(tail_offset))
            .await
            .map_err(ExtractError::IoError)?;
        reader
            .read_exact(&mut tail)
            .await
            .map_err(ExtractError::IoError)?;

        Ok(Self {
            backend: Arc::new(Backend::Sparse {
                client,
                url,
                validator,
                tail_offset,
                tail,
                members,
            }),
        })
    }

    async fn open_spooled(
        client: ClientWithMiddleware,
        url: Url,
        archive_type: CondaArchiveType,
    ) -> Result<Self, ExtractError> {
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(ExtractError::from)?
            .error_for_status()
            .map_err(|e| ExtractError::ReqwestError(e.into()))?;

        // Spool to disk rather than memory: packages can be arbitrarily
        // large (multi-GB), so an in-memory copy is not an option.
        let temp = tempfile::NamedTempFile::new().map_err(ExtractError::IoError)?;
        let (file, temp_path) = temp.into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let mut body = StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
        tokio::io::copy(&mut body, &mut file)
            .await
            .map_err(ExtractError::IoError)?;
        file.flush().await.map_err(ExtractError::IoError)?;

        Self::open_local(temp_path.to_path_buf(), archive_type, Some(temp_path)).await
    }

    async fn open_local(
        path: PathBuf,
        archive_type: CondaArchiveType,
        temp: Option<tempfile::TempPath>,
    ) -> Result<Self, ExtractError> {
        let backend = match archive_type {
            CondaArchiveType::Conda => {
                let file = tokio::fs::File::open(&path)
                    .await
                    .map_err(ExtractError::IoError)?;
                let size = file.metadata().await.map_err(ExtractError::IoError)?.len();
                let buf_reader =
                    futures::io::BufReader::new(tokio::io::BufReader::new(file).compat());
                let zip = ZipFileReader::new(buf_reader).await?;
                let members = collect_members(zip.file(), size)?;
                Backend::LocalConda {
                    path,
                    members,
                    temp,
                }
            }
            CondaArchiveType::TarBz2 => Backend::LocalTarBz2 { path, temp },
        };
        Ok(Self {
            backend: Arc::new(backend),
        })
    }

    // ---------------------------------------------------------------------
    // section readers
    // ---------------------------------------------------------------------

    /// Returns a reader over the stored bytes of the ZIP member holding
    /// `section`.
    async fn conda_section_reader(&self, section: Section) -> Result<DynReader, ExtractError> {
        match &*self.backend {
            Backend::Sparse {
                client,
                url,
                validator,
                tail_offset,
                tail,
                members,
                ..
            } => {
                let span = find_section_member(members, section)?;

                // Serve from the cached tail when the whole member is inside it.
                if span.header_offset >= *tail_offset {
                    let rel = (span.header_offset - tail_offset) as usize;
                    if let Some(data) = member_data_from_buffer(&tail[rel..], span.size) {
                        debug!("serving member {} from the cached tail", span.name);
                        return Ok(Box::new(std::io::Cursor::new(data.to_vec())));
                    }
                }

                // One bounded streaming ranged GET for the member. Dropping
                // the returned reader aborts the transfer.
                debug!(
                    "requesting range {}-{} for member {}",
                    span.header_offset,
                    span.end - 1,
                    span.name
                );
                let mut request = client.get(url.clone()).header(
                    RANGE,
                    format!("bytes={}-{}", span.header_offset, span.end - 1),
                );
                if let Some(validator) = validator {
                    request = request.header(IF_RANGE, validator);
                }
                let response = request
                    .send()
                    .await
                    .map_err(ExtractError::from)?
                    .error_for_status()
                    .map_err(|e| ExtractError::ReqwestError(e.into()))?;
                if response.status() != ::reqwest::StatusCode::PARTIAL_CONTENT {
                    // An honored `If-Range` mismatch (the archive changed since
                    // it was opened) or a server that stopped honoring ranges.
                    return Err(ExtractError::IoError(std::io::Error::other(
                        "remote archive changed while reading it (range request returned a full response)",
                    )));
                }
                let mut reader =
                    StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
                skip_local_header(&mut reader).await?;
                Ok(Box::new(reader.take(span.size)))
            }
            Backend::LocalConda { path, members, .. } => {
                let span = find_section_member(members, section)?;
                let mut file = tokio::fs::File::open(path)
                    .await
                    .map_err(ExtractError::IoError)?;
                file.seek(SeekFrom::Start(span.header_offset))
                    .await
                    .map_err(ExtractError::IoError)?;
                let mut reader = tokio::io::BufReader::new(file);
                skip_local_header(&mut reader).await?;
                Ok(Box::new(reader.take(span.size)))
            }
            Backend::LocalTarBz2 { .. } => unreachable!("tar.bz2 has no conda sections"),
        }
    }

    /// Opens a (optionally section-filtered) stream over a `.tar.bz2` archive.
    async fn tar_bz2_stream(
        &self,
        section: Option<Section>,
    ) -> Result<SectionStream, ExtractError> {
        let Backend::LocalTarBz2 { path, .. } = &*self.backend else {
            unreachable!("tar_bz2_stream called on a .conda backend")
        };
        let file = tokio::fs::File::open(path)
            .await
            .map_err(ExtractError::IoError)?;
        let decoder = BzDecoder::new(tokio::io::BufReader::with_capacity(STREAM_BUF_SIZE, file));
        Ok(SectionStream::new(Box::new(decoder), section))
    }
}

/// A streaming view over the tar entries of one package section.
pub struct SectionStream {
    entries: tokio_tar::Entries<DynReader>,
    /// For `.tar.bz2` archives (one flat tar), entries are filtered to the
    /// requested section. `None` yields every entry.
    filter: Option<Section>,
}

impl SectionStream {
    fn new(reader: DynReader, filter: Option<Section>) -> Self {
        let mut archive = tokio_tar::Archive::new(reader);
        let entries = archive
            .entries()
            .expect("entries() cannot fail on a fresh archive");
        Self { entries, filter }
    }

    /// Advances to the next tar entry of the section, or `None` at the end of
    /// the section.
    pub async fn next_entry(&mut self) -> Result<Option<SectionEntry>, ExtractError> {
        use futures_util::StreamExt;
        while let Some(entry) = self.entries.next().await {
            let entry = entry.map_err(ExtractError::IoError)?;
            if let Some(section) = self.filter {
                let path = entry.path().map_err(ExtractError::IoError)?;
                if Section::containing(&path) != section {
                    continue;
                }
            }
            return Ok(Some(entry));
        }
        Ok(None)
    }
}

/// Reads the requested paths out of a section stream, aborting as soon as the
/// last one has been found.
async fn scan_stream(
    stream: &mut SectionStream,
    paths: Vec<PathBuf>,
) -> Result<HashMap<PathBuf, Option<Vec<u8>>>, ExtractError> {
    let mut remaining: HashSet<PathBuf> = paths.into_iter().collect();
    let mut out = HashMap::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let Some(mut entry) = stream.next_entry().await? else {
            break;
        };
        let path: PathBuf = entry.path().map_err(ExtractError::IoError)?.into_owned();
        if remaining.remove(&path) {
            // The header size is untrusted; cap the upfront allocation.
            let size = entry.header().size().map_err(ExtractError::IoError)?;
            let mut buf = Vec::with_capacity(size.min(MAX_PREALLOC) as usize);
            entry
                .read_to_end(&mut buf)
                .await
                .map_err(ExtractError::IoError)?;
            out.insert(path, Some(buf));
        }
    }
    for path in remaining {
        out.insert(path, None);
    }
    Ok(out)
}

/// Collects the member spans of a `.conda` ZIP archive from its parsed
/// central directory. The exclusive end bound of each member is the offset of
/// the next member (or the end of the archive), which over-approximates by at
/// most the size of the central directory for the last member.
fn collect_members(
    zip: &async_zip::ZipFile,
    archive_size: u64,
) -> Result<Vec<MemberSpan>, ExtractError> {
    let entries = zip.entries();
    let mut members = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .filename()
            .as_str()
            .map_err(|e| {
                ExtractError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?
            .to_owned();
        if name.ends_with(".tar.zst") && entry.compression() != Compression::Stored {
            return Err(ExtractError::UnsupportedCompressionMethod);
        }
        members.push(MemberSpan {
            name,
            header_offset: entry.header_offset(),
            size: entry.compressed_size(),
            end: archive_size,
        });
    }
    // Bound each member by the next member's local header offset.
    let mut offsets: Vec<u64> = members.iter().map(|m| m.header_offset).collect();
    offsets.sort_unstable();
    for member in &mut members {
        if let Some(next) = offsets.iter().find(|&&o| o > member.header_offset) {
            member.end = *next;
        }
    }
    Ok(members)
}

fn find_section_member(
    members: &[MemberSpan],
    section: Section,
) -> Result<&MemberSpan, ExtractError> {
    let prefix = section.zip_prefix();
    members
        .iter()
        .find(|m| m.name.starts_with(prefix) && m.name.ends_with(".tar.zst"))
        .ok_or(ExtractError::MissingComponent)
}

/// Strips `.` components so `./info/index.json` matches `info/index.json`.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// Parses a ZIP local file header at the start of `buf` and returns the
/// member data if `buf` contains all of it.
fn member_data_from_buffer(buf: &[u8], size: u64) -> Option<&[u8]> {
    if buf.len() < 30 || buf[0..4] != LOCAL_HEADER_MAGIC {
        return None;
    }
    let name_len = u16::from_le_bytes([buf[26], buf[27]]) as usize;
    let extra_len = u16::from_le_bytes([buf[28], buf[29]]) as usize;
    let data_start = 30 + name_len + extra_len;
    let data_end = data_start.checked_add(size as usize)?;
    (data_end <= buf.len()).then(|| &buf[data_start..data_end])
}

/// Reads and skips a ZIP local file header from a stream, leaving the reader
/// positioned at the start of the member data.
async fn skip_local_header<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(), ExtractError> {
    let mut header = [0u8; 30];
    reader
        .read_exact(&mut header)
        .await
        .map_err(ExtractError::IoError)?;
    if header[0..4] != LOCAL_HEADER_MAGIC {
        return Err(ExtractError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected a ZIP local file header",
        )));
    }
    let name_len = u64::from(u16::from_le_bytes([header[26], header[27]]));
    let extra_len = u64::from(u16::from_le_bytes([header[28], header[29]]));
    let mut skip = reader.take(name_len + extra_len);
    tokio::io::copy(&mut skip, &mut tokio::io::sink())
        .await
        .map_err(ExtractError::IoError)?;
    Ok(())
}

/// Returns true for errors that mean "sparse access is unavailable" and the
/// caller should fall back to a full download.
pub(crate) fn sparse_unsupported(err: &ExtractError) -> bool {
    match err {
        // Servers that ignore the `Range` header answer with a plain `200 OK`
        // that carries no `Content-Range` header.
        ExtractError::AsyncHttpRangeReaderError(
            AsyncHttpRangeReaderError::HttpRangeRequestUnsupported
            | AsyncHttpRangeReaderError::ContentRangeMissing,
        ) => true,
        // JFrog Artifactory returns 416 when querying more than the object length.
        ExtractError::AsyncHttpRangeReaderError(AsyncHttpRangeReaderError::HttpError(err)) => {
            err.status() == Some(::reqwest::StatusCode::RANGE_NOT_SATISFIABLE)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rattler_conda_types::package::{AboutJson, IndexJson};

    use super::*;
    use crate::reqwest::test_server;

    fn conda_test_file() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/clobber/clobber-fd-1-0.1.0-h4616a5c_0.conda")
    }

    fn tar_bz2_test_file() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/clobber/clobber-1-0.1.0-h4616a5c_0.tar.bz2")
    }

    /// A middleware that counts the HTTP requests going through a client.
    struct RequestCounter(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl reqwest_middleware::Middleware for RequestCounter {
        async fn handle(
            &self,
            req: ::reqwest::Request,
            extensions: &mut http::Extensions,
            next: reqwest_middleware::Next<'_>,
        ) -> reqwest_middleware::Result<::reqwest::Response> {
            self.0.fetch_add(1, Ordering::Relaxed);
            next.run(req, extensions).await
        }
    }

    fn counting_client() -> (ClientWithMiddleware, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let client = reqwest_middleware::ClientBuilder::new(::reqwest::Client::new())
            .with(RequestCounter(counter.clone()))
            .build();
        (client, counter)
    }

    #[tokio::test]
    async fn test_sparse_conda_round_trip() {
        let url = test_server::serve_file(conda_test_file()).await;
        let (client, requests) = counting_client();

        let archive = PackageArchive::from_url(client, url).await.unwrap();
        assert_eq!(archive.access(), ArchiveAccess::Sparse);
        assert_eq!(requests.load(Ordering::Relaxed), 1, "open = 1 request");

        // Typed metadata reads: the test package is tiny, so everything is
        // served from the cached tail without further requests.
        let index: IndexJson = archive.read_package_file().await.unwrap();
        assert_eq!(index.name.as_normalized(), "clobber-fd-1");
        let _about: AboutJson = archive.read_package_file().await.unwrap();
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "metadata reads served from the tail cache"
        );

        // Payload + metadata in one batched call.
        let files = archive
            .read_files(["clobber", "info/index.json", "does/not/exist"])
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(files[Path::new("clobber")].clone().unwrap()).unwrap(),
            "clobber-fd-1\n"
        );
        assert!(files[Path::new("info/index.json")].is_some());
        assert!(files[Path::new("does/not/exist")].is_none());
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "tiny package: payload also served from the tail cache"
        );
    }

    #[tokio::test]
    async fn test_stream_section() {
        let url = test_server::serve_file(conda_test_file()).await;
        let (client, _) = counting_client();

        let archive = PackageArchive::from_url(client, url).await.unwrap();
        let mut names = Vec::new();
        let mut stream = archive.stream(Section::Info).await.unwrap();
        while let Some(entry) = stream.next_entry().await.unwrap() {
            names.push(entry.path().unwrap().display().to_string());
        }
        assert!(names.iter().any(|n| n == "info/index.json"), "{names:?}");
    }

    #[tokio::test]
    async fn test_tar_bz2_spooled() {
        let url = test_server::serve_file(tar_bz2_test_file()).await;
        let (client, requests) = counting_client();

        let archive = PackageArchive::from_url(client, url).await.unwrap();
        assert_eq!(archive.access(), ArchiveAccess::Spooled);
        assert_eq!(requests.load(Ordering::Relaxed), 1, "one full download");

        let files = archive
            .read_files(["info/index.json", "clobber.txt"])
            .await
            .unwrap();
        assert!(files[Path::new("info/index.json")].is_some());
        assert!(files[Path::new("clobber.txt")].is_some());

        let index: IndexJson = archive.read_package_file().await.unwrap();
        assert_eq!(index.name.as_normalized(), "clobber-1");
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "spooled archive is downloaded exactly once"
        );

        // Section streaming filters the flat tar by prefix.
        let mut stream = archive.stream(Section::Content).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = stream.next_entry().await.unwrap() {
            names.push(entry.path().unwrap().display().to_string());
        }
        assert!(names.iter().all(|n| !n.starts_with("info/")), "{names:?}");
        assert!(names.iter().any(|n| n == "clobber.txt"), "{names:?}");
    }

    #[tokio::test]
    async fn test_conda_no_range_support_fallback() {
        let url = test_server::serve_file_no_ranges(conda_test_file()).await;
        let (client, requests) = counting_client();

        let archive = PackageArchive::from_url(client, url).await.unwrap();
        assert_eq!(archive.access(), ArchiveAccess::Spooled);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            2,
            "one failed range probe + one full download"
        );

        let index: IndexJson = archive.read_package_file().await.unwrap();
        assert_eq!(index.name.as_normalized(), "clobber-fd-1");
        let content = archive.read_file("clobber").await.unwrap().unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), "clobber-fd-1\n");
        assert_eq!(
            requests.load(Ordering::Relaxed),
            2,
            "all reads served from the spool file"
        );
    }

    /// A package larger than the 64 KiB tail: payload reads must go through
    /// the ranged member-GET path (local header skip, `If-Range`, end bound).
    #[tokio::test]
    async fn test_sparse_large_package() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sparse/sparse-test-1.0.0-0.conda");
        let url = test_server::serve_file(fixture).await;
        let (client, requests) = counting_client();

        let archive = PackageArchive::from_url(client, url).await.unwrap();
        assert_eq!(requests.load(Ordering::Relaxed), 1, "open = 1 request");

        // The info member sits inside the tail: no extra request. Leading
        // `./` components are normalized away.
        let index = archive
            .read_file("./info/index.json")
            .await
            .unwrap()
            .expect("index.json should exist");
        assert!(!index.is_empty());
        assert_eq!(requests.load(Ordering::Relaxed), 1);

        // The payload member lies outside the tail: exactly one ranged GET,
        // shared by both files.
        let files = archive
            .read_files(["bin/first-file.txt", "share/last-file.txt"])
            .await
            .unwrap();
        assert_eq!(
            files[Path::new("bin/first-file.txt")].as_deref(),
            Some(b"first payload file\n".as_slice())
        );
        assert_eq!(
            files[Path::new("share/last-file.txt")].as_deref(),
            Some(b"last payload file\n".as_slice())
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            2,
            "payload batch = 1 ranged request"
        );

        let names = archive.list_files(Section::Content).await.unwrap();
        assert_eq!(names.len(), 3, "{names:?}");
    }

    #[tokio::test]
    async fn test_list_files() {
        let archive = PackageArchive::from_path(conda_test_file()).await.unwrap();
        let info = archive.list_files(Section::Info).await.unwrap();
        assert!(
            info.iter().any(|p| p == Path::new("info/index.json")),
            "{info:?}"
        );
        let content = archive.list_files(Section::Content).await.unwrap();
        assert_eq!(content, vec![PathBuf::from("clobber")]);
    }

    #[tokio::test]
    async fn test_local_conda() {
        let archive = PackageArchive::from_path(conda_test_file()).await.unwrap();
        assert_eq!(archive.access(), ArchiveAccess::Local);
        let index: IndexJson = archive.read_package_file().await.unwrap();
        assert_eq!(index.name.as_normalized(), "clobber-fd-1");
        let content = archive.read_file("clobber").await.unwrap().unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), "clobber-fd-1\n");
    }
}
