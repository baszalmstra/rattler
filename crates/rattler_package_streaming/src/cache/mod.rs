//! This module provides structs and functions to efficiently extract conda package archives to a
//! cache, and retrieve files from it.

use crate::provenance::{Hash, ProvenanceIntegrity};
use cacache::WriteOpts;
use rattler_conda_types::package::ArchiveType;
use rattler_digest::{
    digest::{Digest, Output},
    HashingReader, Md5, Sha256,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsStr,
    fmt::LowerHex,
    io::{self, BufReader, Error, Read},
    path::{Component, Path, PathBuf},
};

mod error;
#[cfg(feature = "tokio")]
mod streaming_or_local;

#[cfg(feature = "tokio")]
pub use streaming_or_local::StreamingOrLocal;

pub use error::ExtractError;

/// Represents the data of a package archive.
///
/// This data can be read and extracted to a cache directory using the
/// [`RawArchive::extract_to_cache`] and [`RawArchive::extract_to_cache_sync`] functions.
///
/// The archive is accompanied by a [`ProvenanceIntegrity`] which is used to determine the archive
/// integrity. If the archive has no associated integrity the archive's individual files will be
/// stored in the cache but the archive index itself will not be stored in the cache.
///
/// Since reading, decompressing and extracting at the same time can cause some significant
/// back-pressure this struct expects to be able to quickly read new data from the input stream.
/// Using an async stream would most likely not yield any benefit in terms of asynchronicity. It is
/// likely more performant to first read the contents of an async stream into memory (or to disk)
/// followed by extracting the contents while concurrently downloading additional archives. This
/// will be faster because it will saturate the network-io, CPU, and disk-io instead of just one of
/// those being the bottleneck. See [`StreamingOrLocal`] to help with that.
pub struct RawArchive {
    data: Box<dyn Read + Send>,
    archive_type: ArchiveType,
    integrity: ProvenanceIntegrity,
}

impl RawArchive {
    /// Construct a new [`RawArchive`] from the data and the type of archive.
    pub fn new(
        data: Box<dyn Read + Send>,
        archive_type: ArchiveType,
        integrity: ProvenanceIntegrity,
    ) -> Self {
        Self {
            data,
            archive_type,
            integrity,
        }
    }

    /// Extract the contents of the archive into a cache.
    ///
    /// If the archive has an associated integrity the integrity will be checked after the archive
    /// has been extracted. It will also write the returned [`ArchiveIndex`] to the cache alongside
    /// the individual files. This alllows the archive to be retrieved from the cache later using
    /// only the integrity.
    ///
    /// Extraction of the archive will happen in a background task which can be awaited. The task
    /// itself is non-cancellable, so dropping the future returned by this function will not cancel
    /// the extraction.
    ///
    /// This function consumes the instance as it will consume the data.
    ///
    /// The function returns an [`ArchiveIndex`] which enables retrieving the contents of the
    /// archive back from the cache.
    ///
    /// For an synchronous version of this function see [`Self::extract_to_cache_sync`].
    #[cfg(feature = "tokio")]
    pub async fn extract_to_cache(self, cache_path: &Path) -> Result<ArchiveIndex, ExtractError> {
        let cache_path = cache_path.to_path_buf();
        match tokio::task::spawn_blocking(move || self.extract_to_cache_sync(&cache_path))
            .await
            .map_err(tokio::task::JoinError::try_into_panic)
        {
            Ok(result) => result,
            Err(Ok(panic)) => std::panic::resume_unwind(panic),
            Err(_) => Err(ExtractError::Cancelled),
        }
    }

