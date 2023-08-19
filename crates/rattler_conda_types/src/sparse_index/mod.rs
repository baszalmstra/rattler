//! Contains code regarding the Sparse Index, which is a different way of handling the retrieval
//! of records from the index.
use crate::{PackageRecord, RepoData};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use fxhash::{FxHashMap, FxHashSet};
use itertools::Itertools;
use rattler_digest::{HashingWriter, Sha256, Sha256Hash};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::File;
use std::io::{BufRead, Cursor, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use zstd::Encoder;

static NAMES_FILE_MAGIC: &[u8; 4] = b"NAME";
static NAMES_FILE_VERSION: u16 = 1;

static DEPENDENCIES_FILE_MAGIC: &[u8; 4] = b"DEPS";
static DEPENDENCIES_FILE_VERSION: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
/// Record in a sparse index
/// contains the package record and a filename
pub struct SparseIndexRecord {
    /// Filename to address the package with
    pub file_name: String,

    /// Actual data regarding the package
    #[serde(flatten)]
    pub package_record: PackageRecord,
}

impl SparseIndexRecord {
    /// Create a [`SparseIndexRecord`] from a filename and a [`PackageRecord`]
    pub fn from_record(package_record: PackageRecord, filename: String) -> SparseIndexRecord {
        SparseIndexRecord {
            package_record,
            file_name: filename,
        }
    }

    /// Converts to json
    pub fn json(&self) -> serde_json::Result<String> {
        serde_json::to_string(&self)
    }
}

#[derive(Debug)]
/// A single package in the sparse index
pub struct SparseIndexPackage {
    /// Sparse index records
    pub records: Vec<SparseIndexRecord>,
}

impl SparseIndexPackage {
    /// Write a sparse index package to a file and return its sha256 hash.
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<Sha256Hash, WriteSparseIndexError> {
        let hashing_buf_writer =
            HashingWriter::<_, Sha256>::new(std::io::BufWriter::new(writer));
        let mut encoder = Encoder::new(hashing_buf_writer, 19).unwrap();
        for record in self.records.iter() {
            writeln!(encoder, "{}", serde_json::to_string(record)?)?;
        }
        let hashing_buf_writer = encoder.finish().unwrap();
        let (mut buf_writer, hash) = hashing_buf_writer.finalize();
        buf_writer.flush()?;
        Ok(hash)
    }
}

/// The entire sparse index
pub struct SparseIndex {
    /// Package name to sparse index package
    pub packages: FxHashMap<String, SparseIndexPackage>,
}

#[allow(missing_docs)]
#[derive(Error, Debug)]
/// Error when trying to create a sparse index filename
pub enum SparseIndexFilenameError {
    #[error("Empty filename")]
    EmptyFilename,

    #[error("Filename does not contain a package name")]
    NoFileNameComponent,

    #[error("Filename is not valid UTF-8")]
    Utf8Error,
}

/// Create a final path for a sparse index file with the following rules
/// 1. For a package with one letter use 1/<filename>
/// 2. For a package with two letters use 2/<filename>
/// 3. For a package with three letters use 3/<first_two_letters>/<filename>
/// 4. For a package with more letters use <first_two_letters>/<second_two_letters>/<filename>
pub fn sparse_index_filename(package_name: &str) -> Result<PathBuf, SparseIndexFilenameError> {
    let mut new_path = PathBuf::new();

    // Create path according to rules in docs
    match package_name.len() {
        0 => {
            return Err(SparseIndexFilenameError::EmptyFilename);
        }
        // Will yield something like 1/a
        1 => {
            new_path.push("1");
            new_path.push(format!("{package_name}.json.zst"));
        }
        // Will yield something like 2/ab
        2 => {
            new_path.push("2");
            new_path.push(format!("{package_name}.json.zst"));
        }

        // Will yield something like 3/ab/abc
        3 => {
            new_path.push("3");
            new_path.push(&package_name[0..1]);
            new_path.push(format!("{package_name}.json.zst"));
        }
        // Will yield something like py/th/python
        _ => {
            new_path.push(&package_name[0..2]);
            new_path.push(&package_name[2..4]);
            new_path.push(format!("{package_name}.json.zst"));
        }
    }

    Ok(new_path)
}

#[allow(missing_docs)]
/// Error when trying to write a sparse index
#[derive(Error, Debug)]
pub enum WriteSparseIndexError {
    #[error(transparent)]
    FileNameError(#[from] SparseIndexFilenameError),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error(transparent)]
    SerdeError(#[from] serde_json::Error),
}

impl SparseIndex {
    /// Write entire index to local path on filesystem
    /// directories are created if they do not exist yet
    pub fn write_index_to(&self, path: &Path) -> Result<SparseIndexNames, WriteSparseIndexError> {
        let mut names = SparseIndexNames {
            names: Default::default(),
        };

        let mut dependencies = SparseIndexDependencies {
            dependencies: Default::default(),
        };

        // Write each individual package
        for (package, sparse_index_package) in self.packages.iter() {
            // Create the directory for the package
            let package_path = path.join(sparse_index_filename(package)?);
            std::fs::create_dir_all(package_path.parent().unwrap())?;

            // Write the file
            let file = std::fs::File::create(package_path)?;
            let mut writer = std::io::BufWriter::new(file);
            let hash = sparse_index_package.write(&mut writer)?;

            // Determine the dependencies of the package
            let deps = sparse_index_package
                .records
                .iter()
                .flat_map(|record| {
                    record.package_record.depends.iter().map(|dependency| {
                        dependency
                            .split_once(' ')
                            .unwrap_or((dependency, ""))
                            .0
                            .to_owned()
                    })
                })
                .sorted()
                .dedup()
                .collect();

            dependencies.dependencies.insert(package.clone(), deps);

            // Store in `names`
            names.insert(package.clone(), hash);
        }

        // Write the names to the index as well
        names.to_file(&path.join("names"))?;
        dependencies.to_file(&path.join("dependencies"))?;

        Ok(names)
    }
}

impl From<RepoData> for SparseIndex {
    fn from(value: RepoData) -> Self {
        let packages = value
            .packages
            .into_iter()
            .chain(value.conda_packages.into_iter())
            .map(|(filename, record)| SparseIndexRecord::from_record(record, filename))
            .into_group_map_by(|record| record.package_record.name.clone())
            .into_iter()
            .map(|(name, records)| (name, SparseIndexPackage { records }))
            .collect();

        SparseIndex { packages }
    }
}

impl From<&RepoData> for SparseIndex {
    fn from(value: &RepoData) -> Self {
        let packages = value
            .packages
            .iter()
            .chain(value.conda_packages.iter())
            .map(|(filename, record)| {
                SparseIndexRecord::from_record(record.clone(), filename.clone())
            })
            .into_group_map_by(|record| record.package_record.name.clone())
            .into_iter()
            .map(|(name, records)| (name, SparseIndexPackage { records }))
            .collect();

        SparseIndex { packages }
    }
}

/// Holds the information stored in the `names.json` file located at the root of a subdirectory of
/// a sparse index. It contains the names of all the packages stored in the specific subdirectory
/// of the index as well as the SHA256 hash of the [`SparseIndexPackage`] file associated with the
/// name. This allows a client to cache packages with a very high degree of certainty.
pub struct SparseIndexNames {
    /// A mapping from package name to the first 8 bytes of a sha256 hash of the associated
    /// [`SparseIndexPackage`] file.
    pub names: FxHashMap<String, [u8; 8]>,
}

impl SparseIndexNames {
    /// Adds a package and its hash to this instance
    pub fn insert(&mut self, name: String, hash: Sha256Hash) {
        self.names.insert(name, (&hash[0..8]).try_into().unwrap());
    }
}

impl SparseIndexNames {
    /// Writes the contents of this instance to a writer.
    pub fn to_writer(&self, mut writer: impl Write) -> std::io::Result<()> {
        // Write a magic and version
        writer.write_all(NAMES_FILE_MAGIC)?;
        writer.write_u16::<LittleEndian>(NAMES_FILE_VERSION)?;

        // Write all entries
        for (name, hash) in self.names.iter() {
            let c_name =
                CString::new(name.as_str()).expect("package name should not contain a null");
            writer.write_all(c_name.as_bytes_with_nul())?;
            writer.write_all(hash)?;
        }

        writer.flush()
    }

    /// Writes the contents of this instance to a file.
    pub fn to_file(&self, path: &Path) -> std::io::Result<()> {
        let writer = std::io::BufWriter::new(File::create(path)?);
        self.to_writer(writer)
    }

    /// Parse the file from an async reader
    pub fn from_bytes(slice: &[u8]) -> std::io::Result<Self> {
        let mut reader = Cursor::new(slice);

        // Verify the magic of the file
        let mut magic = *NAMES_FILE_MAGIC;
        reader.read_exact(magic.as_mut())?;
        if &magic != NAMES_FILE_MAGIC {
            return Err(std::io::Error::from(ErrorKind::InvalidData));
        }

        // Determine the version of the file
        let version = reader.read_u16::<LittleEndian>()?;
        if version != 1 {
            return Err(std::io::Error::from(ErrorKind::InvalidData));
        }

        // Read the individual entries
        let mut names = FxHashMap::default();
        loop {
            // Read the next package name from the file
            let mut name_bytes = Vec::new();
            let name_len = reader.read_until(b'\0', &mut name_bytes)?;
            if name_len == 0 {
                break;
            }

            // Safe because we read up until the first nul terminating byte
            let name = unsafe { CString::from_vec_with_nul_unchecked(name_bytes) }
                .into_string()
                .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;

            // Read the hash of the file from the package
            let mut hash_bytes: [u8; 8] = [0; 8];
            reader.read_exact(&mut hash_bytes)?;

            names.insert(name.to_owned(), hash_bytes);
        }

        Ok(Self { names })
    }
}

/// Contains a mapping of package records to all dependencies.
pub struct SparseIndexDependencies {
    /// For each package all dependencies referenced by its records.
    pub dependencies: FxHashMap<String, FxHashSet<String>>,
}

impl SparseIndexDependencies {
    /// Writes the contents of this instance to a file.
    pub fn to_file(&self, path: &Path) -> std::io::Result<()> {
        let mut writer = std::io::BufWriter::new(File::create(path)?);

        // Write a magic and version
        writer.write_all(DEPENDENCIES_FILE_MAGIC)?;
        writer.write_u16::<LittleEndian>(DEPENDENCIES_FILE_VERSION)?;

        // Write all entries
        for (name, deps) in self.dependencies.iter() {
            let c_name =
                CString::new(name.as_str()).expect("package name should not contain a null");
            writer.write_all(c_name.as_bytes_with_nul())?;

            // Write all dependencies
            for dep in deps.iter() {
                let c_name =
                    CString::new(dep.as_str()).expect("depdency name should not contain a null");
                writer.write_all(c_name.as_bytes_with_nul())?;
            }

            // Write a null terminator to indicate the end of the dependencies
            writer.write_u8(b'\0')?;
        }

        writer.flush()
    }

    /// Parses an instance from bytes.
    pub fn from_bytes(slice: &[u8]) -> std::io::Result<Self> {
        let mut reader = Cursor::new(slice);

        // Verify the magic of the file
        let mut magic = *DEPENDENCIES_FILE_MAGIC;
        reader.read_exact(magic.as_mut())?;
        if &magic != DEPENDENCIES_FILE_MAGIC {
            return Err(std::io::Error::from(ErrorKind::InvalidData));
        }

        // Determine the version of the file
        let version = reader.read_u16::<LittleEndian>()?;
        if version != 1 {
            return Err(std::io::Error::from(ErrorKind::InvalidData));
        }

        // Read the individual entries
        let mut dependencies = FxHashMap::default();
        loop {
            // Read the next package name from the file
            let mut name_bytes = Vec::new();
            let name_len = reader.read_until(b'\0', &mut name_bytes)?;
            if name_len == 0 {
                break;
            }

            // Safe because we read up until the first nul terminating byte
            let name = unsafe { CString::from_vec_with_nul_unchecked(name_bytes) }
                .into_string()
                .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;

            let mut package_deps = FxHashSet::default();
            loop {
                // Read the next package name from the file
                let mut dep_name_bytes = Vec::new();
                let dep_name_len = reader.read_until(b'\0', &mut dep_name_bytes)?;
                if dep_name_len == 1 {
                    // If we only read a single byte it means we read an empty string
                    break;
                } else if dep_name_len == 0 {
                    // Premature EOF
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }

                // Safe because we read up until the first nul terminating byte
                let dep_name = unsafe { CString::from_vec_with_nul_unchecked(dep_name_bytes) }
                    .into_string()
                    .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;

                package_deps.insert(dep_name);
            }

            dependencies.insert(name.to_owned(), package_deps);
        }

        Ok(Self { dependencies })
    }
}

#[cfg(test)]
mod tests {
    use super::sparse_index_filename;
    use std::path::PathBuf;

    #[test]
    fn test_sparse_index_filename() {
        assert_eq!(
            sparse_index_filename("a").unwrap(),
            PathBuf::from("1/a.json")
        );
        assert_eq!(
            sparse_index_filename("ab").unwrap(),
            PathBuf::from("2/ab.json")
        );
        assert_eq!(
            sparse_index_filename("foo").unwrap(),
            PathBuf::from("3/f/foo.json")
        );
        assert_eq!(
            sparse_index_filename("foobar").unwrap(),
            PathBuf::from("fo/ob/foobar.json")
        );
        assert_eq!(
            sparse_index_filename("foobar.conda").unwrap(),
            PathBuf::from("fo/ob/foobar.conda.json")
        );
    }
}
