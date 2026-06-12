//! Functions that enable extracting or streaming a Conda package for objects that implement the
//! [`std::io::Read`] trait.

use super::{ExtractError, ExtractResult};
use std::io::{Seek, SeekFrom, copy};
use std::mem::ManuallyDrop;
use std::{
    ffi::OsStr,
    io::Read,
    path::{Component, Path, PathBuf},
};
use tempfile::SpooledTempFile;
use zip::read::{ZipArchive, ZipFile, read_zipfile_from_stream};
use zip::result::ZipError;

/// The minimum safe timestamp (1980-01-01T00:00:00 UTC) for filesystems like exFAT
/// that do not support timestamps before 1980.
const SAFE_MTIME_FLOOR: u64 = 315_532_800;

/// zip files may use data descriptors to signal that the decompressor needs to
/// seek ahead in the buffer to find the compressed data length.
/// Streaming extraction cannot seek ahead, so this condition surfaces as an
/// error with this message during decompression. Read more in
/// <https://github.com/conda/rattler/issues/794>
pub(crate) const DATA_DESCRIPTOR_ERROR_MESSAGE: &str =
    "The file length is not available in the local header";

/// Returns the `.tar.bz2` as a decompressed `tar::Archive`. The `tar::Archive` can be used to
/// extract the files from it, or perform introspection.
pub fn stream_tar_bz2(reader: impl Read) -> tar::Archive<impl Read + Sized> {
    tar::Archive::new(bzip2::read::BzDecoder::new(reader))
}

/// Returns the `.tar.zst` as a decompressed `tar` archive. The `tar::Archive` can be used to
/// extract the files from it, or perform introspection.
pub(crate) fn stream_tar_zst(
    reader: impl Read,
) -> Result<tar::Archive<impl Read + Sized>, ExtractError> {
    Ok(tar::Archive::new(zstd::stream::read::Decoder::new(reader)?))
}

/// Extracts the contents a `.tar.bz2` package archive.
pub fn extract_tar_bz2(
    reader: impl Read,
    destination: &Path,
) -> Result<ExtractResult, ExtractError> {
    std::fs::create_dir_all(destination).map_err(ExtractError::CouldNotCreateDestination)?;

    process_with_hashing(reader, |reader| {
        let mut archive = stream_tar_bz2(reader);
        unpack_tar_archive_sync(&mut archive, destination)?;
        Ok(())
    })
}

/// Extracts the contents of a `.conda` package archive.
pub fn extract_conda_via_streaming(
    reader: impl Read,
    destination: &Path,
) -> Result<ExtractResult, ExtractError> {
    // Construct the destination path if it doesn't exist yet
    std::fs::create_dir_all(destination).map_err(ExtractError::CouldNotCreateDestination)?;

    process_with_hashing(reader, |reader| {
        while let Some(file) = read_zipfile_from_stream(reader)? {
            extract_zipfile(file, destination)?;
        }
        Ok(())
    })
}

/// The fixed-size portion of a zip local file header.
const ZIP_LOCAL_HEADER_LEN: usize = 30;

/// Returns true when a zip local file header signals that the entry's sizes
/// are written in a trailing data descriptor (general purpose flag bit 3)
/// rather than in the header itself.
fn local_header_uses_data_descriptor(header: &[u8]) -> bool {
    header.len() >= 8 && header[..4] == [0x50, 0x4b, 0x03, 0x04] && header[6] & 0x08 != 0
}

/// Extracts the contents of a `.conda` package archive from a seekable reader.
///
/// The archive is extracted by streaming, which computes the hashes and
/// extracts the entries in a single pass. Entries whose sizes live in a
/// trailing zip data descriptor cannot be streamed: the entries are stored
/// (not deflated) so the data is not self-delimiting, and the sizes are only
/// available in the central directory at the very end of the archive (see
/// <https://github.com/conda/rattler/issues/794>). Whether that is the case
/// is decided upfront from the first local file header, so such archives go
/// straight to central-directory driven extraction with
/// [`extract_conda_via_seeking`] without first consuming the stream only to
/// fail.
pub fn extract_conda(
    mut reader: impl Read + Seek,
    destination: &Path,
) -> Result<ExtractResult, ExtractError> {
    // Peek at the first local file header to pick the extraction strategy.
    let mut header = [0u8; ZIP_LOCAL_HEADER_LEN];
    let mut peeked = 0;
    while peeked < header.len() {
        let read = reader.read(&mut header[peeked..])?;
        if read == 0 {
            break;
        }
        peeked += read;
    }
    if local_header_uses_data_descriptor(&header[..peeked]) {
        return extract_conda_via_seeking(reader, destination);
    }

    // Replay the peeked bytes in front of the reader so the streaming path
    // does not need to seek at all.
    let result = {
        let mut replay = std::io::Cursor::new(&header[..peeked]).chain(&mut reader);
        extract_conda_via_streaming(&mut replay, destination)
    };
    match result {
        Err(ExtractError::ZipError(ZipError::UnsupportedArchive(message)))
            if message.contains(DATA_DESCRIPTOR_ERROR_MESSAGE) =>
        {
            // A later entry still used a data descriptor (mixed archive).
            tracing::warn!(
                "failed to stream decompress conda package due to the presence of zip data descriptors, falling back to decompression via the zip central directory"
            );
            extract_conda_via_seeking(reader, destination)
        }
        result => result,
    }
}

