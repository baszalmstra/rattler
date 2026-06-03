use std::{
    fmt::{Debug, Formatter},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use fs4::fs_std::FileExt;
use rattler_conda_types::package::{IndexJson, PathsJson};
use rattler_digest::{Md5Hash, Sha256Hash};

use crate::package_cache::PackageCacheLayerError;

/// A validated cache entry with its associated metadata.
///
/// This struct represents a cache entry that has been validated and is ready for use.
/// It holds the cache entry's path, revision number, and optional SHA256 hash.
///
/// Note: Concurrent access is coordinated via the global cache lock mechanism
/// (see [`CacheGlobalLock`]). Individual cache entries do not hold locks.
pub struct CacheMetadata {
    pub(super) revision: u64,
    pub(super) sha256: Option<Sha256Hash>,
    pub(super) path: PathBuf,
    pub(super) index_json: Option<IndexJson>,
    pub(super) paths_json: Option<PathsJson>,
}

impl Debug for CacheMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheMetadata")
            .field("path", &self.path)
            .field("revision", &self.revision)
            .field("sha256", &self.sha256)
            .finish()
    }
}

impl CacheMetadata {
    /// Returns the path to the cache entry on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the revision of the cache entry. This revision indicates the
    /// number of times the cache entry has been updated.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the cached `index.json` data if it was read during validation.
    pub fn index_json(&self) -> Option<&IndexJson> {
        self.index_json.as_ref()
    }

    /// Returns the cached `paths.json` data if it was read during validation.
    pub fn paths_json(&self) -> Option<&PathsJson> {
        self.paths_json.as_ref()
    }
}

/// A global lock for the entire package cache.
///
/// This can be used to reduce lock overhead when performing many package
/// operations by acquiring a single global lock instead of individual per-package locks.
pub struct CacheGlobalLock {
    file: std::fs::File,
}

impl Debug for CacheGlobalLock {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheGlobalLock").finish()
    }
}

impl Drop for CacheGlobalLock {
    fn drop(&mut self) {
        // Ensure that the lock is released when the lock is dropped.
        let _ = fs4::fs_std::FileExt::unlock(&self.file);
    }
}

impl CacheGlobalLock {
    /// Acquires a global write lock on the package cache.
    ///
    /// This lock should be used to coordinate access across multiple package
    /// operations to reduce the overhead of acquiring individual locks.
    pub async fn acquire(path: &Path) -> Result<Self, PackageCacheLayerError> {
        let lock_file_path = path.to_path_buf();
        let acquire_lock_fut = simple_spawn_blocking::tokio::run_blocking_task(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .read(true)
                .open(&lock_file_path)
                .map_err(|e| {
                    PackageCacheLayerError::LockError(
                        format!(
                            "failed to open global cache lock for writing: '{}'",
                            lock_file_path.display()
                        ),
                        e,
                    )
                })?;

            file.lock_exclusive().map_err(move |e| {
                PackageCacheLayerError::LockError(
                    format!(
                        "failed to acquire write lock on global cache lock file: '{}'",
                        lock_file_path.display()
                    ),
                    e,
                )
            })?;

            Ok(CacheGlobalLock { file })
        });

        tokio::select!(
            lock = acquire_lock_fut => lock,
            _ = warn_timeout_future(
                "Blocking waiting for global file lock on package cache".to_string()
            ) => unreachable!("warn_timeout_future should never finish")
        )
    }
}

/// A handle to a cache metadata file.
///
/// This struct manages access to a `.lock` file that stores metadata about a cache entry,
/// including its revision number and optional SHA256 hash. It does not provide filesystem
/// locking - concurrent access should be coordinated via [`CacheGlobalLock`].
pub struct CacheMetadataFile {
    file: Arc<std::fs::File>,
}

impl CacheMetadataFile {
    /// Acquires a handle to the cache metadata file.
    ///
    /// Opens the file with both read and write permissions. Since concurrent access
    /// is coordinated via [`CacheGlobalLock`], this single method is sufficient for
    /// all metadata operations.
    pub async fn acquire(path: &Path) -> Result<Self, PackageCacheLayerError> {
        let lock_file_path = path.to_path_buf();

        simple_spawn_blocking::tokio::run_blocking_task(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_file_path)
                .map_err(|e| {
                    PackageCacheLayerError::LockError(
                        format!(
                            "failed to open cache metadata file: '{}'",
                            lock_file_path.display()
                        ),
                        e,
                    )
                })?;

            Ok(CacheMetadataFile {
                file: Arc::new(file),
            })
        })
        .await
    }
}

