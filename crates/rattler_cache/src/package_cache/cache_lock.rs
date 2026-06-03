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

/// On-disk layout of the cache metadata file.
///
/// The file is a flat sequence of fixed-width fields. Which optional digests
/// are present is recovered purely from the total file length, so the four
/// possible states map to four distinct lengths and can never be confused:
///
/// | length | contents                              |
/// |--------|---------------------------------------|
/// | 8      | revision only (no digest recorded)    |
/// | 24     | revision + md5 (no sha256)            |
/// | 40     | revision + sha256 (no md5)            |
/// | 56     | revision + sha256 + md5               |
///
/// This is backwards compatible with the previous format, which wrote either
/// 8 bytes (revision only) or 40 bytes (revision + sha256): those files keep
/// reading back the exact same revision/sha256 they did before, so existing
/// caches stay valid and are *not* re-fetched on upgrade. The md5 field is a
/// pure addition that older binaries simply ignore (they only ever read the
/// first 40 bytes).
const REVISION_LEN: u64 = 8;
const SHA256_LEN: u64 = 32;
const MD5_LEN: u64 = 16;

impl CacheMetadataFile {
    pub async fn write_revision_and_digests(
        &mut self,
        revision: u64,
        sha256: Option<&Sha256Hash>,
        md5: Option<&Md5Hash>,
    ) -> Result<(), PackageCacheLayerError> {
        let file = self.file.clone();

        let sha256 = sha256.cloned();
        let md5 = md5.cloned();
        simple_spawn_blocking::tokio::run_blocking_task(move || {
            // Ensure we write from the start of the file
            (&*file).rewind().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to rewind cache lock for reading revision".to_string(),
                    e,
                )
            })?;

