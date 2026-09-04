//! A content-addressed store that deduplicates the files of extracted
//! packages.
//!
//! Every regular file of a package is extracted into the package directory as
//! usual. Afterwards each file is hard linked into the store under the BLAKE3
//! hash of its contents and executable bit, so packages that ship identical
//! files share one inode. When a store object already exists, the package's
//! private copy is replaced by a link to it. Small files are hashed and looked
//! up before they are written at all. Larger files are written normally and
//! hashed in parallel from the page cache after extraction.
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
    cmp::Reverse,
    io,
    path::{Path, PathBuf},
};

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
struct KnownShards([u64; 4]);

impl KnownShards {
    fn insert(&mut self, shard: u8) {
        let shard = usize::from(shard);
        self.0[shard / 64] |= 1 << (shard % 64);
    }

    fn contains(&self, digest: &blake3::Hash) -> bool {
        let shard = usize::from(digest.as_bytes()[0]);
        self.0[shard / 64] & (1 << (shard % 64)) != 0
    }
}

/// A regular file that was extracted from a package.
#[derive(Debug, Clone)]
pub struct ExtractedFile {
    /// Absolute path of the file in the package directory.
    path: PathBuf,
    /// BLAKE3 hash of the file contents, or `None` for a file that was
    /// written without hashing and is hashed from disk when it is published.
    digest: Option<blake3::Hash>,
    /// Whether any executable bit is set in the archive entry's mode.
    executable: bool,
    /// Size of the file in bytes.
    size: u64,
}

