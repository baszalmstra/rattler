pub mod file;
pub mod sqlite;
pub mod sqlite_optimized;

use anyhow::Result;
use rattler_conda_types::{Shard, ShardedRepodata};
use rattler_digest::Sha256Hash;
use std::time::SystemTime;

/// HTTP cache metadata for index caching
#[derive(Debug, Clone)]
pub struct CacheMetadata {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_policy: Option<String>, // Serialized CachePolicy
    pub created_at: SystemTime,
    pub is_404: bool,
}

/// Common interface for shard storage backends
pub trait ShardStorage: Send + Sync {
    /// Write a shard to storage
    fn write_shard(&self, hash: &Sha256Hash, shard: &Shard) -> Result<()>;

    /// Read a shard from storage
    fn read_shard(&self, hash: &Sha256Hash) -> Result<Option<Shard>>;

    /// Write an index with cache metadata
    fn write_index(&self, metadata: &CacheMetadata, index: &ShardedRepodata) -> Result<()>;

    /// Read an index by URL
    fn read_index(&self, url: &str) -> Result<Option<(CacheMetadata, ShardedRepodata)>>;

    /// Clear all cached data (for cold cache testing)
    fn clear_cache(&self) -> Result<()>;

    /// Get storage statistics (size, file count, etc.)
    fn get_stats(&self) -> Result<StorageStats>;
}

#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_size_bytes: u64,
    pub shard_count: usize,
    pub index_count: usize,
}
