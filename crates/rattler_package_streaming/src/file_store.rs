//! A content-addressed store that deduplicates the files of extracted
//! packages.
//!
//! Every regular file of a package is extracted into the package directory as
//! usual. Afterwards each file is hard linked into the store under the BLAKE3
//! hash of its contents and executable bit, so packages that ship identical
//! files share one inode. When a store object already exists, the package's
//! private copy is replaced by a link to it. Small files are looked up before
//! they are written at all.
//!
//! Package directories stay complete on their own: a file that cannot be
//! linked, for example because the store is on another filesystem or the
//! filesystem's link limit is reached, is simply left as a private copy.
//! Files under `info/` are never shared, so package metadata can be updated
//! in place without affecting other packages.
//!
//! Objects are immutable once published. Their mode and timestamps are set
//! while the file is still private, and never touched afterwards, since every
//! package sharing the object would see the change.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

use rayon::prelude::*;

/// Files up to this size are hashed in memory and linked from an existing
/// store object before anything is written to the package directory.
pub(crate) const SMALL_FILE_LIMIT: u64 = 64 * 1024;

/// Domain separation for object keys, so a key never equals a plain content
/// hash and the executable bit is part of the identity.
const OBJECT_KEY_CONTEXT: &str = "rattler package file store v1";

/// Files smaller than this are not shared. Tiny files carry almost none of
/// the space savings while each one costs a link call per package. The value
/// still has to be chosen from measurements; see
/// `docs/design/package-file-store.md`.
pub(crate) fn min_shared_file_size() -> u64 {
    0
}

/// A content-addressed file store rooted at a directory.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

/// The shard directories that exist in a store at the start of an
/// extraction. A cold store has none, so no object lookup can succeed and the
/// lookups are skipped entirely; a warm store has all 256.
#[derive(Debug, Default)]
pub(crate) struct KnownShards(std::collections::HashSet<String>);

impl KnownShards {
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.0.contains(&key[..2])
    }
}

/// A regular file that was extracted from a package.
#[derive(Debug, Clone)]
pub struct ExtractedFile {
    /// Absolute path of the file in the package directory.
    path: PathBuf,
    /// Hash of the file contents.
    digest: blake3::Hash,
    /// Whether any executable bit is set in the archive entry's mode.
    executable: bool,
    /// Size of the file in bytes.
    size: u64,
    /// Whether the file was linked from an existing object instead of written.
    reused: bool,
}

impl ExtractedFile {
    pub(crate) fn new(
        path: PathBuf,
        digest: blake3::Hash,
        executable: bool,
        size: u64,
        reused: bool,
    ) -> Self {
        Self {
            path,
            digest,
            executable,
            size,
            reused,
        }
    }
}

/// What publishing a package's files to the store did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileStoreStats {
    /// Regular files that were eligible for sharing.
    pub files: usize,
    /// Files whose contents were new to the store.
    pub added: usize,
    /// Files that now share an object that already existed.
    pub reused: usize,
    /// Files that stayed private because linking failed.
    pub unshared: usize,
    /// Bytes of reused files, which the package would otherwise occupy again.
    pub bytes_reused: u64,
}

impl FileStore {
    /// Creates a store rooted at `root`. The directory is created on first
    /// use.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory that holds the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of the object for the given contents and executable bit,
    /// whether or not it exists.
    pub fn object_path(&self, digest: &blake3::Hash, executable: bool) -> PathBuf {
        self.object_path_for_key(&object_key(digest, executable))
    }

    fn object_path_for_key(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(key)
    }

    /// Lists the shard directories that currently exist.
    pub(crate) fn known_shards(&self) -> KnownShards {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return KnownShards::default();
        };
        KnownShards(
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.len() == 2)
                .collect(),
        )
    }

    /// Links an existing object to `destination`. Returns `Ok(false)` when
    /// the object does not exist, so the caller writes the file instead.
    /// Objects in shards that did not exist when `shards` was listed are
    /// assumed absent without touching the filesystem.
    pub(crate) fn link_existing(
        &self,
        shards: &KnownShards,
        digest: &blake3::Hash,
        executable: bool,
        destination: &Path,
    ) -> io::Result<bool> {
        let key = object_key(digest, executable);
        if !shards.contains(&key) {
            return Ok(false);
        }
        match std::fs::hard_link(self.object_path_for_key(&key), destination) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Shares the given files through the store. Files that were linked from
    /// an existing object during extraction are counted but not touched.
    ///
    /// Files are grouped by shard directory so each shard is created once and
    /// linked by one worker, which avoids contention on the shard directory.
    /// Never fails: a file that cannot be shared stays a private copy.
    pub(crate) fn publish(&self, files: &[ExtractedFile]) -> FileStoreStats {
        let mut stats = FileStoreStats {
            files: files.len(),
            ..FileStoreStats::default()
        };

        let mut shards: HashMap<PathBuf, Vec<(&ExtractedFile, PathBuf)>> = HashMap::new();
        for file in files {
            if file.reused {
                stats.reused += 1;
                stats.bytes_reused += file.size;
                continue;
            }
            let object = self.object_path(&file.digest, file.executable);
            let shard = object
                .parent()
                .expect("object paths always have a shard directory")
                .to_path_buf();
            shards.entry(shard).or_default().push((file, object));
        }
        if shards.is_empty() {
            return stats;
        }

        // Creating shard directories concurrently contends on the store root,
        // so create them up front and let workers link inside them. A store
        // that cannot be written leaves the package with private copies.
        let mut ready = Vec::with_capacity(shards.len());
        for (shard, files) in shards {
            match std::fs::create_dir_all(&shard) {
                Ok(()) => ready.push(files),
                Err(err) => {
                    tracing::debug!("could not create {}: {err}", shard.display());
                    stats.unshared += files.len();
                }
            }
        }
        let shards = ready;
        let linked = shards
            .par_iter()
            .map(|files| {
                let mut shard_stats = FileStoreStats::default();
                for (file, object) in files {
                    publish_file(&file.path, object, file.size, &mut shard_stats);
                }
                shard_stats
            })
            .reduce(FileStoreStats::default, |a, b| FileStoreStats {
                files: 0,
                added: a.added + b.added,
                reused: a.reused + b.reused,
                unshared: a.unshared + b.unshared,
                bytes_reused: a.bytes_reused + b.bytes_reused,
            });

        stats.added += linked.added;
        stats.reused += linked.reused;
        stats.unshared += linked.unshared;
        stats.bytes_reused += linked.bytes_reused;
        stats
    }
}