impl ExtractedFile {
    pub(crate) fn new(
        path: PathBuf,
        digest: Option<blake3::Hash>,
        executable: bool,
        size: u64,
    ) -> Self {
        Self {
            path,
            digest,
            executable,
            size,
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

/// The sharing state of one package extraction: the store objects that can
/// be looked up and the files extracted so far.
#[derive(Debug)]
pub(crate) struct FileStoreSession<'a> {
    store: &'a FileStore,
    shards: KnownShards,
    files: Vec<ExtractedFile>,
    reused: FileStoreStats,
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
    pub fn object_path(&self, digest: &blake3::Hash, executable: bool) -> PathBuf {
        let hash = digest.to_hex();
        let mut path = self.root.join(&hash.as_str()[..2]);
        if executable {
            let mut file_name = String::with_capacity(65);
            file_name.push_str(hash.as_str());
            file_name.push('x');
            path.push(file_name);
        } else {
            path.push(hash.as_str());
        }
        path
    }

    /// Starts sharing the files of one package.
    pub(crate) fn begin(&self) -> FileStoreSession<'_> {
        FileStoreSession {
            store: self,
            shards: self.known_shards(),
            files: Vec::new(),
            reused: FileStoreStats::default(),
        }
    }

    /// Lists the shard directories that currently exist.
    fn known_shards(&self) -> KnownShards {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return KnownShards::default();
        };
        let mut shards = KnownShards::default();
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.len() == 2
                && let Ok(shard) = u8::from_str_radix(name, 16)
            {
                shards.insert(shard);
            }
        }
        shards
    }

    /// Links an existing object to `destination`. Returns `Ok(false)` when
    /// the object does not exist, so the caller writes the file instead.
    /// Objects in shards that did not exist when `shards` was listed are
    /// assumed absent without touching the filesystem.
    fn link_existing(
        &self,
        shards: &KnownShards,
        digest: &blake3::Hash,
        executable: bool,
        destination: &Path,
    ) -> io::Result<bool> {
        if !shards.contains(digest) {
            return Ok(false);
        }
        match std::fs::hard_link(self.object_path(digest, executable), destination) {
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
            .filter(|file| file.digest.is_none())
            .for_each(|file| match hash_file(&file.path) {
                Ok(digest) => file.digest = Some(digest),
                Err(err) => {
                    tracing::debug!("could not hash {}: {err}", file.path.display());
                }
            });

        let mut shards: [Vec<(&ExtractedFile, PathBuf)>; 256] = std::array::from_fn(|_| Vec::new());
        for file in &files {
            let Some(digest) = &file.digest else {
                stats.unshared += 1;
                continue;
            };
            shards[usize::from(digest.as_bytes()[0])]
                .push((file, self.object_path(digest, file.executable)));
        }

        let mut shards = shards
            .into_iter()
            .enumerate()
            .filter(|(_, files)| !files.is_empty())
            .map(|(shard, files)| (shard, files, FileStoreStats::default()))
            .collect::<Vec<_>>();
        shards.sort_unstable_by_key(|(_, files, _)| Reverse(files.len()));

        if should_overlap_directory_creation(&self.root) {
            // ReFS and Unix benefit when serial shard creation overlaps links
            // into shards that are already ready.
            rayon::in_place_scope(|scope| {
                for (shard, files, shard_stats) in &mut shards {
                    let shard = self.root.join(format!("{shard:02x}"));
                    match std::fs::create_dir_all(&shard) {
                        Ok(()) => {
                            scope.spawn(move |_| {
                                for (file, object) in files {
                                    publish_file(&file.path, object, file.size, shard_stats);
                                }
                            });
                        }
                        Err(err) => {
                            tracing::debug!("could not create {}: {err}", shard.display());
                            shard_stats.unshared += files.len();
                        }
                    }
                }
            });
        } else {
            // NTFS performs better when root-directory mutations finish
            // before links start.
            for (shard, files, shard_stats) in &mut shards {
                let shard = self.root.join(format!("{shard:02x}"));
                if let Err(err) = std::fs::create_dir_all(&shard) {
                    tracing::debug!("could not create {}: {err}", shard.display());
                    shard_stats.unshared += files.len();
                    files.clear();
                }
            }
            shards.par_iter_mut().for_each(|(_, files, shard_stats)| {
                for (file, object) in files {
                    publish_file(&file.path, object, file.size, shard_stats);
                }
            });
        }

        let linked = shards
            .into_iter()
            .fold(FileStoreStats::default(), |a, (_, _, b)| FileStoreStats {
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
        digest: &blake3::Hash,
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

    /// Records an extracted regular file for publishing.
    pub(crate) fn record(&mut self, file: ExtractedFile) {
        self.files.push(file);
    }

    /// Records a file linked directly from an existing store object.
    pub(crate) fn record_reused(&mut self, size: u64) {
        self.reused.files += 1;
        self.reused.reused += 1;
        self.reused.bytes_reused += size;
    }

    /// Shares the recorded files through the store and reports what that
    /// did. Never fails: a file that cannot be shared stays a private copy.
    pub(crate) fn publish(self) -> FileStoreStats {
        let mut stats = self.store.publish(self.files);
        stats.files += self.reused.files;
        stats.reused += self.reused.reused;
        stats.bytes_reused += self.reused.bytes_reused;
        stats
    }
}

/// Computes the BLAKE3 hash of a file through a wide read buffer.
fn hash_file(path: &Path) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize())
}

/// Whether serial directory creation should overlap file linking on this filesystem.
#[doc(hidden)]
#[cfg(not(windows))]
pub fn should_overlap_directory_creation(_root: &Path) -> bool {
    true
}

/// `ReFS` benefits from overlapping directory creation and file linking, while
/// `NTFS` performs better with a directory-creation barrier.
#[doc(hidden)]
#[cfg(windows)]
pub fn should_overlap_directory_creation(root: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetThreadErrorMode, SEM_FAILCRITICALERRORS, SetThreadErrorMode,
    };

    struct ErrorModeGuard(u32);

    impl ErrorModeGuard {
        fn suppress_critical_errors() -> Option<Self> {
            // SAFETY: these functions access error-mode state belonging to
            // the current thread. The previous mode is saved by value.
            let previous = unsafe { GetThreadErrorMode() };
            // SAFETY: the null output pointer is explicitly supported.
            let success = unsafe {
                SetThreadErrorMode(previous | SEM_FAILCRITICALERRORS, std::ptr::null_mut())
            };
            (success != 0).then_some(Self(previous))
        }
    }

    impl Drop for ErrorModeGuard {
        fn drop(&mut self) {
            // SAFETY: this restores the value read from this thread before
            // the guard was created. The null output pointer is supported.
            let _ = unsafe { SetThreadErrorMode(self.0, std::ptr::null_mut()) };
        }
    }

    let Ok(absolute) = std::path::absolute(root) else {
        return false;
    };
    let Some(_error_mode) = ErrorModeGuard::suppress_critical_errors() else {
        return false;
    };
    let Some(existing) = absolute.ancestors().find(|path| path.exists()) else {
        return false;
    };
    let existing = existing
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut volume_root = [0u16; 260];
    // SAFETY: both buffers remain valid for the duration of the call. The
    // input is null-terminated and the writable buffer length is exact.
    let success = unsafe {
        GetVolumePathNameW(
            existing.as_ptr(),
            volume_root.as_mut_ptr(),
            volume_root.len() as u32,
        )
    };
    if success == 0 {
        return false;
    }

    let mut filesystem_name = [0u16; 16];
    // SAFETY: the volume-root buffer was initialized and null-terminated by
    // GetVolumePathNameW. The optional output pointers are null, and the
    // writable filesystem-name buffer length matches the value passed.
    let success = unsafe {
        GetVolumeInformationW(
            volume_root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem_name.as_mut_ptr(),
            16,
        )
    };
    success != 0 && filesystem_name.starts_with(&[0x52, 0x65, 0x46, 0x53, 0])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(contents: &[u8]) -> blake3::Hash {
        blake3::hash(contents)
    }

    #[test]
    fn executable_bit_is_part_of_the_key() {
        let store = FileStore::new("store");
        let digest = digest(b"same bytes");
        let hash = digest.to_hex();
        let regular = store.object_path(&digest, false);
        let executable = store.object_path(&digest, true);

        assert_eq!(
            regular,
            Path::new("store")
                .join(&hash.as_str()[..2])
                .join(hash.as_str())
        );
        assert_eq!(
            executable,
            Path::new("store")
                .join(&hash.as_str()[..2])
                .join(format!("{hash}x"))
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
            ExtractedFile::new(path, Some(digest(contents)), false, contents.len() as u64)
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
        )]);
        assert_eq!((stats.added, stats.reused), (0, 1));
        assert!(same_file(&pkg_a.join("x"), &pkg_b.join("w")));
        let stats = store.publish(vec![ExtractedFile::new(
            pkg_b.join("missing"),
            None,
            false,
            0,
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
            ),
            ExtractedFile::new(
                pkg.join("orphan"),
                Some(digest(&orphan)),
                true,
                orphan.len() as u64,
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
