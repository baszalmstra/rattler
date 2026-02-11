use super::{CacheMetadata, ShardStorage, StorageStats};
use anyhow::{Context, Result};
use rattler_conda_types::{Shard, ShardedRepodata};
use rattler_digest::{parse_digest_from_hex, Sha256, Sha256Hash};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Mutex;

/// Optimized SQLite storage with separate read/write connections and batch support
pub struct SqliteStorageOptimized {
    write_conn: Mutex<Connection>,  // Dedicated for writes
    read_conn: Mutex<Connection>,   // Dedicated for reads (non-blocking)
}

impl SqliteStorageOptimized {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        // Create parent directory if needed
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).context("failed to create database directory")?;
        }

        // Open write connection first to create schema
        let write_conn = Connection::open(&db_path).context("failed to open sqlite database")?;

        // CRITICAL: Set page size BEFORE creating tables
        write_conn.execute_batch(
            "
            PRAGMA page_size = 65536;  -- 64KB pages for large blobs
            ",
        )
        .context("failed to set page size")?;

        // Configure write connection for maximum write performance
        write_conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;  -- Good balance of safety and speed
            PRAGMA wal_autocheckpoint = 0;  -- Manual checkpointing for bulk writes
            PRAGMA journal_size_limit = -1;  -- Unlimited journal
            PRAGMA cache_size = -128000;  -- 128MB cache (64KB pages × 2000)
            PRAGMA temp_store = MEMORY;
            PRAGMA mmap_size = 536870912;  -- 512MB mmap
            PRAGMA locking_mode = NORMAL;  -- Allow concurrent readers
            ",
        )
        .context("failed to configure write connection")?;

        // Create schema (only needed once)
        write_conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS shards (
                hash BLOB PRIMARY KEY CHECK(length(hash) = 32),
                data BLOB NOT NULL,
                created_at INTEGER NOT NULL
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS idx_shards_created ON shards(created_at);

            CREATE TABLE IF NOT EXISTS index_cache (
                url TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                etag TEXT,
                last_modified TEXT,
                cache_policy TEXT,
                created_at INTEGER NOT NULL,
                is_404 INTEGER NOT NULL DEFAULT 0
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS idx_index_created ON index_cache(created_at);
            ",
        )
        .context("failed to create schema")?;

        // Open separate read connection
        let read_conn = Connection::open(&db_path).context("failed to open read connection")?;

        // Configure read connection for maximum read performance
        read_conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -128000;  -- 128MB cache
            PRAGMA temp_store = MEMORY;
            PRAGMA mmap_size = 536870912;  -- 512MB mmap
            PRAGMA query_only = 1;  -- Mark as read-only connection
            ",
        )
        .context("failed to configure read connection")?;

        Ok(Self {
            write_conn: Mutex::new(write_conn),
            read_conn: Mutex::new(read_conn),
        })
    }

    /// Convert a Sha256Hash to 32-byte array
    fn hash_to_bytes(hash: &Sha256Hash) -> Result<[u8; 32]> {
        let hex_str = format!("{:x}", hash);
        let bytes = hex::decode(&hex_str).context("failed to decode hash as hex")?;
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("hash is not 32 bytes"))
    }

    /// Convert 32-byte array back to Sha256Hash
    #[allow(dead_code)]
    fn bytes_to_hash(bytes: &[u8]) -> Result<Sha256Hash> {
        if bytes.len() != 32 {
            anyhow::bail!("hash bytes must be 32 bytes, got {}", bytes.len());
        }
        let hex_str = hex::encode(bytes);
        parse_digest_from_hex::<Sha256>(&hex_str)
            .ok_or_else(|| anyhow::anyhow!("failed to parse hash from hex string"))
    }

    /// Write multiple shards in a single transaction (10-100x faster)
    pub fn write_shards_batch(&self, shards: &[(Sha256Hash, Shard)]) -> Result<()> {
        let conn = self.write_conn.lock().unwrap();

        // Begin explicit transaction
        conn.execute("BEGIN IMMEDIATE", [])
            .context("failed to begin transaction")?;

        // Use prepared statement for efficiency
        let mut stmt = conn
            .prepare_cached(
                "INSERT OR REPLACE INTO shards (hash, data, created_at) VALUES (?1, ?2, ?3)",
            )
            .context("failed to prepare statement")?;

        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Execute all inserts within transaction
        for (hash, shard) in shards {
            let hash_bytes = Self::hash_to_bytes(hash)?;
            let shard_bytes = rmp_serde::to_vec(shard)
                .context("failed to serialize shard to messagepack")?;
            stmt.execute(rusqlite::params![&hash_bytes[..], shard_bytes, created_at])
                .context("failed to insert shard")?;
        }

        // Single fsync for all writes
        conn.execute("COMMIT", [])
            .context("failed to commit transaction")?;

        Ok(())
    }

    /// Manually checkpoint the WAL after bulk writes
    pub fn checkpoint_wal(&self) -> Result<()> {
        let conn = self.write_conn.lock().unwrap();
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", [])
            .context("failed to checkpoint WAL")?;
        Ok(())
    }
}