/// Extracts the contents of a `.conda` package archive from a seekable reader
/// using the zip central directory instead of streaming the local headers.
///
/// This is the fallback for archives that use zip data descriptors. Unlike
/// [`extract_conda_via_buffering`] it does not copy the data into yet another
/// temporary buffer: the seekable source already retains the package, so the
/// hashes and size are computed in one linear pass and the entries are then
/// read directly through random access.
fn extract_conda_via_seeking(
    mut reader: impl Read + Seek,
    destination: &Path,
) -> Result<ExtractResult, ExtractError> {
    // Delete the destination first as this is used as a fallback from a
    // failed streaming decompression that may have partially extracted files.
    if destination.exists() {
        std::fs::remove_dir_all(destination).map_err(ExtractError::CouldNotCreateDestination)?;
    }
    std::fs::create_dir_all(destination).map_err(ExtractError::CouldNotCreateDestination)?;

    // Compute the hashes and the total size in one linear pass.
    reader.seek(SeekFrom::Start(0))?;
    let sha256_reader =
        rattler_digest::HashingReader::<_, rattler_digest::Sha256>::new(&mut reader);
    let mut md5_reader =
        rattler_digest::HashingReader::<_, rattler_digest::Md5>::new(sha256_reader);
    let mut size_reader = SizeCountingReader::new(&mut md5_reader);
    copy(&mut size_reader, &mut std::io::sink())?;
    let (_, total_size) = size_reader.finalize();
    let (sha256_reader, md5) = md5_reader.finalize();
    let (_, sha256) = sha256_reader.finalize();

    // Extract the entries through the central directory.
    reader.seek(SeekFrom::Start(0))?;
    let mut archive = ZipArchive::new(reader)?;
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        extract_zipfile(file, destination)?;
    }

    Ok(ExtractResult {
        sha256,
        md5,
        total_size,
    })
}

/// Extracts the contents of a .conda package archive by fully reading the stream and then decompressing
pub fn extract_conda_via_buffering(
    reader: impl Read,
    destination: &Path,
) -> Result<ExtractResult, ExtractError> {
    // delete destination first, as this method is usually used as a fallback from a failed streaming decompression
    if destination.exists() {
        std::fs::remove_dir_all(destination).map_err(ExtractError::CouldNotCreateDestination)?;
    }
    std::fs::create_dir_all(destination).map_err(ExtractError::CouldNotCreateDestination)?;

    process_with_hashing(reader, |reader| {
        // Create a SpooledTempFile with a 5MB limit
        let mut temp_file = SpooledTempFile::new(5 * 1024 * 1024);
        copy(reader, &mut temp_file)?;
        temp_file.seek(SeekFrom::Start(0))?;
        let mut archive = ZipArchive::new(temp_file)?;

        for i in 0..archive.len() {
            let file = archive.by_index(i)?;
            extract_zipfile(file, destination)?;
        }
        Ok(())
    })
}

fn extract_zipfile<R: std::io::Read>(
    zip_file: ZipFile<'_, R>,
    destination: &Path,
) -> Result<(), ExtractError> {
    // If an error occurs while we are reading the contents of the zip we don't want to
    // seek to the end of the file. Using [`ManuallyDrop`] we prevent `drop` to be called on
    // the `file` in case the stack unwinds.
    let mut file = ManuallyDrop::new(zip_file);

    if file
        .mangled_name()
        .file_name()
        .map(OsStr::to_string_lossy)
        .is_some_and(|file_name| file_name.ends_with(".tar.zst"))
    {
        let mut archive = stream_tar_zst(&mut *file)?;
        unpack_tar_archive_sync(&mut archive, destination)?;
    } else {
        // Manually read to the end of the stream if that didn't happen.
        std::io::copy(&mut *file, &mut std::io::sink())?;
    }

    // Take the file out of the [`ManuallyDrop`] to properly drop it.
    let _ = ManuallyDrop::into_inner(file);

    Ok(())
}