/// On-disk metadata layout. Digest presence is derived from total length, so
/// the four states map to distinct lengths and can't be confused:
///
/// | length | contents                |
/// |--------|-------------------------|
/// | 8      | revision only           |
/// | 24     | revision + md5          |
/// | 40     | revision + sha256       |
/// | 56     | revision + sha256 + md5 |
///
/// Backwards compatible: the legacy 8/40-byte files read back unchanged, so
/// existing caches stay valid. md5 is an append older binaries ignore.
const REVISION_LEN: u64 = 8;
const SHA256_LEN: u64 = 32;
const MD5_LEN: u64 = 16;

/// Content digests a caller pinned for a package request.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RequestedDigests {
    pub(super) sha256: Option<Sha256Hash>,
    pub(super) md5: Option<Md5Hash>,
}

impl RequestedDigests {
    /// Serializes the present digests in on-disk order: sha256 (if any)
    /// followed by md5 (if any). See the layout table on [`REVISION_LEN`].
    fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity((SHA256_LEN + MD5_LEN) as usize);
        if let Some(sha256) = self.sha256 {
            bytes.extend_from_slice(&sha256[..]);
        }
        if let Some(md5) = self.md5 {
            bytes.extend_from_slice(&md5[..]);
        }
        bytes
    }
}

/// Content digests recorded for an existing cache entry.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RecordedDigests {
    pub(super) sha256: Option<Sha256Hash>,
    pub(super) md5: Option<Md5Hash>,
}

impl RecordedDigests {
    /// Parses digest bytes written by [`RequestedDigests::encode`] (the file
    /// contents after the revision). Presence is recovered from the length:
    /// a leading 32 bytes is the sha256, a trailing 16 bytes is the md5.
    fn decode(mut bytes: &[u8]) -> Self {
        let mut digests = RecordedDigests::default();
        if bytes.len() >= SHA256_LEN as usize {
            let mut sha256 = [0u8; SHA256_LEN as usize];
            sha256.copy_from_slice(&bytes[..SHA256_LEN as usize]);
            digests.sha256 = Some(Sha256Hash::from(sha256));
            bytes = &bytes[SHA256_LEN as usize..];
        }
        if bytes.len() >= MD5_LEN as usize {
            let mut md5 = [0u8; MD5_LEN as usize];
            md5.copy_from_slice(&bytes[..MD5_LEN as usize]);
            digests.md5 = Some(Md5Hash::from(md5));
        }
        digests
    }

    /// Whether this entry may satisfy a checksum-pinned `requested`. A pinned
    /// digest must match the recorded one; sha256 takes precedence over md5
    /// (mirroring download verification). An unpinned request matches anything.
    pub(super) fn satisfies(self, requested: RequestedDigests) -> bool {
        match (requested.sha256, requested.md5) {
            (Some(sha256), _) => self.sha256 == Some(sha256),
            (None, Some(md5)) => self.md5 == Some(md5),
            (None, None) => true,
        }
    }
}

impl CacheMetadataFile {
    pub async fn write_revision_and_digests(
        &mut self,
        revision: u64,
        digests: RequestedDigests,
    ) -> Result<(), PackageCacheLayerError> {
        let file = self.file.clone();

        // revision followed by the digests in their on-disk order.
        let revision_bytes = revision.to_be_bytes();
        let digest_bytes = digests.encode();
        simple_spawn_blocking::tokio::run_blocking_task(move || {
            // Ensure we write from the start of the file
            (&*file).rewind().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to rewind cache lock for reading revision".to_string(),
                    e,
                )
            })?;

            (&*file).write_all(&revision_bytes).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to write revision from cache lock".to_string(),
                    e,
                )
            })?;

            (&*file).write_all(&digest_bytes).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to write digests from cache lock".to_string(),
                    e,
                )
            })?;

            // Ensure all bytes are written to disk
            (&*file).flush().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to flush cache lock after writing revision".to_string(),
                    e,
                )
            })?;

            // Update the length of the file
            let file_length = revision_bytes.len() + digest_bytes.len();
            file.set_len(file_length as u64).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to truncate cache lock after writing revision".to_string(),
                    e,
                )
            })?;

            Ok(())
        })
        .await
    }

    /// Total file length in bytes; determines which digest fields are present.
    fn len(&self) -> Result<u64, PackageCacheLayerError> {
        self.file.metadata().map(|m| m.len()).map_err(|e| {
            PackageCacheLayerError::LockError("failed to stat cache lock".to_string(), e)
        })
    }

    /// Reads the revision from the cache metadata file.
    pub fn read_revision(&mut self) -> Result<u64, PackageCacheLayerError> {
        (&*self.file).rewind().map_err(|e| {
            PackageCacheLayerError::LockError(
                "failed to rewind cache lock for reading revision".to_string(),
                e,
            )
        })?;
        let mut buf = [0; 8];
        match (&*self.file).read_exact(&mut buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(0);
            }
            Err(e) => {
                return Err(PackageCacheLayerError::LockError(
                    "failed to read revision from cache lock".to_string(),
                    e,
                ));
            }
        }
        Ok(u64::from_be_bytes(buf))
    }

    /// Reads the digests recorded for this cache entry, i.e. everything after
    /// the revision, parsed by [`RecordedDigests::decode`].
    pub fn read_recorded_digests(&mut self) -> Result<RecordedDigests, PackageCacheLayerError> {
        let digest_len = self.len()?.saturating_sub(REVISION_LEN);
        let mut buf = vec![0u8; digest_len as usize];
        (&*self.file)
            .seek(SeekFrom::Start(REVISION_LEN))
            .map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to seek to digests in cache lock".to_string(),
                    e,
                )
            })?;
        if let Err(e) = (&*self.file).read_exact(&mut buf) {
            // A truncated/short file simply records fewer digests.
            if e.kind() != std::io::ErrorKind::UnexpectedEof {
                return Err(PackageCacheLayerError::LockError(
                    "failed to read digests from cache lock".to_string(),
                    e,
                ));
            }
        }
        Ok(RecordedDigests::decode(&buf))
    }
}