    /// Extract the contents of the archive into a cache.
    ///
    /// If the archive has an associated integrity the integrity will be checked after the archive
    /// has been extracted. It will also write the returned [`ArchiveIndex`] to the cache alongside
    /// the individual files. This alllows the archive to be retrieved from the cache later using
    /// only the integrity.
    ///
    /// This function consumes the instance as it will consume the data.
    ///
    /// The function returns an [`ArchiveIndex`] which enables retrieving the contents of the
    /// archive back from the cache.
    ///
    /// For an asynchronous version of this function see [`Self::extract_to_cache`].
    pub fn extract_to_cache_sync(self, cache_path: &Path) -> Result<ArchiveIndex, ExtractError> {
        // A helper function to write the archive index to the cache without checking the integrity
        // of the archive.
        fn extract_unchecked<R: Read>(
            data: R,
            archive_type: ArchiveType,
            cache_path: &Path,
        ) -> Result<ArchiveIndex, ExtractError> {
            Ok(match archive_type {
                ArchiveType::TarBz2 => extract_tar_bz2_to_cache(data, cache_path)?,
                ArchiveType::Conda => extract_conda_to_cache(data, cache_path)?,
            })
        }

        // A helper function to write the archive index to the cache and check the integrity of the
        // archive at the same time.
        fn extract_checked<R: Read, D: Digest + Default>(
            data: R,
            archive_type: ArchiveType,
            cache_path: &Path,
            expected_hash: &Output<D>,
        ) -> Result<ArchiveIndex, ExtractError>
        where
            Output<D>: LowerHex,
        {
            // Construct a hashing reader and extract using that reader
            let mut reader = HashingReader::<R, D>::new(data);
            let index = extract_unchecked(&mut reader, archive_type, cache_path)?;

            // Drain the rest of the bytes so we can compute the integrity of the archive. We have
            // to drain bytes because there might be so unread bytes at the end of the archive.
            let mut drain_buf = [0u8; 8 * 1024];
            loop {
                let bytes_read = reader.read(&mut drain_buf).map_err(|e| {
                    ExtractError::IoError(e, None, "flushing the rest of the archive".into())
                })?;
                if bytes_read == 0 {
                    break;
                }
            }

            // Check if the resulting hash is the same as the expected hash.
            let (_, actual_hash) = reader.finalize();
            if &actual_hash != expected_hash {
                return Err(ExtractError::IntegrityMismatch(
                    format!("{actual_hash:x}"),
                    format!("{expected_hash:x}"),
                ));
            }

            Ok(index)
        }

        // Determine the best hash that is associated with the integrity. It is also possible that
        // the archive has no associated integrity in which case we just extract the archive to the
        // cache but we dont really insert a cache entry for the archive itself.
        let best_hash = self.integrity.get_best_hash();
        let archive_index = match best_hash {
            None => return extract_unchecked(self.data, self.archive_type, cache_path),
            Some(Hash::Sha256(hash)) => {
                extract_checked::<_, Sha256>(self.data, self.archive_type, cache_path, hash)?
            }
            Some(Hash::Md5(hash)) => {
                extract_checked::<_, Md5>(self.data, self.archive_type, cache_path, hash)?
            }
        };

        // Write the archive index to the cache using the provenance
        archive_index.write_to_cache(cache_path, &self.integrity)?;

        Ok(archive_index)
    }
}

/// Extracts a conda archive to a cache directory and returns an [`ArchiveIndex`] to be able to
/// find the entries from the archive in the cache.
fn extract_conda_to_cache<'r, R: Read + 'r>(
    mut data: R,
    cache_path: &Path,
) -> Result<ArchiveIndex, ExtractError> {
    let mut index = ArchiveIndex::default();
    while let Some(entry) = zip::read::read_zipfile_from_stream(&mut data)
        .map_err(|err| ExtractError::zip_error(err, None))?
    {
        // Determine the filename of the zip entry
        let manged_named = entry.mangled_name();
        let file_name = manged_named
            .file_name()
            .map(OsStr::to_string_lossy)
            .ok_or_else(|| {
                ExtractError::IoError(
                    io::Error::new(
                        io::ErrorKind::Other,
                        "file name is missing from zip archive",
                    ),
                    None,
                    "while reading conda archive".into(),
                )
            })?;

        // If this is a data file, extract it to the cache.
        if file_name.ends_with(".tar.zst") {
            // Extract the internal tarball to the cache
            let index_part = extract_tar_zst_to_cache(entry, cache_path, Some(manged_named))?;

            // Merge the archive index with the rest of the data
            index.append(index_part);
        }
    }

    Ok(index)
}

