//! A content-addressed store that deduplicates the files of extracted
//! packages.
//!
//! Every regular file of a package is extracted into the package directory as
//! usual. Afterwards each file is hard linked into the store under the SHA-256
//! hash of its contents and executable bit, so packages that ship identical
//! files share one inode. When a store object already exists, the package's
//! private copy is replaced by a link to it. Small files are looked up before
//! they are written at all, and larger files are looked up through the
//! SHA-256 that `info/paths.json` records for them, so on a warm store they
//! are never written.
//!
//! Package directories stay complete on their own: a file that cannot be
//! linked, for example because the store is on another filesystem or the
//! filesystem's link limit is reached, is simply left as a private copy.
//! Files under `info/` are never shared, so package metadata can be updated
//! in place without affecting other packages.
//!
//! Objects are immutable once published. Their mode and timestamps are set
//! while the file is still private, and never touched afterwards, since every
//! package sharing the object would see the change. Objects that no package
//! links to any more are removed by [`FileStore::prune`].

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

use rattler_conda_types::package::{PackageFile, PathType, PathsJson};
use rattler_digest::Sha256Hash;
use rayon::prelude::*;

/// Files up to this size are hashed in memory and linked from an existing
/// store object before anything is written to the package directory.
pub(crate) const SMALL_FILE_LIMIT: u64 = 64 * 1024;

/// A content-addressed file store rooted at a directory.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
    min_shared_size: u64,
}

/// The shard directories that exist in a store at the start of an
/// extraction. A cold store has none, so no object lookup can succeed and the
/// lookups are skipped entirely; a warm store has all 256.
#[derive(Debug, Default)]
struct KnownShards(HashSet<String>);

impl KnownShards {
    fn contains(&self, key: &str) -> bool {
        self.0.contains(&key[..2])
    }
}

/// A regular file that was extracted from a package.
#[derive(Debug, Clone)]
pub struct ExtractedFile {
    /// Absolute path of the file in the package directory.
    path: PathBuf,
    /// SHA-256 of the file contents, or `None` for a file that was written
    /// without hashing and is hashed from disk when it is published.
    digest: Option<Sha256Hash>,
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
        digest: Option<Sha256Hash>,
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

/// What pruning a store did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Objects that at least one package still links to.
    pub kept: usize,
    /// Objects that no package linked to any more and were removed.
    pub removed: usize,
    /// Bytes occupied by the removed objects.
    pub bytes_freed: u64,
}

/// What `info/paths.json` records about a file, used to look up a store
/// object before the file's contents are read.
#[derive(Debug, Clone, Copy)]
struct PathHint {
    sha256: Sha256Hash,
    size: Option<u64>,
}

/// The sharing state of one package extraction: the store objects that can
/// be looked up, the files extracted so far, and the `info/paths.json` hints
/// once the package's metadata is on disk.
#[derive(Debug)]
pub(crate) struct FileStoreSession<'a> {
    store: &'a FileStore,
    shards: KnownShards,
    files: Vec<ExtractedFile>,
    /// `None` until the first lookup; an empty map when the package has no
    /// usable `info/paths.json`.
    hints: Option<HashMap<PathBuf, PathHint>>,
}

impl FileStore {
    /// Files smaller than this are not shared by default. Sharing every
    /// file is the measured default: a small file linked from a warm store
    /// is cheaper than writing it, on NTFS by a wide margin, and leaving
    /// small files private only saves a few link calls on a cold store. See
    /// `docs/design/package-file-store.md`.
    pub const DEFAULT_MIN_SHARED_SIZE: u64 = 0;