async fn warn_timeout_future(message: String) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        tracing::warn!("{}", &message);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rattler_digest::{Md5, Sha256, parse_digest_from_hex};

    use super::{
        CacheMetadataFile, MD5_LEN, Md5Hash, REVISION_LEN, RecordedDigests, RequestedDigests,
        SHA256_LEN, Sha256Hash,
    };

    fn sample_sha() -> Sha256Hash {
        parse_digest_from_hex::<Sha256>(
            "4dd9893f1eee45e1579d1a4f5533ef67a84b5e4b7515de7ed0db1dd47adc6bc8",
        )
        .unwrap()
    }

    fn sample_md5() -> Md5Hash {
        parse_digest_from_hex::<Md5>("d41d8cd98f00b204e9800998ecf8427e").unwrap()
    }

    async fn temp_metadata() -> (tempfile::TempDir, CacheMetadataFile) {
        let dir = tempfile::tempdir().unwrap();
        let file = CacheMetadataFile::acquire(&dir.path().join("meta.lock"))
            .await
            .unwrap();
        (dir, file)
    }

    /// `encode` then `decode` must reproduce the original digests, and the
    /// encoded length must match the layout the readers rely on.
    #[test]
    fn requested_digests_encode_decode_roundtrip() {
        let sha = Some(sample_sha());
        let md5 = Some(sample_md5());

        for (sha_in, md5_in, expected_len) in [
            (None, None, 0),
            (None, md5, MD5_LEN),
            (sha, None, SHA256_LEN),
            (sha, md5, SHA256_LEN + MD5_LEN),
        ] {
            let encoded = RequestedDigests {
                sha256: sha_in,
                md5: md5_in,
            }
            .encode();
            assert_eq!(encoded.len() as u64, expected_len);

            let decoded = RecordedDigests::decode(&encoded);
            assert_eq!(decoded.sha256, sha_in);
            assert_eq!(decoded.md5, md5_in);
        }
    }

    /// The file path preserves the revision and round-trips every digest
    /// combination through the length-discriminated layout.
    #[tokio::test]
    async fn cache_metadata_file_roundtrip() {
        let sha = Some(sample_sha());
        let md5 = Some(sample_md5());

        for (sha_in, md5_in) in [(None, None), (sha, None), (None, md5), (sha, md5)] {
            let (_dir, mut metadata) = temp_metadata().await;
            metadata
                .write_revision_and_digests(
                    7,
                    RequestedDigests {
                        sha256: sha_in,
                        md5: md5_in,
                    },
                )
                .await
                .unwrap();

            assert_eq!(metadata.read_revision().unwrap(), 7);
            let recorded = metadata.read_recorded_digests().unwrap();
            assert_eq!(recorded.sha256, sha_in);
            assert_eq!(recorded.md5, md5_in);
        }
    }

    /// The legacy revision + sha256 layout must read back unchanged, so
    /// upgrading does not invalidate existing caches.
    #[tokio::test]
    async fn cache_metadata_reads_legacy_revision_and_sha_layout() {
        let sha = sample_sha();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.lock");

        // Hand-write the legacy 40-byte layout: revision + sha256, no md5.
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&3u64.to_be_bytes()).unwrap();
        f.write_all(&sha[..]).unwrap();
        f.flush().unwrap();
        assert_eq!(f.metadata().unwrap().len(), REVISION_LEN + SHA256_LEN);

        let mut metadata = CacheMetadataFile::acquire(&path).await.unwrap();
        assert_eq!(metadata.read_revision().unwrap(), 3);
        let recorded = metadata.read_recorded_digests().unwrap();
        assert_eq!(recorded.sha256, Some(sha));
        assert_eq!(recorded.md5, None);
    }
}
