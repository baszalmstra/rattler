use crate::storage::{CacheMetadata, ShardStorage};
use anyhow::{Context, Result};
use rattler_conda_types::{Shard, ShardedRepodata};
use rattler_digest::Sha256Hash;
use std::time::{Duration, Instant, SystemTime};

const CONDA_FORGE_SHARDED_URL: &str = "https://conda.anaconda.org/conda-forge-sharded";
const SUBDIR: &str = "linux-64";

pub struct RemoteBenchmarkResults {
    pub initial_index_fetch: Duration,
    pub initial_shard_fetches: Duration,
    pub cached_index_read: Duration,
    pub cached_shard_reads: Duration,
    pub total_shards_tested: usize,
    pub network_bytes_downloaded: u64,
}

/// Benchmark that tests caching performance against real remote server
pub async fn run_remote_benchmark<S: ShardStorage>(
    storage: S,
    num_shards_to_test: usize,
) -> Result<RemoteBenchmarkResults> {
    println!("\n=== Fetching Sharded Index from Remote ===");

    // Download the sharded index
    let index_url = format!("{}/{}/repodata_shards.msgpack.zst", CONDA_FORGE_SHARDED_URL, SUBDIR);

    let start = Instant::now();
    let client = reqwest::Client::new();
    let response = client
        .get(&index_url)
        .send()
        .await
        .context("failed to download index")?
        .error_for_status()
        .context("index download returned error status")?;

    let compressed_bytes = response.bytes().await?;
    let network_bytes = compressed_bytes.len() as u64;

    // Decompress
    let decompressed_bytes = decompress_zstd(&compressed_bytes).await?;
    let index: ShardedRepodata = rmp_serde::from_slice(&decompressed_bytes)
        .context("failed to parse index")?;

    let initial_index_fetch = start.elapsed();
    println!("  Downloaded index: {} bytes compressed, {} shards total",
             network_bytes, index.shards.len());
    println!("  Time: {:?}", initial_index_fetch);

    // Cache the index
    let metadata = CacheMetadata {
        url: format!("{}/{}", CONDA_FORGE_SHARDED_URL, SUBDIR),
        etag: None,
        last_modified: None,
        cache_policy: None,
        created_at: SystemTime::now(),
        is_404: false,
    };
    storage.write_index(&metadata, &index)?;

    // Select N shards to test (use first N from index for consistency)
    // Store both package names and hashes
    let test_shards: Vec<_> = index.shards.iter()
        .take(num_shards_to_test)
        .map(|(pkg_name, hash)| (pkg_name.clone(), *hash))
        .collect();

    println!("\n=== Downloading {} Sample Shards ===", test_shards.len());

    let start = Instant::now();
    let mut total_network_bytes = network_bytes;
    let mut successful_downloads = 0;
    let mut downloaded_hashes = Vec::new();

    // Construct base URL for shards from the index info
    let shards_base_url = if index.info.shards_base_url.starts_with("http") {
        index.info.shards_base_url.clone()
    } else {
        format!("{}/{}/{}", CONDA_FORGE_SHARDED_URL, SUBDIR, index.info.shards_base_url)
    };

    for (i, (_pkg_name, hash)) in test_shards.iter().enumerate() {
        // Use hash for URL as per CEP-0016
        let shard_url = format!("{}{:x}.msgpack.zst", shards_base_url, hash);

        match client.get(&shard_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let compressed = resp.bytes().await?;
                total_network_bytes += compressed.len() as u64;

                let decompressed = decompress_zstd(&compressed).await?;
                let shard: Shard = rmp_serde::from_slice(&decompressed)
                    .context("failed to parse shard")?;

                // Cache it
                storage.write_shard(hash, &shard)?;
                downloaded_hashes.push(*hash);
                successful_downloads += 1;

                if (i + 1) % 10 == 0 {
                    println!("  Downloaded {}/{} shards...", i + 1, test_shards.len());
                }
            }
            Ok(resp) => {
                // Skip 404s and other errors silently
                if i < 5 {
                    println!("  Skipping shard {:x} (HTTP {})", hash, resp.status());
                }
            }
            Err(e) => {
                if i < 5 {
                    println!("  Skipping shard {:x} ({})", hash, e);
                }
            }
        }

        // Stop if we have at least 20 successful downloads
        if successful_downloads >= 20 {
            break;
        }
    }

    let initial_shard_fetches = start.elapsed();
    println!("  Successfully downloaded {} shards", successful_downloads);
    println!("  Time: {:?}", initial_shard_fetches);
    println!("  Total network data: {:.2} MB", total_network_bytes as f64 / 1_048_576.0);

    if successful_downloads == 0 {
        anyhow::bail!("Failed to download any shards successfully");
    }

    // Now benchmark cached reads
    println!("\n=== Reading from Cache (Index) ===");
    let start = Instant::now();
    let _cached_index = storage.read_index(&metadata.url)?;
    let cached_index_read = start.elapsed();
    println!("  Time: {:?}", cached_index_read);

    println!("\n=== Reading from Cache (Shards) ===");
    let start = Instant::now();
    let cached_shards: Vec<_> = downloaded_hashes.iter()
        .filter_map(|hash| storage.read_shard(hash).ok().flatten())
        .collect();
    let cached_shard_reads = start.elapsed();
    println!("  Read {} shards from cache", cached_shards.len());
    println!("  Time: {:?}", cached_shard_reads);
    println!("  Average per shard: {:?}", cached_shard_reads / cached_shards.len() as u32);

    Ok(RemoteBenchmarkResults {
        initial_index_fetch,
        initial_shard_fetches,
        cached_index_read,
        cached_shard_reads,
        total_shards_tested: successful_downloads,
        network_bytes_downloaded: total_network_bytes,
    })
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

