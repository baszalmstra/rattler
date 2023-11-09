use std::io;
use std::path::PathBuf;
use thiserror::Error;
use zip::result::ZipError;

/// An error that can occur while extracting an archive.
#[derive(Debug, Error)]
pub enum ExtractError {
    /// Failed to extract a tarball while doing a certain IO operation.
    #[error("failed to extract archive while {2}{}", if let Some(path) = .1 {
    format!(" (file: {})", path.to_string_lossy())
    } else {
    "".to_string()
    })]
    IoError(#[source] io::Error, Option<PathBuf>, String),

    /// Failed to extract an archive to the cache.
    #[error("failed to extract archive to cache. {0}{}", if let Some(path) = .1 {
    format!(" (file: {})", path.to_string_lossy())
    } else {
    "".to_string()
    })]
    CacheError(#[source] cacache::Error, Option<PathBuf>),

    /// An error occured while decoding a zip archive
    #[error("failed to extract archive to cache. {0}{}", if let Some(path) = .1 {
    format!(" (file: {})", path.to_string_lossy())
    } else {
    "".to_string()
    })]
    ZipError(#[source] ZipError, Option<PathBuf>),

    /// The integrity of a file mismatches
    #[error("the integrity of the archive is compromised, expected '{0}' got '{1}'")]
    IntegrityMismatch(String, String),

    /// An error occurred while serializing archive metadata to cache.
    #[error("failed to serialize archive metadata to cache: {0}")]
    SerializeCacheError(String),

    /// An error happened while deserializing cache metadata.
    #[error("failed to deserialize cache metadata: {0}")]
    DeserializeCacheError(String),

    /// A async task has been cancelled.
    #[error("the operation was cancelled")]
    Cancelled,
}

impl ExtractError {
    /// Constructs a new error from a zip error and the file that was involved. If the error
    /// represents a problem reading the underlying datastructure this will be converted to an IO
    /// error.
    pub fn zip_error(err: ZipError, path: Option<PathBuf>) -> Self {
        match err {
            ZipError::Io(err) => Self::IoError(err, path, "reading zip archive".into()),
            _ => Self::ZipError(err, path),
        }
    }
}