            // Write the bytes of the revision
            let revision_bytes = revision.to_be_bytes();
            (&*file).write_all(&revision_bytes).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to write revision from cache lock".to_string(),
                    e,
                )
            })?;

            // Write the bytes of the sha256 hash (if any). Keeping sha256
            // directly after the revision preserves the legacy on-disk layout.
            let sha_bytes = if let Some(sha) = sha256 {
                (&*file).write_all(&sha[..]).map_err(|e| {
                    PackageCacheLayerError::LockError(
                        "failed to write sha256 from cache lock".to_string(),
                        e,
                    )
                })?;
                sha.len()
            } else {
                0
            };

            // Write the bytes of the md5 hash (if any), after the sha256 slot.
            let md5_bytes = if let Some(md5) = md5 {
                (&*file).write_all(&md5[..]).map_err(|e| {
                    PackageCacheLayerError::LockError(
                        "failed to write md5 from cache lock".to_string(),
                        e,
                    )
                })?;
                md5.len()
            } else {
                0
            };

            // Ensure all bytes are written to disk
            (&*file).flush().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to flush cache lock after writing revision".to_string(),
                    e,
                )
            })?;

            // Update the length of the file
            let file_length = revision_bytes.len() + sha_bytes + md5_bytes;
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

    /// Returns the total length of the metadata file in bytes, used to
    /// determine which optional digest fields are present.
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

    /// Reads the sha256 hash from the cache metadata file.
    ///
    /// The sha256 is present only when the file is long enough to hold a
    /// revision followed by a full sha256 (see the layout table on
    /// [`REVISION_LEN`]). A shorter file (e.g. revision-only, or
    /// revision + md5) reports no sha256.
    pub fn read_sha256(&mut self) -> Result<Option<Sha256Hash>, PackageCacheLayerError> {
        if self.len()? < REVISION_LEN + SHA256_LEN {
            return Ok(None);
        }
        (&*self.file)
            .seek(SeekFrom::Start(REVISION_LEN))
            .map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to seek to sha256 in cache lock".to_string(),
                    e,
                )
            })?;
        let mut buf = [0; SHA256_LEN as usize];
        match (&*self.file).read_exact(&mut buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => {
                return Err(PackageCacheLayerError::LockError(
                    "failed to read sha256 from cache lock".to_string(),
                    e,
                ));
            }
        }
        Ok(Some(Sha256Hash::from(buf)))
    }

    /// Reads the md5 hash from the cache metadata file.
    ///
    /// The md5 is stored after the (optional) sha256, so its offset depends on
    /// whether a sha256 is present: it lives at `REVISION_LEN + SHA256_LEN`
    /// when a sha256 was recorded, and directly at `REVISION_LEN` otherwise.
    /// Presence is again derived from the total file length.
    pub fn read_md5(&mut self) -> Result<Option<Md5Hash>, PackageCacheLayerError> {
        let len = self.len()?;
        // If a sha256 is present the md5 follows it; otherwise it sits right
        // after the revision.
        let offset = if len >= REVISION_LEN + SHA256_LEN {
            REVISION_LEN + SHA256_LEN
        } else {
            REVISION_LEN
        };
        if len < offset + MD5_LEN {
            return Ok(None);
        }
        (&*self.file).seek(SeekFrom::Start(offset)).map_err(|e| {
            PackageCacheLayerError::LockError("failed to seek to md5 in cache lock".to_string(), e)
        })?;
        let mut buf = [0; MD5_LEN as usize];
        match (&*self.file).read_exact(&mut buf) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => {
                return Err(PackageCacheLayerError::LockError(
                    "failed to read md5 from cache lock".to_string(),
                    e,
                ));
            }
        }
        Ok(Some(Md5Hash::from(buf)))
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

    use super::{CacheMetadataFile, REVISION_LEN, SHA256_LEN};

    #[tokio::test]
    async fn cache_metadata_serialize_deserialize() {
        // Temporarily create a metadata file and write a revision and sha to it
        let temp_dir = tempfile::tempdir().unwrap();
        let metadata_file = temp_dir.path().join("foo.lock");
        // Acquire a handle on the file
        let mut metadata = CacheMetadataFile::acquire(&metadata_file).await.unwrap();
        // Write a revision and sha to the lock file
        let sha = parse_digest_from_hex::<Sha256>(
            "4dd9893f1eee45e1579d1a4f5533ef67a84b5e4b7515de7ed0db1dd47adc6bc8",
        );
        metadata
            .write_revision_and_digests(1, sha.as_ref(), None)
            .await
            .unwrap();
        // Read back the revision and sha from the metadata file
        let revision = metadata.read_revision().unwrap();
        assert_eq!(revision, 1);
        let read_sha = metadata.read_sha256().unwrap();
        assert_eq!(sha, read_sha);
        // No md5 was written.
        assert_eq!(metadata.read_md5().unwrap(), None);
    }

    /// Every combination of present/absent sha256 and md5 must round-trip,
    /// since presence is recovered from the file length alone.
    #[tokio::test]
    async fn cache_metadata_roundtrips_all_digest_combinations() {
        let sha = parse_digest_from_hex::<Sha256>(
            "4dd9893f1eee45e1579d1a4f5533ef67a84b5e4b7515de7ed0db1dd47adc6bc8",
        );
        let md5 = parse_digest_from_hex::<Md5>("d41d8cd98f00b204e9800998ecf8427e");

        for (sha_in, md5_in) in [(None, None), (sha, None), (None, md5), (sha, md5)] {
            let temp_dir = tempfile::tempdir().unwrap();
            let metadata_file = temp_dir.path().join("foo.lock");
            let mut metadata = CacheMetadataFile::acquire(&metadata_file).await.unwrap();
            metadata
                .write_revision_and_digests(7, sha_in.as_ref(), md5_in.as_ref())
                .await
                .unwrap();

            assert_eq!(metadata.read_revision().unwrap(), 7);
            assert_eq!(metadata.read_sha256().unwrap(), sha_in);
            assert_eq!(metadata.read_md5().unwrap(), md5_in);
        }
    }

    /// A metadata file written by an older rattler (revision + raw sha256, no
    /// md5 field) must still read back its revision and sha256 unchanged, so
    /// upgrading does not invalidate existing caches.
    #[tokio::test]
    async fn cache_metadata_reads_legacy_revision_and_sha_layout() {
        let sha = parse_digest_from_hex::<Sha256>(
            "4dd9893f1eee45e1579d1a4f5533ef67a84b5e4b7515de7ed0db1dd47adc6bc8",
        )
        .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let metadata_file = temp_dir.path().join("legacy.lock");

        // Hand-write the legacy 40-byte layout: 8-byte revision + 32-byte sha256.
        {
            let mut f = std::fs::File::create(&metadata_file).unwrap();
            f.write_all(&3u64.to_be_bytes()).unwrap();
            f.write_all(&sha[..]).unwrap();
            f.flush().unwrap();
        }
        assert_eq!(
            std::fs::metadata(&metadata_file).unwrap().len(),
            REVISION_LEN + SHA256_LEN
        );

        let mut metadata = CacheMetadataFile::acquire(&metadata_file).await.unwrap();
        assert_eq!(metadata.read_revision().unwrap(), 3);
        assert_eq!(metadata.read_sha256().unwrap(), Some(sha));
        assert_eq!(metadata.read_md5().unwrap(), None);
    }
}
