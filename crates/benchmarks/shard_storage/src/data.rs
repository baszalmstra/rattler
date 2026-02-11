use anyhow::{Context, Result};
use rand::prelude::IndexedRandom;
use rattler_conda_types::{Shard, ShardedRepodata};
use rattler_digest::{parse_digest_from_hex, Sha256, Sha256Hash};
use std::collections::HashMap;
use std::path::PathBuf;

const CONDA_FORGE_SHARDED_URL: &str = "https://conda.anaconda.org/conda-forge-sharded";
const DEFAULT_SUBDIR: &str = "linux-64";

/// Downloads and caches test data from conda-forge
pub struct TestDataDownloader {
    cache_dir: PathBuf,
    base_url: String,
    subdir: String,
}

impl TestDataDownloader {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            base_url: CONDA_FORGE_SHARDED_URL.to_string(),
            subdir: DEFAULT_SUBDIR.to_string(),
        }
    }

    pub fn with_subdir(mut self, subdir: impl Into<String>) -> Self {
        self.subdir = subdir.into();
        self
    }

    /// Downloads the sharded repodata index
    pub async fn download_index(&self) -> Result<ShardedRepodata> {
        let index_url = format!(
            "{}/{}/repodata_shards.msgpack.zst",
            self.base_url, self.subdir
        );

        println!("Downloading sharded index from: {}", index_url);

        let client = reqwest::Client::new();
        let response = client
            .get(&index_url)
            .send()
            .await
            .context("failed to download index")?
            .error_for_status()
            .context("index download returned error status")?;

        let compressed_bytes = response
            .bytes()
            .await
            .context("failed to read index bytes")?;

        println!(
            "Downloaded {} bytes (compressed)",
            compressed_bytes.len()
        );

        // Decompress zstd
        let decompressed_bytes = decompress_zstd(&compressed_bytes).await?;

        println!(
            "Decompressed to {} bytes",
            decompressed_bytes.len()
        );

        // Parse MessagePack
        let index: ShardedRepodata = rmp_serde::from_slice(&decompressed_bytes)
            .context("failed to parse index from messagepack")?;

        println!("Index contains {} shards", index.shards.len());

        Ok(index)
    }

    /// Downloads a specific shard
    pub async fn download_shard(&self, hash: &Sha256Hash) -> Result<Shard> {
        let shard_url = format!(
            "{}/{}/shards/{:x}.msgpack.zst",
            self.base_url, self.subdir, hash
        );

        let client = reqwest::Client::new();
        let response = client
            .get(&shard_url)
            .send()
            .await
            .context("failed to download shard")?
            .error_for_status()
            .context("shard download returned error status")?;

        let compressed_bytes = response
            .bytes()
            .await
            .context("failed to read shard bytes")?;

        // Decompress zstd
        let decompressed_bytes = decompress_zstd(&compressed_bytes).await?;

        // Parse MessagePack
        let shard: Shard = rmp_serde::from_slice(&decompressed_bytes)
            .context("failed to parse shard from messagepack")?;

        Ok(shard)
    }

    /// Downloads N random shards and returns them with their hashes
    pub async fn download_sample_shards(
        &self,
        index: &ShardedRepodata,
        count: usize,
    ) -> Result<HashMap<Sha256Hash, Shard>> {
        let mut rng = rand::rng();
        let shard_hashes: Vec<_> = index.shards.values().cloned().collect();

        let sample_count = count.min(shard_hashes.len());
        let sampled: Vec<_> = shard_hashes
            .as_slice()
            .choose_multiple(&mut rng, sample_count)
            .cloned()
            .collect();

        println!(
            "Downloading {} sample shards out of {} total...",
            sample_count,
            shard_hashes.len()
        );

        let mut shards = HashMap::new();

        let pb = indicatif::ProgressBar::new(sample_count as u64);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("##-"),
        );

        for (i, hash) in sampled.iter().enumerate() {
            pb.set_message(format!("Shard {}/{}", i + 1, sample_count));

            match self.download_shard(hash).await {
                Ok(shard) => {
                    shards.insert(hash.clone(), shard);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to download shard {:x}: {}", hash, e);
                }
            }

            pb.inc(1);
        }

        pb.finish_with_message("Done");

        println!("Successfully downloaded {} shards", shards.len());

        Ok(shards)
    }

    /// Save downloaded data to cache directory for reuse
    pub async fn save_to_cache(
        &self,
        index: &ShardedRepodata,
        shards: &HashMap<Sha256Hash, Shard>,
    ) -> Result<()> {
        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .context("failed to create cache directory")?;

        // Save index
        let index_path = self.cache_dir.join("index.msgpack");
        let index_bytes = rmp_serde::to_vec(index)?;
        tokio::fs::write(&index_path, index_bytes)
            .await
            .context("failed to write index to cache")?;

        // Save shards
        let shards_dir = self.cache_dir.join("shards");
        tokio::fs::create_dir_all(&shards_dir)
            .await
            .context("failed to create shards directory")?;

        for (hash, shard) in shards {
            let shard_path = shards_dir.join(format!("{:x}.msgpack", hash));
            let shard_bytes = rmp_serde::to_vec(shard)?;
            tokio::fs::write(&shard_path, shard_bytes)
                .await
                .context("failed to write shard to cache")?;
        }

        println!("Saved data to cache: {}", self.cache_dir.display());

        Ok(())
    }

    /// Load previously cached data
    pub async fn load_from_cache(&self) -> Result<(ShardedRepodata, HashMap<Sha256Hash, Shard>)> {
        let index_path = self.cache_dir.join("index.msgpack");
        let index_bytes = tokio::fs::read(&index_path)
            .await
            .context("failed to read cached index")?;
        let index: ShardedRepodata = rmp_serde::from_slice(&index_bytes)?;

        let shards_dir = self.cache_dir.join("shards");
        let mut shards = HashMap::new();

        let mut entries = tokio::fs::read_dir(&shards_dir)
            .await
            .context("failed to read shards directory")?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .context("failed to read directory entry")?
        {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("msgpack") {
                let shard_bytes = tokio::fs::read(&path).await?;
                let shard: Shard = rmp_serde::from_slice(&shard_bytes)?;

                // Extract hash from filename
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Some(hash) = parse_digest_from_hex::<Sha256>(stem) {
                        shards.insert(hash, shard);
                    }
                }
            }
        }

        println!(
            "Loaded {} shards from cache: {}",
            shards.len(),
            self.cache_dir.display()
        );

        Ok((index, shards))
    }

    /// Check if cached data exists
    pub async fn has_cached_data(&self) -> bool {
        let index_path = self.cache_dir.join("index.msgpack");
        let shards_dir = self.cache_dir.join("shards");
        index_path.exists() && shards_dir.exists()
    }
}

async fn decompress_zstd(compressed: &[u8]) -> Result<Vec<u8>> {
    use async_compression::tokio::bufread::ZstdDecoder;
    use tokio::io::AsyncReadExt;

    let reader = std::io::Cursor::new(compressed);
    let mut decoder = ZstdDecoder::new(reader);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .await
        .context("failed to decompress zstd")?;
    Ok(decompressed)
}