/// Extracts a zstd compressed tar archive to a cache directory and returns an [`ArchiveIndex`] to
/// be able to read the extracted content back from the cache.
fn extract_tar_zst_to_cache<'r, R: Read + 'r>(
    data: R,
    cache_path: &Path,
    archive_path: Option<PathBuf>,
) -> Result<ArchiveIndex, ExtractError> {
    let decompressed_tar = zstd::stream::read::Decoder::new(data)
        .map_err(|e| ExtractError::IoError(e, archive_path, "while reading zstd stream".into()))?;
    extract_tar_to_cache(decompressed_tar, cache_path)
}

/// Extracts an bz2 compressed tar archive to a cache directory and returns an [`ArchiveIndex`] to
/// be able to read the extracted content back from the cache.
fn extract_tar_bz2_to_cache<'r, R: Read + 'r>(
    data: R,
    cache_path: &Path,
) -> Result<ArchiveIndex, ExtractError> {
    let decompressed_tar = bzip2::read::BzDecoder::new(BufReader::new(data));
    extract_tar_to_cache(decompressed_tar, cache_path)
}

/// Extracts an archive to a cache directory and returns an [`ArchiveIndex`] to be able to read the
/// extracted content back from the cache.
fn extract_tar_to_cache<'r, R: Read + 'r>(
    data: R,
    cache_path: &Path,
) -> Result<ArchiveIndex, ExtractError> {
    let mut index = ArchiveIndex::default();
    let mut archive = tar::Archive::new(data);
    let entries = archive.entries().map_err(|err| {
        ExtractError::IoError(err, None, "reading path from entry header.".into())
    })?;
    let mut drain_buffer = [0u8; 1024 * 8];

    for entry in entries {
        let mut entry = entry
            .map_err(|e| ExtractError::IoError(e, None, "reading entry from tarball".into()))?;
        let header = entry.header();
        let mode = header.mode().unwrap_or(0o644) | 0o600;
        let entry_type = header.entry_type();

        // Skip invalid paths
        let entry_path = header.path().map_err(|e| {
            ExtractError::IoError(e, None, "reading path from entry header.".into())
        })?;
        let Some(entry_path) = strip_prefix(&entry_path) else { continue };

        match entry_type {
            tar::EntryType::Regular => {
                // Open a writer to write a file to cache
                let mut writer = WriteOpts::new()
                    .algorithm(cacache::Algorithm::Xxh3)
                    .open_hash_sync(cache_path)
                    .map_err(|e| ExtractError::CacheError(e, Some(entry_path.to_path_buf())))?;

                // Copy the content from the tarball directly into the cache.
                std::io::copy(&mut entry, &mut writer).map_err(|e| {
                    ExtractError::IoError(
                        e,
                        Some(entry_path.to_path_buf()),
                        "copying to cacache".into(),
                    )
                })?;

                // Finish writing the file to the cache and constructing a hash
                let sri = writer
                    .commit()
                    .map_err(|e| ExtractError::CacheError(e, Some(entry_path.to_path_buf())))?;

                // Store a record in the index so we can retrieve the file later.
                index.files.insert(
                    entry_path.to_string_lossy().replace('\\', "/"),
                    (sri.to_string(), mode),
                );
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                // Read the link name from archive
                let link_name = read_link_name(&mut entry).map_err(|e| {
                    ExtractError::IoError(
                        e,
                        Some(entry_path.to_path_buf()),
                        "while reading link".into(),
                    )
                })?;

                // Make sure the link doesnt point outside of the archive.
                if is_target_outside_of_path(&entry_path, &link_name) {
                    return Err(ExtractError::IoError(
                        io::Error::new(
                            io::ErrorKind::Other,
                            "link destination is outside of the archive",
                        ),
                        Some(entry_path.to_path_buf()),
                        "while reading link".into(),
                    ));
                }

                // Store the record in the index so we can create it later.
                index.links.insert(
                    entry_path.to_string_lossy().replace('\\', "/"),
                    (
                        link_name.to_string_lossy().to_string(),
                        if entry_type.is_hard_link() {
                            LinkType::Hard
                        } else {
                            LinkType::Soft
                        },
                    ),
                );
            }
            // Otherwise skip the entry by reading its content.
            _ => loop {
                let bytes_read = entry.read(&mut drain_buffer).map_err(|e| {
                    ExtractError::IoError(e, Some(entry_path.to_path_buf()), "reading entry".into())
                })?;
                if bytes_read == 0 {
                    break;
                }
            },
        }
    }

    Ok(index)
}

