use std::{
    fmt::{Debug, Formatter},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use fs4::fs_std::FileExt;
use rattler_conda_types::package::{IndexJson, PathsJson};
use rattler_digest::serde::SerializableHash;
use rattler_digest::{Md5Hash, Sha256Hash};
use serde_with::serde_as;

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

/// Versioned on-disk metadata: a fixed `MAGIC` prefix, a version byte, then a
/// `MessagePack`-encoded [`StoredMetadataV1`] body.
///
/// The frame lets [`StoredMetadata::decode`] tell this apart from the legacy
/// raw `revision (+ sha256)` layout with one byte compare: the first `MAGIC`
/// byte is nonzero, so it never collides with a legacy file (whose leading
/// bytes are the zero high bytes of a big-endian revision). Decoding tries the
/// versioned body, then the legacy layout (so existing caches still read), and
/// otherwise falls back to empty metadata, forcing a safe re-fetch. A future
/// layout bumps `VERSION` (with a `StoredMetadataV2`); older binaries fall back
/// instead of misreading it.
const MAGIC: [u8; 4] = *b"RCM1";
const VERSION: u8 = 1;
const SHA256_LEN: usize = 32;
/// Length of the legacy layout's leading big-endian revision counter.
const LEGACY_REVISION_LEN: usize = 8;

/// Content digests a caller pinned for a package request.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RequestedDigests {
    pub(super) sha256: Option<Sha256Hash>,
    pub(super) md5: Option<Md5Hash>,
}

/// Content digests recorded for an existing cache entry.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RecordedDigests {
    pub(super) sha256: Option<Sha256Hash>,
    pub(super) md5: Option<Md5Hash>,
}

impl RecordedDigests {
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

/// `MessagePack` body of the versioned format. Digests serialize as raw bytes
/// via [`SerializableHash`]. New fields can go in a future `StoredMetadataV2`
/// behind a bumped [`VERSION`].
#[serde_as]
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredMetadataV1 {
    revision: u64,
    #[serde_as(as = "Option<SerializableHash::<rattler_digest::Sha256>>")]
    sha256: Option<Sha256Hash>,
    #[serde_as(as = "Option<SerializableHash::<rattler_digest::Md5>>")]
    md5: Option<Md5Hash>,
}

/// The parsed contents of a cache metadata file: a revision counter plus the
/// digests recorded for the entry.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct StoredMetadata {
    pub(super) revision: u64,
    pub(super) digests: RecordedDigests,
}

impl StoredMetadata {
    /// Serializes `revision` and `digests` in the current versioned format.
    fn encode(revision: u64, digests: RequestedDigests) -> Vec<u8> {
        let body = StoredMetadataV1 {
            revision,
            sha256: digests.sha256,
            md5: digests.md5,
        };
        let mut bytes = Vec::with_capacity(MAGIC.len() + 1 + 64);
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        rmp_serde::encode::write(&mut bytes, &body)
            .expect("serializing cache metadata to a Vec cannot fail");
        bytes
    }

    /// Parses metadata bytes. Tries the current versioned format, then the
    /// legacy `revision (+ sha256)` layout, then falls back to empty metadata
    /// (revision 0, no digests), which makes the caller re-fetch rather than
    /// trust unparseable bytes.
    fn decode(bytes: &[u8]) -> Self {
        Self::decode_versioned(bytes)
            .or_else(|| Self::decode_legacy(bytes))
            .unwrap_or_default()
    }

    fn decode_versioned(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC)?;
        let (&version, body) = rest.split_first()?;
        if version != VERSION {
            // A future version we don't understand: let the caller fall back.
            return None;
        }
        let body: StoredMetadataV1 = rmp_serde::from_slice(body).ok()?;
        Some(Self {
            revision: body.revision,
            digests: RecordedDigests {
                sha256: body.sha256,
                md5: body.md5,
            },
        })
    }

    fn decode_legacy(bytes: &[u8]) -> Option<Self> {
        // A versioned file starts with MAGIC; never misinterpret it as legacy.
        if bytes.starts_with(&MAGIC) {
            return None;
        }
        // Empty/short file: no recorded revision yet.
        let Some((revision, rest)) = bytes.split_at_checked(LEGACY_REVISION_LEN) else {
            return Some(Self::default());
        };
        let revision = u64::from_be_bytes(revision.try_into().ok()?);
        let sha256 = rest
            .get(..SHA256_LEN)
            .and_then(|s| <[u8; SHA256_LEN]>::try_from(s).ok())
            .map(Sha256Hash::from);
        Some(Self {
            revision,
            digests: RecordedDigests { sha256, md5: None },
        })
    }
}