impl ShardStorage for SqliteStorageOptimized {
    fn write_shard(&self, hash: &Sha256Hash, shard: &Shard) -> Result<()> {
        let hash_bytes = Self::hash_to_bytes(hash)?;
        let shard_bytes =
            rmp_serde::to_vec(shard).context("failed to serialize shard to messagepack")?;
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let conn = self.write_conn.lock().unwrap();

        // Use prepared statement cache
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO shards (hash, data, created_at) VALUES (?1, ?2, ?3)"
        )?;

        stmt.execute(rusqlite::params![&hash_bytes[..], shard_bytes, created_at])
            .context("failed to insert shard into database")?;

        Ok(())
    }

    fn read_shard(&self, hash: &Sha256Hash) -> Result<Option<Shard>> {
        let hash_bytes = Self::hash_to_bytes(hash)?;

        // Use dedicated read connection (doesn't block on writes!)
        let conn = self.read_conn.lock().unwrap();
        let shard_bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data FROM shards WHERE hash = ?1",
                rusqlite::params![&hash_bytes[..]],
                |row| row.get(0),
            )
            .optional()
            .context("failed to query shard from database")?;

        match shard_bytes {
            Some(bytes) => {
                let shard = rmp_serde::from_slice(&bytes)
                    .context("failed to deserialize shard from messagepack")?;
                Ok(Some(shard))
            }
            None => Ok(None),
        }
    }

    fn write_index(&self, metadata: &CacheMetadata, index: &ShardedRepodata) -> Result<()> {
        let index_bytes =
            rmp_serde::to_vec(index).context("failed to serialize index to messagepack")?;
        let created_at = metadata
            .created_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let conn = self.write_conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO index_cache
                 (url, data, etag, last_modified, cache_policy, created_at, is_404)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                &metadata.url,
                index_bytes,
                metadata.etag.as_deref(),
                metadata.last_modified.as_deref(),
                metadata.cache_policy.as_deref(),
                created_at,
                metadata.is_404 as i32,
            ],
        )
        .context("failed to insert index into database")?;

        Ok(())
    }

    fn read_index(&self, url: &str) -> Result<Option<(CacheMetadata, ShardedRepodata)>> {
        // Use dedicated read connection
        let conn = self.read_conn.lock().unwrap();
        let result: Option<(Vec<u8>, Option<String>, Option<String>, Option<String>, i64, i32)> =
            conn.query_row(
                "SELECT data, etag, last_modified, cache_policy, created_at, is_404
                 FROM index_cache WHERE url = ?1",
                rusqlite::params![url],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .context("failed to query index from database")?;

        match result {
            Some((index_bytes, etag, last_modified, cache_policy, created_at, is_404)) => {
                let index: ShardedRepodata = rmp_serde::from_slice(&index_bytes)
                    .context("failed to deserialize index from messagepack")?;

                let metadata = CacheMetadata {
                    url: url.to_string(),
                    etag,
                    last_modified,
                    cache_policy,
                    created_at: std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(created_at as u64),
                    is_404: is_404 != 0,
                };

                Ok(Some((metadata, index)))
            }
            None => Ok(None),
        }
    }

    fn clear_cache(&self) -> Result<()> {
        let conn = self.write_conn.lock().unwrap();
        conn.execute("DELETE FROM shards", [])
            .context("failed to clear shards table")?;
        conn.execute("DELETE FROM index_cache", [])
            .context("failed to clear index_cache table")?;

        // Vacuum to reclaim space
        conn.execute("VACUUM", [])
            .context("failed to vacuum database")?;

        Ok(())
    }

    fn get_stats(&self) -> Result<StorageStats> {
        // Use read connection for stats
        let conn = self.read_conn.lock().unwrap();

        let shard_count: usize = conn
            .query_row("SELECT COUNT(*) FROM shards", [], |row| row.get(0))
            .context("failed to count shards")?;

        let index_count: usize = conn
            .query_row("SELECT COUNT(*) FROM index_cache", [], |row| row.get(0))
            .context("failed to count indexes")?;

        // Get total data size (approximation using SQLite page count)
        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .context("failed to get page count")?;
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .context("failed to get page size")?;

        let total_size_bytes = (page_count * page_size) as u64;

        Ok(StorageStats {
            total_size_bytes,
            shard_count,
            index_count,
        })
    }
}