/// Reads a link name from a tar entry and produces a sensible error message if the name is missing
/// or invalid.
fn read_link_name<'e, 'r, R: Read + 'r>(
    entry: &'e mut tar::Entry<R>,
) -> Result<Cow<'e, Path>, Error> {
    match entry.link_name() {
        Ok(Some(link_name)) if link_name.iter().next().is_some() => Ok(link_name),
        Ok(Some(_)) => Err(io::Error::new(
            io::ErrorKind::Other,
            "link destination is empty",
        )),
        Ok(None) => Err(io::Error::new(
            io::ErrorKind::Other,
            "link destination is missing",
        )),
        Err(err) => Err(err),
    }
}

/// Ensure that the specified path is a valid path in an archive.
fn strip_prefix(path: &Path) -> Option<PathBuf> {
    let mut dest = PathBuf::new();
    for part in path.components() {
        match part {
            // Leading '/' characters, root paths, and '.'
            // components are just ignored and treated as "empty
            // components"
            Component::Prefix(..) | Component::RootDir | Component::CurDir => continue,

            // If any part of the filename is '..', then skip over
            // unpacking the file to prevent directory traversal
            // security issues.  See, e.g.: CVE-2001-1267,
            // CVE-2002-0399, CVE-2005-1918, CVE-2007-4131
            Component::ParentDir => return None,

            Component::Normal(part) => dest.push(part),
        }
    }
    Some(dest)
}

/// Checks whether a given `target` path is located outside of the `path`.
///
/// This function determines if the `target` path is located outside of the `path` by iterating
/// through the components of the `target` path and comparing them with the `path`. If any
/// component of the `target` path references an absolute path, a root directory, or goes
/// above the parent directory of the `path`, the function returns `true`. Otherwise, it returns
/// `false`.
fn is_target_outside_of_path(path: &Path, target: &Path) -> bool {
    if path == target {
        return false;
    }

    let mut current = path.to_path_buf();

    for target_components in target.components() {
        match target_components {
            Component::CurDir => continue, // Skip current directory component
            Component::ParentDir => match current.parent() {
                Some(parent) => current = parent.to_path_buf(),
                None => return true, // Target tries to go above the parent directory, so it's outside
            },
            Component::Normal(path) => current.push(path),
            c @ Component::Prefix(prefix) => match current.components().next() {
                Some(Component::Prefix(p)) if p == prefix => {
                    current = AsRef::<Path>::as_ref(&c).to_path_buf()
                }
                _ => return true,
            },
            c @ Component::RootDir => match current.components().next() {
                Some(Component::RootDir) => current = AsRef::<Path>::as_ref(&c).to_path_buf(),
                _ => return true,
            },
        }
    }

    false
}

/// Represents the result of extracting an archive to a cache.
///
/// This struct records for each file in the archive the hash of the file and some additional
/// metadata (like file permissions). This can then be used to retrieve the file from the cache and
/// extract it to a destination folder.
///
/// An [`ArchiveIndex`] can be created by extracting a [`RawArchive`] using the
/// [`RawArchive::extract_to_cache`] or [`RawArchive::extract_to_cache_sync`] functions.
#[derive(rkyv::Archive, rkyv::Serialize, Default)]
#[cfg_attr(test, derive(serde::Serialize))]
#[archive(check_bytes)]
pub struct ArchiveIndex {
    /// A map of file names to the hash of the file and some file permissions.
    pub files: HashMap<String, (String, u32)>,

    /// A map of fileystem links to the target of the link and the type of link.
    pub links: HashMap<String, (String, LinkType)>,
}

/// Describes a type of filesystem link.
#[derive(rkyv::Archive, rkyv::Serialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub enum LinkType {
    /// A hardlink or junction
    Hard,

    /// A soft or symbolic link
    Soft,
}