impl CacheMetadataFile {
    /// Writes `revision` and `digests` in the current versioned format,
    /// replacing any previous contents.
    pub async fn write(
        &mut self,
        revision: u64,
        digests: RequestedDigests,
    ) -> Result<(), PackageCacheLayerError> {
        let file = self.file.clone();
        let bytes = StoredMetadata::encode(revision, digests);
        simple_spawn_blocking::tokio::run_blocking_task(move || {
            (&*file).rewind().map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to rewind cache lock for writing".to_string(),
                    e,
                )
            })?;
            (&*file).write_all(&bytes).map_err(|e| {
                PackageCacheLayerError::LockError("failed to write cache metadata".to_string(), e)
            })?;
            (&*file).flush().map_err(|e| {
                PackageCacheLayerError::LockError("failed to flush cache metadata".to_string(), e)
            })?;
            file.set_len(bytes.len() as u64).map_err(|e| {
                PackageCacheLayerError::LockError(
                    "failed to truncate cache metadata".to_string(),
                    e,
                )
            })?;
            Ok(())
        })
        .await
    }

    /// Reads and parses the metadata file. Unparseable content yields empty
    /// metadata (revision 0, no digests) via [`StoredMetadata::decode`].
    pub fn read(&mut self) -> Result<StoredMetadata, PackageCacheLayerError> {
        (&*self.file).rewind().map_err(|e| {
            PackageCacheLayerError::LockError(
                "failed to rewind cache lock for reading".to_string(),
                e,
            )
        })?;
        let mut buf = Vec::new();
        (&*self.file).read_to_end(&mut buf).map_err(|e| {
            PackageCacheLayerError::LockError("failed to read cache metadata".to_string(), e)
        })?;
        Ok(StoredMetadata::decode(&buf))
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
        CacheMetadataFile, LEGACY_REVISION_LEN, MAGIC, Md5Hash, RequestedDigests, SHA256_LEN,
        Sha256Hash, StoredMetadata, VERSION,
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

    /// `encode` then `decode` reproduces the revision and digests for every
    /// digest combination, and produces the current versioned format.
    #[test]
    fn stored_metadata_encode_decode_roundtrip() {
        let sha = Some(sample_sha());
        let md5 = Some(sample_md5());

        for (sha_in, md5_in) in [(None, None), (sha, None), (None, md5), (sha, md5)] {
            let bytes = StoredMetadata::encode(
                9,
                RequestedDigests {
                    sha256: sha_in,
                    md5: md5_in,
                },
            );
            assert!(bytes.starts_with(&MAGIC));
            assert_eq!(bytes[MAGIC.len()], VERSION);

            let decoded = StoredMetadata::decode(&bytes);
            assert_eq!(decoded.revision, 9);
            assert_eq!(decoded.digests.sha256, sha_in);
            assert_eq!(decoded.digests.md5, md5_in);
        }
    }

    /// The file path preserves the revision and round-trips every digest combo.
    #[tokio::test]
    async fn cache_metadata_file_roundtrip() {
        let sha = Some(sample_sha());
        let md5 = Some(sample_md5());

        for (sha_in, md5_in) in [(None, None), (sha, None), (None, md5), (sha, md5)] {
            let (_dir, mut metadata) = temp_metadata().await;
            metadata
                .write(
                    7,
                    RequestedDigests {
                        sha256: sha_in,
                        md5: md5_in,
                    },
                )
                .await
                .unwrap();

            let stored = metadata.read().unwrap();
            assert_eq!(stored.revision, 7);
            assert_eq!(stored.digests.sha256, sha_in);
            assert_eq!(stored.digests.md5, md5_in);
        }
    }

    /// A freshly created (empty) metadata file reads back as the default.
    #[tokio::test]
    async fn cache_metadata_empty_file_is_default() {
        let (_dir, mut metadata) = temp_metadata().await;
        let stored = metadata.read().unwrap();
        assert_eq!(stored.revision, 0);
        assert_eq!(stored.digests.sha256, None);
        assert_eq!(stored.digests.md5, None);
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
        assert_eq!(
            f.metadata().unwrap().len() as usize,
            LEGACY_REVISION_LEN + SHA256_LEN
        );

        let mut metadata = CacheMetadataFile::acquire(&path).await.unwrap();
        let stored = metadata.read().unwrap();
        assert_eq!(stored.revision, 3);
        assert_eq!(stored.digests.sha256, Some(sha));
        assert_eq!(stored.digests.md5, None);
    }

    /// An unknown future version is not trusted: decode falls back to empty
    /// metadata so the caller re-fetches instead of misreading the payload.
    #[test]
    fn decode_unknown_version_falls_back_to_empty() {
        let mut bytes = MAGIC.to_vec();
        bytes.push(VERSION + 1); // a version this binary does not understand
        bytes.extend_from_slice(&[0xab; 48]); // arbitrary body we must not trust

        let decoded = StoredMetadata::decode(&bytes);
        assert_eq!(decoded.revision, 0);
        assert_eq!(decoded.digests.sha256, None);
        assert_eq!(decoded.digests.md5, None);
    }
}