/// Unpacks a tar archive while handling mtime-setting failures gracefully.
///
/// Disables the tar crate's automatic mtime preservation and instead sets
/// mtimes manually with clamping (to `SAFE_MTIME_FLOOR`) and error handling.
/// This prevents fatal extraction failures on filesystems like exFAT that
/// do not support timestamps before 1980-01-01.
fn unpack_tar_archive_sync<R: Read>(
    archive: &mut tar::Archive<R>,
    destination: &Path,
) -> Result<(), ExtractError> {
    archive.set_preserve_mtime(false);

    for entry in archive.entries().map_err(ExtractError::IoError)? {
        let mut entry = entry.map_err(ExtractError::IoError)?;

        // On Windows, skip symlink entries as they require special privileges
        if cfg!(windows) && entry.header().entry_type().is_symlink() {
            tracing::warn!(
                "Skipping symlink in tar archive: {}",
                entry.path().map_err(ExtractError::IoError)?.display()
            );
            continue;
        }

        let mtime = entry.header().mtime().unwrap_or(0);
        let is_symlink = entry.header().entry_type().is_symlink();
        let entry_path = entry.path().map_err(ExtractError::IoError)?.into_owned();

        let unpacked = entry
            .unpack_in(destination)
            .map_err(ExtractError::IoError)?;

        // Set mtime on the path the entry was actually written to, recomputed
        // with the same sanitization `unpack_in` applies. Joining the raw
        // header path onto `destination` would let an absolute or `..` entry
        // point the mtime write at a file outside the extraction directory.
        if unpacked && let Some(full_path) = unpacked_destination_path(destination, &entry_path) {
            set_mtime_safe(&full_path, mtime, is_symlink);
        }
    }

    Ok(())
}

/// Resolves the on-disk path a tar entry is unpacked to, mirroring the
/// sanitization in [`tar::Entry::unpack_in`]: absolute-path roots and `.`
/// components are stripped and a `..` component makes the entry unsafe.
///
/// Returns `None` when the entry would not map to a distinct path inside
/// `destination`, so callers never set metadata on a path that could resolve
/// outside the extraction directory.
fn unpacked_destination_path(destination: &Path, entry_path: &Path) -> Option<PathBuf> {
    let mut full_path = destination.to_path_buf();
    for component in entry_path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(part) => full_path.push(part),
        }
    }

    if full_path == destination {
        return None;
    }

    Some(full_path)
}

/// Sets the modification time on a file, clamping to a safe minimum and
/// logging a warning on failure instead of propagating the error.
fn set_mtime_safe(path: &Path, mtime: u64, is_symlink: bool) {
    let clamped = std::cmp::max(mtime, SAFE_MTIME_FLOOR);
    let file_time = filetime::FileTime::from_unix_time(clamped as i64, 0);

    let result = if is_symlink {
        filetime::set_symlink_file_times(path, file_time, file_time)
    } else {
        filetime::set_file_mtime(path, file_time)
    };

    if let Err(e) = result {
        tracing::warn!(
            "Failed to set mtime for '{}': {}. \
             The target filesystem may not support this timestamp. \
             This does not affect package integrity.",
            path.display(),
            e
        );
    }
}

// Define a custom reader to track file size
pub(crate) struct SizeCountingReader<R> {
    inner: R,
    size: u64,
}

impl<R> SizeCountingReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self { inner, size: 0 }
    }

    pub(crate) fn finalize(self) -> (R, u64) {
        (self.inner, self.size)
    }
}

impl<R: Read> Read for SizeCountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bytes_read = self.inner.read(buf)?;
        self.size += bytes_read as u64;
        Ok(bytes_read)
    }
}

// AsyncRead implementation for use with tokio
impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for SizeCountingReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let previously_filled = buf.filled().len();

        // Since R: Unpin, we can safely use get_mut
        let this = self.as_mut().get_mut();
        let reader = std::pin::Pin::new(&mut this.inner);

        match reader.poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let bytes_read = buf.filled().len() - previously_filled;
                this.size += bytes_read as u64;
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Helper function to compute hashes and size while processing a tar archive
fn process_with_hashing<E, R, F>(reader: R, processor: F) -> Result<ExtractResult, E>
where
    R: Read,
    E: From<std::io::Error>,
    F: FnOnce(
        &mut SizeCountingReader<
            &mut rattler_digest::HashingReader<
                rattler_digest::HashingReader<R, rattler_digest::Sha256>,
                rattler_digest::Md5,
            >,
        >,
    ) -> Result<(), E>,
{
    // Wrap the reading in additional readers that will compute the hashes of the file while its
    // being read, and count the total size.
    let sha256_reader = rattler_digest::HashingReader::<_, rattler_digest::Sha256>::new(reader);
    let mut md5_reader =
        rattler_digest::HashingReader::<_, rattler_digest::Md5>::new(sha256_reader);
    let mut size_reader = SizeCountingReader::new(&mut md5_reader);

    processor(&mut size_reader)?;

    // Read the file to the end to make sure the hash is properly computed
    std::io::copy(&mut size_reader, &mut std::io::sink())?;

    // Get the size and hashes
    let (_, total_size) = size_reader.finalize();
    let (sha256_reader, md5) = md5_reader.finalize();
    let (_, sha256) = sha256_reader.finalize();

    Ok(ExtractResult {
        sha256,
        md5,
        total_size,
    })
}
