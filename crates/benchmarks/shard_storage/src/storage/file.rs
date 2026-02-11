use super::{CacheMetadata, ShardStorage, StorageStats};
use anyhow::{Context, Result};
use rattler_conda_types::{Shard, ShardedRepodata};
use rattler_digest::Sha256Hash;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

const MAGIC_NUMBER: &[u8] = b"SHARD-CACHE-V1";

/// File-based storage backend that mirrors rattler_repodata_gateway implementation
pub struct FileStorage {
    base_dir: PathBuf,
}

impl FileStorage {
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base_dir).context("failed to create base directory")?;

        // Create shards subdirectory
        let shards_dir = base_dir.join("shards-v1");
        fs::create_dir_all(&shards_dir).context("failed to create shards directory")?;

        Ok(Self { base_dir })
    }

    fn shard_path(&self, hash: &Sha256Hash) -> PathBuf {
        self.base_dir
            .join("shards-v1")
            .join(format!("{:x}.msgpack", hash))
    }

    fn index_path(&self, url: &str) -> PathBuf {
        // Create a simple hash of the URL for the filename
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        let hash = hasher.finalize();
        let hash_prefix = hex::encode(&hash[..8]);
        let filename = format!("{}.shards-cache-v1", hash_prefix);
        self.base_dir.join(filename)
    }
}

impl ShardStorage for FileStorage {
    fn write_shard(&self, hash: &Sha256Hash, shard: &Shard) -> Result<()> {
        let path = self.shard_path(hash);

        // Serialize to MessagePack
        let bytes =
            rmp_serde::to_vec(shard).context("failed to serialize shard to messagepack")?;

        // Write atomically using tempfile
        let temp_dir = path.parent().expect("shard path must have parent");
        let mut temp_file = tempfile::Builder::new()
            .tempfile_in(temp_dir)
            .context("failed to create temp file")?;

        temp_file
            .write_all(&bytes)
            .context("failed to write shard to temp file")?;

        if let Err(e) = temp_file.persist(&path) {
            // If persist fails but file exists, someone else wrote it (concurrent write)
            if !path.is_file() {
                return Err(e).context("failed to persist shard to cache");
            }
        }

        Ok(())
    }

    fn read_shard(&self, hash: &Sha256Hash) -> Result<Option<Shard>> {
        let path = self.shard_path(hash);

        match fs::read(&path) {
            Ok(bytes) => {
                let shard = rmp_serde::from_slice(&bytes)
                    .context("failed to deserialize shard from messagepack")?;
                Ok(Some(shard))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("failed to read shard from cache"),
        }
    }

    fn write_index(&self, metadata: &CacheMetadata, index: &ShardedRepodata) -> Result<()> {
        let path = self.index_path(&metadata.url);

        // Serialize index to MessagePack
        let index_bytes =
            rmp_serde::to_vec(index).context("failed to serialize index to messagepack")?;

        // Create cache header
        let header = CacheHeader {
            etag: metadata.etag.clone(),
            last_modified: metadata.last_modified.clone(),
            cache_policy: metadata.cache_policy.clone(),
            created_at: metadata
                .created_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            is_404: metadata.is_404,
        };

        let header_bytes =
            rmp_serde::to_vec(&header).context("failed to serialize cache header")?;

        // Write cache file: MAGIC | header_len (u32) | header | body
        let mut file = fs::File::create(&path).context("failed to create cache file")?;

        file.write_all(MAGIC_NUMBER)
            .context("failed to write magic number")?;
        file.write_all(&(header_bytes.len() as u32).to_le_bytes())
            .context("failed to write header length")?;
        file.write_all(&header_bytes)
            .context("failed to write header")?;
        file.write_all(&index_bytes)
            .context("failed to write index body")?;

        file.sync_all()
            .context("failed to sync cache file to disk")?;

        Ok(())
    }

    fn read_index(&self, url: &str) -> Result<Option<(CacheMetadata, ShardedRepodata)>> {
        let path = self.index_path(url);

        let mut file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("failed to open cache file"),
        };

        // Read and verify magic number
        let mut magic = vec![0u8; MAGIC_NUMBER.len()];
        file.read_exact(&mut magic)
            .context("failed to read magic number")?;
        if magic != MAGIC_NUMBER {
            anyhow::bail!("invalid magic number in cache file");
        }

        // Read header length
        let mut header_len_bytes = [0u8; 4];
        file.read_exact(&mut header_len_bytes)
            .context("failed to read header length")?;
        let header_len = u32::from_le_bytes(header_len_bytes) as usize;

        // Read header
        let mut header_bytes = vec![0u8; header_len];
        file.read_exact(&mut header_bytes)
            .context("failed to read header")?;
        let header: CacheHeader = rmp_serde::from_slice(&header_bytes)
            .context("failed to deserialize cache header")?;

        // Read body
        let mut body_bytes = Vec::new();
        file.read_to_end(&mut body_bytes)
            .context("failed to read index body")?;
        let index: ShardedRepodata = rmp_serde::from_slice(&body_bytes)
            .context("failed to deserialize index from messagepack")?;

        let metadata = CacheMetadata {
            url: url.to_string(),
            etag: header.etag,
            last_modified: header.last_modified,
            cache_policy: header.cache_policy,
            created_at: std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(header.created_at),
            is_404: header.is_404,
        };

        Ok(Some((metadata, index)))
    }

    fn clear_cache(&self) -> Result<()> {
        // Remove the entire cache directory
        if self.base_dir.exists() {
            fs::remove_dir_all(&self.base_dir).context("failed to remove cache directory")?;
            fs::create_dir_all(&self.base_dir).context("failed to recreate cache directory")?;
            fs::create_dir_all(self.base_dir.join("shards-v1"))
                .context("failed to recreate shards directory")?;
        }
        Ok(())
    }

    fn get_stats(&self) -> Result<StorageStats> {
        let mut total_size = 0u64;
        let mut shard_count = 0usize;
        let mut index_count = 0usize;

        // Count shards
        let shards_dir = self.base_dir.join("shards-v1");
        if shards_dir.exists() {
            for entry in fs::read_dir(&shards_dir).context("failed to read shards directory")? {
                let entry = entry?;
                if entry.path().is_file() {
                    let metadata = entry.metadata()?;
                    total_size += metadata.len();
                    shard_count += 1;
                }
            }
        }

        // Count index files
        for entry in fs::read_dir(&self.base_dir).context("failed to read base directory")? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("shards-cache-v1") {
                let metadata = entry.metadata()?;
                total_size += metadata.len();
                index_count += 1;
            }
        }

        Ok(StorageStats {
            total_size_bytes: total_size,
            shard_count,
            index_count,
        })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CacheHeader {
    etag: Option<String>,
    last_modified: Option<String>,
    cache_policy: Option<String>,
    created_at: u64, // Unix timestamp
    #[serde(default)]
    is_404: bool,
}