/// Publishes one private file. Most objects are new, so the link is attempted
/// before checking whether the object exists.
fn publish_file(source: &Path, object: &Path, size: u64, stats: &mut FileStoreStats) {
    match std::fs::hard_link(source, object) {
        Ok(()) => {
            stats.added += 1;
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            // The package directory is still private, so its copy can be
            // swapped for a link to the shared object.
            match replace_with_link(source, object) {
                Ok(()) => {
                    stats.reused += 1;
                    stats.bytes_reused += size;
                }
                Err(err) => {
                    tracing::debug!("keeping a private copy of {}: {err}", source.display());
                    stats.unshared += 1;
                }
            }
        }
        Err(err) => {
            // Cross-filesystem stores and link-count limits end up here. The
            // package directory is complete without the store.
            tracing::debug!("could not share {}: {err}", source.display());
            stats.unshared += 1;
        }
    }
}

/// Replaces `source` with a hard link to `object`, restoring a copy if the
/// link cannot be created after the original was removed.
fn replace_with_link(source: &Path, object: &Path) -> io::Result<()> {
    std::fs::remove_file(source)?;
    match std::fs::hard_link(object, source) {
        Ok(()) => Ok(()),
        Err(link_err) => {
            tracing::debug!(
                "could not link {} to {}, copying instead: {link_err}",
                source.display(),
                object.display()
            );
            std::fs::copy(object, source).map(|_| ())
        }
    }
}

/// Derives the object key from the content hash and executable bit.
fn object_key(digest: &blake3::Hash, executable: bool) -> String {
    let mut hasher = blake3::Hasher::new_derive_key(OBJECT_KEY_CONTEXT);
    hasher.update(digest.as_bytes());
    hasher.update(&[u8::from(executable)]);
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_bit_is_part_of_the_key() {
        let store = FileStore::new("store");
        let digest = blake3::hash(b"same bytes");
        assert_ne!(
            store.object_path(&digest, false),
            store.object_path(&digest, true)
        );
        let path = store.object_path(&digest, false);
        let shard = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(shard.len(), 2);
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(shard)
        );
    }

    #[test]
    fn publish_shares_identical_files_and_keeps_private_copies_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("store"));
        let pkg_a = dir.path().join("a");
        let pkg_b = dir.path().join("b");
        std::fs::create_dir_all(&pkg_a).unwrap();
        std::fs::create_dir_all(&pkg_b).unwrap();

        let same = b"shared contents".to_vec();
        std::fs::write(pkg_a.join("x"), &same).unwrap();
        std::fs::write(pkg_b.join("y"), &same).unwrap();
        std::fs::write(pkg_b.join("z"), b"only in b").unwrap();

        let file = |path: PathBuf, contents: &[u8]| {
            ExtractedFile::new(
                path,
                blake3::hash(contents),
                false,
                contents.len() as u64,
                false,
            )
        };

        let stats = store.publish(&[file(pkg_a.join("x"), &same)]);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.reused, 0);

        let stats = store.publish(&[
            file(pkg_b.join("y"), &same),
            file(pkg_b.join("z"), b"only in b"),
        ]);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.reused, 1);
        assert_eq!(stats.bytes_reused, same.len() as u64);

        // Both packages now point at the same object and read the same bytes.
        let object = store.object_path(&blake3::hash(&same), false);
        assert!(object.exists());
        assert_eq!(std::fs::read(pkg_a.join("x")).unwrap(), same);
        assert_eq!(std::fs::read(pkg_b.join("y")).unwrap(), same);
        assert!(same_file(&pkg_a.join("x"), &pkg_b.join("y")));

        // A missing source is reported as unshared, not as an error.
        let stats = store.publish(&[file(pkg_b.join("missing"), b"nope")]);
        assert_eq!(stats.unshared, 1);
    }

    #[test]
    fn link_existing_only_links_known_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("store"));
        let contents = b"lookup";
        let digest = blake3::hash(contents);
        let target = dir.path().join("target");

        assert!(
            !store
                .link_existing(&store.known_shards(), &digest, false, &target)
                .unwrap()
        );
        assert!(!target.exists());

        let object = store.object_path(&digest, false);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, contents).unwrap();
        assert!(
            store
                .link_existing(&store.known_shards(), &digest, false, &target)
                .unwrap()
        );
        assert_eq!(std::fs::read(&target).unwrap(), contents);
        assert!(same_file(&object, &target));
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(a).unwrap();
            let b = std::fs::metadata(b).unwrap();
            a.ino() == b.ino() && a.dev() == b.dev()
        }
        #[cfg(windows)]
        {
            // Hard links share their contents and link count.
            std::fs::read(a).unwrap() == std::fs::read(b).unwrap()
        }
    }
}