impl ArchiveIndex {
    /// Appends the entries from another index to this index.
    pub(crate) fn append(&mut self, other: Self) {
        self.files.extend(other.files.into_iter());
        self.links.extend(other.links.into_iter());
    }

    /// Write the archive index to a cache directory.
    pub fn write_to_cache(
        &self,
        cache_path: &Path,
        provenance_integrity: &ProvenanceIntegrity,
    ) -> Result<(), ExtractError> {
        cacache::index::insert(
            cache_path,
            &archive_cache_key(provenance_integrity),
            WriteOpts::new()
                // This is just so the index entry is loadable.
                .integrity("xxh3-deadbeef".parse().unwrap())
                .raw_metadata(
                    rkyv::util::to_bytes::<_, 1024>(self)
                        .map_err(|e| ExtractError::SerializeCacheError(format!("{e}")))?
                        .into_vec(),
                ),
        )
        .map_err(|e| ExtractError::CacheError(e, None))?;

        Ok(())
    }
}

/// Returns a cache key for the specified provenance based on the integrity of the archive.
pub fn archive_cache_key(integrity: &ProvenanceIntegrity) -> String {
    format!("rattler::package::{integrity}")
}

#[cfg(test)]
mod test {
    use super::*;
    use rattler_conda_types::package::ArchiveIdentifier;
    use rstest::*;
    use std::fs::File;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[rstest]
    #[case("", "a", true)]
    #[case("", "..", false)]
    #[case("a", "..", true)]
    #[case("a", "../..", false)]
    #[case("", "/", false)]
    #[case("/", "/", true)]
    #[case("/", "/..", false)]
    #[case("/a/b/c", "/a/b/c", true)]
    #[case("/a/b/c", "/a/b/c/d", true)]
    #[case("/a/b/c/d", "/a/b/c", true)]
    #[case("/", "/a", true)]
    #[case("/a", "/", true)]
    #[case("/a/b", "/a/b/c/../d", true)]
    #[case("/a/b", "/a/b/../c/../d", true)]
    #[case("/a/b", "/a/b/c/../../d", true)]
    #[case("/a/b/c", "/a/b/../x/y/z", true)]
    #[case("/a/b/c", "x/y/z", true)]
    #[case("/a/b/c", "../../a/b/c", true)]
    #[case("", "a/b", true)]
    #[case("a/b", "", true)]
    fn test_is_target_outside_of_path(
        #[case] path: PathBuf,
        #[case] target: PathBuf,
        #[case] inside: bool,
    ) {
        assert_eq!(
            is_target_outside_of_path(&path, &target),
            !inside,
            "'{}' should {}be rooted in '{}'",
            target.display(),
            if inside { "" } else { "NOT " },
            path.display(),
        );
    }

    #[rstest]
    #[case::mock_tar_bz("mock-2.0.0-py37_1000.tar.bz2", "md5-0f9cce120a73803a70abb14bd4d4900b")]
    #[case::mock_conda(
        "mock-2.0.0-py37_1000.conda",
        "md5-23c226430e35a3bd994db6c36b9ac8ae,sha256-181ec44eb7b06ebb833eae845bcc466ad96474be1f33ee55cab7ac1b0fdbbfa3"
    )]
    #[case::mock_libzlib_symlink(
        "with-symlinks/libzlib-1.2.13-hfd90126_4.tar.bz2",
        "sha256-0d954350222cc12666a1f4852dbc9bcf4904d8e467d29505f2b04ded6518f890"
    )]
    fn test_extract_archive_to_cache(#[case] archive_name: &str, #[case] integrity: &str) {
        let cache_dir = tempdir().unwrap();

        let archive_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data")
            .join(archive_name);

        let integrity = ProvenanceIntegrity::from_str(integrity).unwrap();
        let identifier = ArchiveIdentifier::try_from_path(&archive_path).unwrap();

        let file = File::open(archive_path).unwrap();

        let index = RawArchive::new(Box::new(file), identifier.archive_type, integrity)
            .extract_to_cache_sync(cache_dir.path())
            .unwrap();

        insta::with_settings!({sort_maps => true}, {
            insta::assert_yaml_snapshot!(archive_name, index);
        });
    }
}