pub fn print_remote_comparison(
    file_results: &RemoteBenchmarkResults,
    sqlite_results: &RemoteBenchmarkResults,
    sqlite_opt_results: &RemoteBenchmarkResults,
) {
    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║              REMOTE CACHING BENCHMARK COMPARISON                          ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");

    println!("\n┌─────────────────────────────────────────────────────────────────────────┐");
    println!("│ INITIAL FETCH FROM REMOTE (Index + {} Shards)                          │",
             file_results.total_shards_tested);
    println!("├─────────────────────────────────────────────────────────────────────────┤");
    println!("│ Network data: {:.2} MB                                                  │",
             file_results.network_bytes_downloaded as f64 / 1_048_576.0);
    println!("│ File:              {:>8.2?}                                             │",
             file_results.initial_index_fetch + file_results.initial_shard_fetches);
    println!("│ SQLite:            {:>8.2?}                                             │",
             sqlite_results.initial_index_fetch + sqlite_results.initial_shard_fetches);
    println!("│ SQLite Optimized:  {:>8.2?}                                             │",
             sqlite_opt_results.initial_index_fetch + sqlite_opt_results.initial_shard_fetches);
    println!("└─────────────────────────────────────────────────────────────────────────┘");

    println!("\n┌─────────────────────────────────────────────────────────────────────────┐");
    println!("│ CACHED INDEX READ                                                       │");
    println!("├─────────────────────────────────────────────────────────────────────────┤");
    println!("│ File:              {:>12.2?}                                         │",
             file_results.cached_index_read);
    println!("│ SQLite:            {:>12.2?}                                         │",
             sqlite_results.cached_index_read);
    println!("│ SQLite Optimized:  {:>12.2?}                                         │",
             sqlite_opt_results.cached_index_read);

    let speedup = file_results.cached_index_read.as_micros() as f64
                  / sqlite_opt_results.cached_index_read.as_micros() as f64;
    println!("│ Speedup (Optimized): {:>6.2}x                                           │", speedup);
    println!("└─────────────────────────────────────────────────────────────────────────┘");

    println!("\n┌─────────────────────────────────────────────────────────────────────────┐");
    println!("│ CACHED SHARD READS ({} shards)                                          │",
             file_results.total_shards_tested);
    println!("├─────────────────────────────────────────────────────────────────────────┤");
    println!("│ File:              {:>12.2?}  ({:>8.2?} avg/shard)                  │",
             file_results.cached_shard_reads,
             file_results.cached_shard_reads / file_results.total_shards_tested as u32);
    println!("│ SQLite:            {:>12.2?}  ({:>8.2?} avg/shard)                  │",
             sqlite_results.cached_shard_reads,
             sqlite_results.cached_shard_reads / sqlite_results.total_shards_tested as u32);
    println!("│ SQLite Optimized:  {:>12.2?}  ({:>8.2?} avg/shard)                  │",
             sqlite_opt_results.cached_shard_reads,
             sqlite_opt_results.cached_shard_reads / sqlite_opt_results.total_shards_tested as u32);

    let speedup = file_results.cached_shard_reads.as_micros() as f64
                  / sqlite_opt_results.cached_shard_reads.as_micros() as f64;
    println!("│ Speedup (Optimized): {:>6.2}x                                           │", speedup);
    println!("└─────────────────────────────────────────────────────────────────────────┘");
}