    /// Creates a store rooted at `root`. The directory is created on first
    /// use.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            min_shared_size: Self::DEFAULT_MIN_SHARED_SIZE,
        }
    }

    /// Leaves files smaller than `size` bytes private instead of sharing
    /// them through the store.
    pub fn with_min_shared_size(self, size: u64) -> Self {
        Self {
            min_shared_size: size,
            ..self
        }
    }

    /// The directory that holds the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of the object for the given contents and executable bit,
    /// whether or not it exists.
    pub fn object_path(&self, digest: &Sha256Hash, executable: bool) -> PathBuf {
        self.object_path_for_key(&object_key(digest, executable))
    }

    fn object_path_for_key(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(key)
    }

    /// Starts sharing the files of one package.
    pub(crate) fn begin(&self) -> FileStoreSession<'_> {
        FileStoreSession {
            store: self,
            shards: self.known_shards(),
            files: Vec::new(),
            hints: None,
        }
    }

    /// Lists the shard directories that currently exist.
    fn known_shards(&self) -> KnownShards {
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
    fn link_existing(
        &self,
        shards: &KnownShards,
        digest: &Sha256Hash,
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
    /// Files that were written without hashing are hashed here from disk, in
    /// parallel, which keeps the hashing of large files off the extraction
    /// thread. Files are then grouped by shard directory so each shard is
    /// created once and linked by one worker, which avoids contention on the
    /// shard directory. Never fails: a file that cannot be shared stays a
    /// private copy.
    fn publish(&self, mut files: Vec<ExtractedFile>) -> FileStoreStats {
        let mut stats = FileStoreStats {
            files: files.len(),
            ..FileStoreStats::default()
        };

        files
            .par_iter_mut()
            .filter(|file| file.digest.is_none() && !file.reused)
            .for_each(|file| {
                match rattler_digest::compute_file_digest::<rattler_digest::Sha256>(&file.path) {
                    Ok(digest) => file.digest = Some(digest),
                    Err(err) => {
                        tracing::debug!("could not hash {}: {err}", file.path.display());
                    }
                }
            });

        let mut shards: HashMap<PathBuf, Vec<(&ExtractedFile, PathBuf)>> = HashMap::new();
        for file in &files {
            if file.reused {
                stats.reused += 1;
                stats.bytes_reused += file.size;
                continue;
            }
            let Some(digest) = &file.digest else {
                stats.unshared += 1;
                continue;
            };
            let object = self.object_path(digest, file.executable);
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

    /// Removes every object that no package links to any more, and every
    /// shard directory that became empty. A missing store is empty.
    ///
    /// Safe to run next to extractions: an object is removed only while its
    /// link count is one, and an extraction that loses the race to link it
    /// simply writes the file and creates the object again.
    pub fn prune(&self) -> io::Result<PruneStats> {
        let shards = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        shards
            .par_iter()
            .map(|shard| prune_shard(shard))
            .try_reduce(PruneStats::default, |a, b| {
                Ok(PruneStats {
                    kept: a.kept + b.kept,
                    removed: a.removed + b.removed,
                    bytes_freed: a.bytes_freed + b.bytes_freed,
                })
            })
    }
}

impl FileStoreSession<'_> {
    /// Whether a regular file at `relative` in the package may be shared.
    /// Package metadata under `info/` is rewritten in place by some tools,
    /// so it is never shared between packages.
    pub(crate) fn is_shareable(&self, relative: &Path, size: u64) -> bool {
        size >= self.store.min_shared_size && !relative.starts_with("info")
    }

    /// Links `destination` to the object for `digest`, if it exists. Returns
    /// whether it did; a failed link is reported and treated as absent.
    pub(crate) fn link_existing(
        &self,
        digest: &Sha256Hash,
        executable: bool,
        destination: &Path,
    ) -> bool {
        match self
            .store
            .link_existing(&self.shards, digest, executable, destination)
        {
            Ok(linked) => linked,
            Err(err) => {
                tracing::debug!(
                    "could not link {} from the store: {err}",
                    destination.display()
                );
                false
            }
        }
    }

    /// The SHA-256 that the package's `info/paths.json` records for the file
    /// at `relative`, when the metadata is already extracted below
    /// `destination` and agrees with the entry's `size`. The hint is only
    /// worth looking up when its object can exist, so a cold store never
    /// reads `paths.json`.
    pub(crate) fn hinted_digest(
        &mut self,
        destination: &Path,
        relative: &Path,
        size: u64,
    ) -> Option<Sha256Hash> {
        if self.shards.0.is_empty() {
            return None;
        }
        let hints = self
            .hints
            .get_or_insert_with(|| load_hints(&destination.join("info").join("paths.json")));
        let hint = hints.get(relative)?;
        (hint.size.is_none_or(|hinted| hinted == size)).then_some(hint.sha256)
    }

    /// Records an extracted regular file for publishing.
    pub(crate) fn record(&mut self, file: ExtractedFile) {
        self.files.push(file);
    }

    /// Shares the recorded files through the store and reports what that
    /// did. Never fails: a file that cannot be shared stays a private copy.
    pub(crate) fn publish(self) -> FileStoreStats {
        self.store.publish(self.files)
    }
}

/// Reads the hints from a `paths.json`. Anything that cannot be read or
/// parsed yields no hints; the files are then hashed while they are written.
fn load_hints(path: &Path) -> HashMap<PathBuf, PathHint> {
    let paths = match PathsJson::from_path(path) {
        Ok(paths) => paths,
        Err(err) => {
            tracing::debug!("no file store hints from {}: {err}", path.display());
            return HashMap::new();
        }
    };
    paths
        .paths
        .into_iter()
        .filter_map(|entry| {
            let sha256 = entry.sha256?;
            match entry.path_type {
                PathType::HardLink => Some((
                    entry.relative_path,
                    PathHint {
                        sha256,
                        size: entry.size_in_bytes,
                    },
                )),
                PathType::SoftLink | PathType::Directory => None,
            }
        })
        .collect()
}

/// Removes the objects in one shard directory that no package links to.
fn prune_shard(shard: &Path) -> io::Result<PruneStats> {
    let mut stats = PruneStats::default();
    for entry in std::fs::read_dir(shard)? {
        let object = entry?.path();
        let (links, size) = match object_links(&object) {
            Ok(info) => info,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if links > 1 {
            stats.kept += 1;
            continue;
        }
        match std::fs::remove_file(&object) {
            Ok(()) => {
                stats.removed += 1;
                stats.bytes_freed += size;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    // Only an empty shard can be removed; a package racing to publish into
    // it recreates the directory.
    if let Err(err) = std::fs::remove_dir(shard)
        && err.kind() != io::ErrorKind::DirectoryNotEmpty
    {
        tracing::debug!("could not remove {}: {err}", shard.display());
    }
    Ok(stats)
}

/// The number of hard links to an object and its size.
#[cfg(unix)]
fn object_links(object: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(object)?;
    Ok((metadata.nlink(), metadata.len()))
}

/// The number of hard links to an object and its size. Windows only reports
/// the link count through an open handle.
#[cfg(windows)]
fn object_links(object: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = std::fs::File::open(object)?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` keeps the handle open for the duration of the call, and
    // `info` is a valid, writable location for a `BY_HANDLE_FILE_INFORMATION`
    // that is only read after the call reports success.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so the structure is fully initialized.
    let info = unsafe { info.assume_init() };
    let size = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
    Ok((u64::from(info.nNumberOfLinks), size))
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

/// The object name for a content hash and executable bit: the hex SHA-256,
/// with an `x` suffix for executables so the two never collide.
fn object_key(digest: &Sha256Hash, executable: bool) -> String {
    let mut key = hex::encode(digest);
    if executable {
        key.push('x');
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use rattler_digest::{Sha256, compute_bytes_digest};

    fn digest(contents: &[u8]) -> Sha256Hash {
        compute_bytes_digest::<Sha256>(contents)
    }

    #[test]
    fn executable_bit_is_part_of_the_key() {
        let store = FileStore::new("store");
        let digest = digest(b"same bytes");
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
                Some(digest(contents)),
                false,
                contents.len() as u64,
                false,
            )
        };

        let stats = store.publish(vec![file(pkg_a.join("x"), &same)]);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.reused, 0);

        let stats = store.publish(vec![
            file(pkg_b.join("y"), &same),
            file(pkg_b.join("z"), b"only in b"),
        ]);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.reused, 1);
        assert_eq!(stats.bytes_reused, same.len() as u64);

        // Both packages now point at the same object and read the same bytes.
        let object = store.object_path(&digest(&same), false);
        assert!(object.exists());
        assert_eq!(std::fs::read(pkg_a.join("x")).unwrap(), same);
        assert_eq!(std::fs::read(pkg_b.join("y")).unwrap(), same);
        assert!(same_file(&pkg_a.join("x"), &pkg_b.join("y")));

        // A missing source is reported as unshared, not as an error.
        let stats = store.publish(vec![file(pkg_b.join("missing"), b"nope")]);
        assert_eq!(stats.unshared, 1);

        // A file recorded without a digest is hashed from disk before it is
        // shared, and lands on the same object as its in-memory twin.
        std::fs::write(pkg_b.join("w"), &same).unwrap();
        let stats = store.publish(vec![ExtractedFile::new(
            pkg_b.join("w"),
            None,
            false,
            same.len() as u64,
            false,
        )]);
        assert_eq!((stats.added, stats.reused), (0, 1));
        assert!(same_file(&pkg_a.join("x"), &pkg_b.join("w")));
        let stats = store.publish(vec![ExtractedFile::new(
            pkg_b.join("missing"),
            None,
            false,
            0,
            false,
        )]);
        assert_eq!(stats.unshared, 1);
    }

    #[test]
    fn link_existing_only_links_known_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("store"));
        let contents = b"lookup";
        let digest = digest(contents);
        let target = dir.path().join("target");

        assert!(!store.begin().link_existing(&digest, false, &target));
        assert!(!target.exists());

        let object = store.object_path(&digest, false);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, contents).unwrap();
        assert!(store.begin().link_existing(&digest, false, &target));
        assert_eq!(std::fs::read(&target).unwrap(), contents);
        assert!(same_file(&object, &target));
    }

    #[test]
    fn prune_removes_objects_without_packages() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("store"));
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();

        // A store that does not exist yet is empty.
        assert_eq!(store.prune().unwrap(), PruneStats::default());

        let kept = b"still in a package".to_vec();
        let orphan = b"package was deleted".to_vec();
        std::fs::write(pkg.join("kept"), &kept).unwrap();
        std::fs::write(pkg.join("orphan"), &orphan).unwrap();
        let stats = store.publish(vec![
            ExtractedFile::new(
                pkg.join("kept"),
                Some(digest(&kept)),
                false,
                kept.len() as u64,
                false,
            ),
            ExtractedFile::new(
                pkg.join("orphan"),
                Some(digest(&orphan)),
                true,
                orphan.len() as u64,
                false,
            ),
        ]);
        assert_eq!(stats.added, 2);
        std::fs::remove_file(pkg.join("orphan")).unwrap();

        let stats = store.prune().unwrap();
        assert_eq!(
            stats,
            PruneStats {
                kept: 1,
                removed: 1,
                bytes_freed: orphan.len() as u64,
            }
        );
        assert!(store.object_path(&digest(&kept), false).exists());
        assert!(!store.object_path(&digest(&orphan), true).exists());
        assert_eq!(std::fs::read(pkg.join("kept")).unwrap(), kept);

        // Deleting the last package empties the store, shard directories
        // included.
        std::fs::remove_dir_all(&pkg).unwrap();
        let stats = store.prune().unwrap();
        assert_eq!((stats.kept, stats.removed), (0, 1));
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
        assert_eq!(store.prune().unwrap(), PruneStats::default());
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
