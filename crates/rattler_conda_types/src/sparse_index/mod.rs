//! Contains code regarding the Sparse Index, which is a different way of handling the retrieval
//! of records from the index.
use crate::{PackageRecord, RepoData};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Serialize, Deserialize)]
/// Record in a sparse index
/// contains the package record and a filename
struct SparseIndexRecord {
    #[serde(flatten)]
    /// Actual data regarding the package
    package_record: PackageRecord,
    /// Filename to address the package with
    filename: String,
}

impl SparseIndexRecord {
    /// Create a [`SparseIndexRecord`] from a filename and a [`PackageRecord`]
    pub fn from_record(package_record: PackageRecord, filename: String) -> SparseIndexRecord {
        SparseIndexRecord {
            package_record,
            filename,
        }
    }
}

#[derive(Debug)]
/// A single package in the sparse index
struct SparseIndexPackage {
    /// Sparse index records
    records: Vec<SparseIndexRecord>,
}

impl SparseIndexPackage {

    /// Write a sparse index package to a file
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<(), WriteSparseIndexError> {
        let mut writer = std::io::LineWriter::new(writer);
        for record in self.records.iter() {
            writer.write_all(serde_json::to_string(record)?.as_bytes())?
        }
        Ok(())
    }
}

/// The entire sparse index
struct SparseIndex {
    /// Package name to sparse index package
    pub packages: HashMap<String, SparseIndexPackage>,
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
pub fn sparse_index_filename(filename: &Path) -> Result<PathBuf, SparseIndexFilenameError> {
    let mut new_path = PathBuf::new();
    let filename = filename
        .file_name()
        .ok_or_else(|| SparseIndexFilenameError::NoFileNameComponent)?;
    let filename = filename
        .to_str()
        .ok_or_else(|| SparseIndexFilenameError::Utf8Error)?;

    // Create path according to rules in docs
    match filename.len() {
        0 => {
            return Err(SparseIndexFilenameError::EmptyFilename);
        }
        // Will yield something like 1/a
        1 => {
            new_path.push("1");
            new_path.push(filename);
        }
        // Will yield something like 2/ab
        2 => {
            new_path.push("2");
            new_path.push(filename);
        }

        // Will yield something like 3/ab/abc
        3 => {
            new_path.push("3");
            new_path.push(&filename[0..1]);
            new_path.push(filename);
        }
        // Will yield something like py/th/python
        _ => {
            new_path.push(&filename[0..2]);
            new_path.push(&filename[2..4]);
            new_path.push(filename);
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
    pub fn write_index_to(&self, path: &Path) -> Result<(), WriteSparseIndexError> {
        for (package, sparse_index_package) in self.packages.iter() {
            // Create the directory for the package
            let package_path = sparse_index_filename(Path::new(package))?;
            std::fs::create_dir_all(&package_path)?;

            // Write the file
            let file = std::fs::File::create(package_path)?;
            let mut writer = std::io::BufWriter::new(file);
            sparse_index_package.write(&mut writer)?;
        }

        Ok(())
    }
}

impl From<RepoData> for SparseIndex {
    fn from(value: RepoData) -> Self {
        let mut packages = HashMap::new();
        for (package_name, record) in value.packages.into_iter() {
            let sparse_index_record = SparseIndexRecord::from_record(record, package_name.clone());
            packages
                .entry(package_name)
                .or_insert_with(|| SparseIndexPackage { records: vec![] })
                .records
                .push(sparse_index_record);
        }

        SparseIndex {
            packages,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::sparse_index_filename;
    use std::path::{Path, PathBuf};

    #[test]
    fn test_sparse_index_filename() {
        assert_eq!(
            sparse_index_filename(Path::new("a")).unwrap(),
            PathBuf::from("1/a")
        );
        assert_eq!(
            sparse_index_filename(Path::new("ab")).unwrap(),
            PathBuf::from("2/ab")
        );
        assert_eq!(
            sparse_index_filename(Path::new("foo")).unwrap(),
            PathBuf::from("3/f/foo")
        );
        assert_eq!(
            sparse_index_filename(Path::new("foobar")).unwrap(),
            PathBuf::from("fo/ob/foobar")
        );
        assert_eq!(
            sparse_index_filename(Path::new("foobar.conda")).unwrap(),
            PathBuf::from("fo/ob/foobar.conda")
        );
    }
}
