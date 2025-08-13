//! Conda history file parsing and management functionality.
//!
//! This module provides types and functions for working with conda history files.
//! Conda history files track the installation, update, and removal of packages
//! in conda environments over time.

mod revision;

pub use revision::{
    CreateOperation, CustomOperation, InstallOperation, PackageChange, PackageOperation, 
    RemoveOperation, Revision, UpdateOperation, UserRequest
};

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use thiserror::Error;

use crate::PackageName;

/// Errors that can occur when working with conda history files.
#[derive(Debug, Error)]
pub enum HistoryError {
    /// IO error occurred during file operations
    #[error(transparent)]
    Io(#[from] std::io::Error),
    
    /// Error parsing history file
    #[error("Parse error at line {line}: {message}")]
    ParseError {
        /// The line number where the parsing error occurred
        line: usize,
        /// The error message
        message: String,
    },
    
    /// Invalid revision index
    #[error("Invalid revision {revision}, max revision is {max}")]
    InvalidRevision {
        /// The requested revision number
        revision: usize,
        /// The maximum available revision number
        max: usize,
    },
}


/// Represents a conda environment's history file.
/// 
/// This struct provides a Vec-like interface for managing conda history revisions.
/// Each revision represents a transaction (install, remove, update) that occurred
/// in the conda environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// The list of revisions in chronological order
    revisions: Vec<Revision>,
}

impl History {
    /// Create a new empty history
    pub fn new() -> Self {
        Self {
            revisions: Vec::new(),
        }
    }
    
    /// Load history from a file path
    pub fn from_path(_path: impl AsRef<Path>) -> Result<Self, HistoryError> {
        // TODO: Implement parsing
        Ok(Self::new())
    }
    
    /// Add a new revision to the history (Vec-like API)
    pub fn push(&mut self, revision: Revision) {
        self.revisions.push(revision);
    }
    
    /// Get an iterator over the revisions
    pub fn iter(&self) -> std::slice::Iter<'_, Revision> {
        self.revisions.iter()
    }
    
    /// Write the history to a file
    pub fn to_path(&self, path: &Path) -> Result<(), HistoryError> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        
        for revision in &self.revisions {
            write!(writer, "{revision}")?;
        }
        
        writer.flush()?;
        Ok(())
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl FromIterator<Revision> for History {
    fn from_iter<T: IntoIterator<Item = Revision>>(iter: T) -> Self {
        Self {
            revisions: iter.into_iter().collect(),
        }
    }
}

impl IntoIterator for History {
    type Item = Revision;
    type IntoIter = std::vec::IntoIter<Revision>;
    
    fn into_iter(self) -> Self::IntoIter {
        self.revisions.into_iter()
    }
}

impl<'a> IntoIterator for &'a History {
    type Item = &'a Revision;
    type IntoIter = std::slice::Iter<'a, Revision>;
    
    fn into_iter(self) -> Self::IntoIter {
        self.revisions.iter()
    }
}

/// Environment state at a specific revision (for state reconstruction)
pub type EnvironmentState = HashMap<PackageName, PackageChange>;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use crate::history::{InstallOperation, UpdateOperation};

    #[test]
    fn test_history_new_and_default() {
        let history1 = History::new();
        let history2 = History::default();
        assert_eq!(history1, history2);
        assert!(history1.iter().count() == 0);
    }
    
    #[test]
    fn test_history_push_and_iter() {
        let mut history = History::new();
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let revision = Revision::new(timestamp, InstallOperation { specs: vec![] }.into(), vec![]);
        
        history.push(revision.clone());
        assert_eq!(history.iter().count(), 1);
        assert_eq!(history.iter().next().unwrap(), &revision);
    }
    
    #[test]
    fn test_history_from_iterator() {
        let timestamp1 = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let timestamp2 = Utc.with_ymd_and_hms(2023, 1, 2, 12, 0, 0).unwrap();
        let revision1 = Revision::new(timestamp1, InstallOperation { specs: vec![] }.into(), vec![]);
        let revision2 = Revision::new(timestamp2, UpdateOperation { specs: vec![] }.into(), vec![]);
        
        let revisions = vec![revision1.clone(), revision2.clone()];
        let history: History = revisions.into_iter().collect();
        
        assert_eq!(history.iter().count(), 2);
        let collected: Vec<_> = history.iter().cloned().collect();
        assert_eq!(collected[0], revision1);
        assert_eq!(collected[1], revision2);
    }
    
    #[test]
    fn test_history_into_iterator() {
        let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 12, 0, 0).unwrap();
        let revision = Revision::new(timestamp, InstallOperation { specs: vec![] }.into(), vec![]);
        let mut history = History::new();
        history.push(revision.clone());
        
        // Test owned iterator
        let collected: Vec<_> = history.clone().into_iter().collect();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0], revision);
        
        // Test borrowed iterator
        let borrowed: Vec<_> = (&history).into_iter().cloned().collect();
        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0], revision);
    }
}

