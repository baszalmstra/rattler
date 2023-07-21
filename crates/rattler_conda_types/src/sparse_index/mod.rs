//! Contains code regarding the Sparse Index, which is a different way of handling the retrieval
//! of records from the index.
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::PackageRecord;

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
    pub fn from_record(
        package_record: PackageRecord,
        filename: String,
    ) -> SparseIndexRecord {
        SparseIndexRecord {
            package_record,
            filename,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
/// A single package in the sparse index
struct SparseIndexPackage {
    /// Sparse index records
    records: Vec<SparseIndexRecord>,
}

/// The entire sparse index
struct SparseIndex {
    /// Package name to sparse index package
    packages: HashMap<String, SparseIndexPackage>,
}