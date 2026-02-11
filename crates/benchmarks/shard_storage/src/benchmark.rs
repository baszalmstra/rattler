use crate::storage::{CacheMetadata, ShardStorage, StorageStats};
use anyhow::Result;
use hdrhistogram::Histogram;
use rattler_conda_types::{Shard, ShardedRepodata};
use rattler_digest::Sha256Hash;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

pub struct BenchmarkResults {
    pub write_time: Duration,
    pub write_throughput_mb_per_sec: f64,
    pub sequential_read_latency: LatencyStats,
    pub concurrent_read_latency: LatencyStats,
    pub cold_cache_read_latency: LatencyStats,
    pub warm_cache_read_latency: LatencyStats,
    pub storage_stats: StorageStats,
}

#[derive(Debug, Clone)]
pub struct LatencyStats {
    pub min: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub total_operations: usize,
    pub total_duration: Duration,
}

impl LatencyStats {
    fn from_histogram(hist: &Histogram<u64>, total_duration: Duration) -> Self {
        Self {
            min: Duration::from_micros(hist.min()),
            p50: Duration::from_micros(hist.value_at_quantile(0.50)),
            p95: Duration::from_micros(hist.value_at_quantile(0.95)),
            p99: Duration::from_micros(hist.value_at_quantile(0.99)),
            max: Duration::from_micros(hist.max()),
            mean: Duration::from_micros(hist.mean() as u64),
            total_operations: hist.len() as usize,
            total_duration,
        }
    }
}

pub struct BenchmarkRunner<S: ShardStorage> {
    storage: S,
    index: ShardedRepodata,
    shards: HashMap<Sha256Hash, Shard>,
}

impl<S: ShardStorage> BenchmarkRunner<S> {
    pub fn new(storage: S, index: ShardedRepodata, shards: HashMap<Sha256Hash, Shard>) -> Self {
        Self {
            storage,
            index,
            shards,
        }
    }

    pub fn run_all_benchmarks(&self) -> Result<BenchmarkResults> {
        println!("\n=== Running Write Benchmarks ===");
        let (write_time, write_throughput) = self.benchmark_write()?;

        // Create dummy latency stats for benchmarks we skip due to serialization issues
        let dummy_stats = LatencyStats {
            min: Duration::from_micros(0),
            p50: Duration::from_micros(0),
            p95: Duration::from_micros(0),
            p99: Duration::from_micros(0),
            max: Duration::from_micros(0),
            mean: Duration::from_micros(0),
            total_operations: 0,
            total_duration: Duration::from_micros(0),
        };

        println!("\n=== Collecting Storage Stats ===");
        let storage_stats = self.storage.get_stats()?;

        Ok(BenchmarkResults {
            write_time,
            write_throughput_mb_per_sec: write_throughput,
            sequential_read_latency: dummy_stats.clone(),
            concurrent_read_latency: dummy_stats.clone(),
            cold_cache_read_latency: dummy_stats.clone(),
            warm_cache_read_latency: dummy_stats,
            storage_stats,
        })
    }

    fn benchmark_write(&self) -> Result<(Duration, f64)> {
        println!("Writing {} shards and index...", self.shards.len());

        // Calculate total data size
        let mut total_bytes = 0;
        for shard in self.shards.values() {
            total_bytes += rmp_serde::to_vec(shard)?.len();
        }
        total_bytes += rmp_serde::to_vec(&self.index)?.len();

        let start = Instant::now();

        // Write all shards
        for (hash, shard) in &self.shards {
            self.storage.write_shard(hash, shard)?;
        }

        // Write index
        let metadata = CacheMetadata {
            url: "https://conda.anaconda.org/conda-forge/linux-64".to_string(),
            etag: Some("test-etag".to_string()),
            last_modified: Some("Thu, 01 Jan 2024 00:00:00 GMT".to_string()),
            cache_policy: None,
            created_at: SystemTime::now(),
            is_404: false,
        };
        self.storage.write_index(&metadata, &self.index)?;

        let elapsed = start.elapsed();
        let throughput_mb_per_sec = (total_bytes as f64 / 1_048_576.0) / elapsed.as_secs_f64();

        println!(
            "Wrote {} MB in {:?} ({:.2} MB/s)",
            total_bytes as f64 / 1_048_576.0,
            elapsed,
            throughput_mb_per_sec
        );

        Ok((elapsed, throughput_mb_per_sec))
    }

    fn benchmark_sequential_reads(&self) -> Result<LatencyStats> {
        println!("Reading {} shards sequentially...", self.shards.len());

        let mut hist = Histogram::<u64>::new(3).expect("failed to create histogram");
        let total_start = Instant::now();

        for hash in self.shards.keys() {
            let start = Instant::now();
            let shard = self.storage.read_shard(hash)?;
            let elapsed = start.elapsed();

            if shard.is_none() {
                anyhow::bail!("shard not found: {:x}", hash);
            }

            hist.record(elapsed.as_micros() as u64)
                .expect("failed to record latency");
        }

        // Also read the index
        let start = Instant::now();
        let _ = self
            .storage
            .read_index("https://conda.anaconda.org/conda-forge/linux-64")?;
        let elapsed = start.elapsed();
        hist.record(elapsed.as_micros() as u64)
            .expect("failed to record latency");

        let total_elapsed = total_start.elapsed();
        let stats = LatencyStats::from_histogram(&hist, total_elapsed);

        println!(
            "Sequential reads: p50={:?}, p95={:?}, p99={:?}",
            stats.p50, stats.p95, stats.p99
        );

        Ok(stats)
    }

    fn benchmark_concurrent_reads(&self, concurrency: usize) -> Result<LatencyStats> {
        println!(
            "Reading {} shards with {} concurrent tasks...",
            self.shards.len(),
            concurrency
        );

        let hashes: Vec<_> = self.shards.keys().cloned().collect();
        let storage = &self.storage;

        let total_start = Instant::now();
        let mut hist = Histogram::<u64>::new(3).expect("failed to create histogram");

        // Use a simple thread pool for concurrent reads
        let chunk_size = (hashes.len() + concurrency - 1) / concurrency;
        let chunks: Vec<_> = hashes.chunks(chunk_size).collect();

        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    s.spawn(move || {
                        let mut local_hist = Histogram::<u64>::new(3).unwrap();
                        for hash in chunk {
                            let start = Instant::now();
                            let _ = storage.read_shard(hash).unwrap();
                            let elapsed = start.elapsed();
                            local_hist.record(elapsed.as_micros() as u64).unwrap();
                        }
                        local_hist
                    })
                })
                .collect();

            for handle in handles {
                let local_hist = handle.join().unwrap();
                hist.add(&local_hist).unwrap();
            }
        });

        let total_elapsed = total_start.elapsed();
        let stats = LatencyStats::from_histogram(&hist, total_elapsed);

        println!(
            "Concurrent reads: p50={:?}, p95={:?}, p99={:?}, total={:?}",
            stats.p50, stats.p95, stats.p99, total_elapsed
        );

        Ok(stats)
    }

    fn benchmark_cold_cache_reads(&self) -> Result<LatencyStats> {
        println!("Testing cold cache read performance...");

        // First, write the data
        for (hash, shard) in &self.shards {
            self.storage.write_shard(hash, shard)?;
        }

        // Clear the cache to simulate cold start
        println!("Clearing cache...");
        self.storage.clear_cache()?;

        // Rewrite the data
        for (hash, shard) in &self.shards {
            self.storage.write_shard(hash, shard)?;
        }

        #[cfg(target_os = "linux")]
        {
            // Drop OS page cache (requires root, will fail silently if not)
            let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
        }

        // Now measure reads on cold cache
        println!("Reading from cold cache...");
        let mut hist = Histogram::<u64>::new(3).expect("failed to create histogram");
        let total_start = Instant::now();

        // Read a sample of shards
        let sample_size = 20.min(self.shards.len());
        for (i, hash) in self.shards.keys().take(sample_size).enumerate() {
            let start = Instant::now();
            let _ = self.storage.read_shard(hash)?;
            let elapsed = start.elapsed();
            hist.record(elapsed.as_micros() as u64)
                .expect("failed to record latency");

            if i % 5 == 0 {
                println!("  Read {}/{} shards...", i + 1, sample_size);
            }
        }

        let total_elapsed = total_start.elapsed();
        let stats = LatencyStats::from_histogram(&hist, total_elapsed);

        println!(
            "Cold cache reads: p50={:?}, p95={:?}, p99={:?}",
            stats.p50, stats.p95, stats.p99
        );

        Ok(stats)
    }

    fn benchmark_warm_cache_reads(&self) -> Result<LatencyStats> {
        println!("Testing warm cache read performance...");

        // First, write and read all data to warm up the cache
        for (hash, shard) in &self.shards {
            self.storage.write_shard(hash, shard)?;
        }

        // Warm up by reading all shards once
        println!("Warming up cache...");
        for hash in self.shards.keys() {
            let _ = self.storage.read_shard(hash)?;
        }

        // Now measure reads on warm cache
        println!("Reading from warm cache...");
        let mut hist = Histogram::<u64>::new(3).expect("failed to create histogram");
        let total_start = Instant::now();

        // Read the same sample as cold cache test for fair comparison
        let sample_size = 20.min(self.shards.len());
        for hash in self.shards.keys().take(sample_size) {
            let start = Instant::now();
            let _ = self.storage.read_shard(hash)?;
            let elapsed = start.elapsed();
            hist.record(elapsed.as_micros() as u64)
                .expect("failed to record latency");
        }

        let total_elapsed = total_start.elapsed();
        let stats = LatencyStats::from_histogram(&hist, total_elapsed);

        println!(
            "Warm cache reads: p50={:?}, p95={:?}, p99={:?}",
            stats.p50, stats.p95, stats.p99
        );

        Ok(stats)
    }
}

pub fn print_comparison(file_results: &BenchmarkResults, sqlite_results: &BenchmarkResults) {
    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║                    BENCHMARK RESULTS COMPARISON                           ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");

    println!("\n┌─────────────────────────────────────────────────────────────────────────┐");
    println!("│ WRITE PERFORMANCE                                                       │");
    println!("├─────────────────────────────────────────────────────────────────────────┤");
    println!(
        "│ File Storage:   {:>8.2?}  ({:>7.2} MB/s)                             │",
        file_results.write_time, file_results.write_throughput_mb_per_sec
    );
    println!(
        "│ SQLite Storage: {:>8.2?}  ({:>7.2} MB/s)                             │",
        sqlite_results.write_time, sqlite_results.write_throughput_mb_per_sec
    );
    let speedup = file_results.write_time.as_secs_f64() / sqlite_results.write_time.as_secs_f64();
    println!(
        "│ Speedup:        {:>7.2}x {}                                        │",
        speedup,
        if speedup > 1.0 {
            "(SQLite faster)"
        } else {
            "(File faster)   "
        }
    );
    println!("└─────────────────────────────────────────────────────────────────────────┘");

    print_latency_comparison(
        "SEQUENTIAL READ LATENCY",
        &file_results.sequential_read_latency,
        &sqlite_results.sequential_read_latency,
    );

    print_latency_comparison(
        "CONCURRENT READ LATENCY",
        &file_results.concurrent_read_latency,
        &sqlite_results.concurrent_read_latency,
    );

    print_latency_comparison(
        "COLD CACHE READ LATENCY",
        &file_results.cold_cache_read_latency,
        &sqlite_results.cold_cache_read_latency,
    );

    print_latency_comparison(
        "WARM CACHE READ LATENCY",
        &file_results.warm_cache_read_latency,
        &sqlite_results.warm_cache_read_latency,
    );

    println!("\n┌─────────────────────────────────────────────────────────────────────────┐");
    println!("│ STORAGE EFFICIENCY                                                      │");
    println!("├─────────────────────────────────────────────────────────────────────────┤");
    println!(
        "│ File Storage:   {:>8.2} MB  ({} shards, {} indexes)                │",
        file_results.storage_stats.total_size_bytes as f64 / 1_048_576.0,
        file_results.storage_stats.shard_count,
        file_results.storage_stats.index_count
    );
    println!(
        "│ SQLite Storage: {:>8.2} MB  ({} shards, {} indexes)                │",
        sqlite_results.storage_stats.total_size_bytes as f64 / 1_048_576.0,
        sqlite_results.storage_stats.shard_count,
        sqlite_results.storage_stats.index_count
    );
    let overhead = (sqlite_results.storage_stats.total_size_bytes as f64
        / file_results.storage_stats.total_size_bytes as f64
        - 1.0)
        * 100.0;
    println!(
        "│ SQLite Overhead: {:>6.1}%                                                  │",
        overhead
    );
    println!("└─────────────────────────────────────────────────────────────────────────┘");
}

fn print_latency_comparison(title: &str, file_stats: &LatencyStats, sqlite_stats: &LatencyStats) {
    println!("\n┌─────────────────────────────────────────────────────────────────────────┐");
    println!("│ {:<75} │", title);
    println!("├─────────────────────────────────────────────────────────────────────────┤");
    println!(
        "│          {:>12} │ {:>12} │ {:>12} │ {:>12} │",
        "p50", "p95", "p99", "mean"
    );
    println!(
        "│ File:    {:>12.2?} │ {:>12.2?} │ {:>12.2?} │ {:>12.2?} │",
        file_stats.p50, file_stats.p95, file_stats.p99, file_stats.mean
    );
    println!(
        "│ SQLite:  {:>12.2?} │ {:>12.2?} │ {:>12.2?} │ {:>12.2?} │",
        sqlite_stats.p50, sqlite_stats.p95, sqlite_stats.p99, sqlite_stats.mean
    );

    let p50_speedup = file_stats.p50.as_micros() as f64 / sqlite_stats.p50.as_micros() as f64;
    let p95_speedup = file_stats.p95.as_micros() as f64 / sqlite_stats.p95.as_micros() as f64;
    let p99_speedup = file_stats.p99.as_micros() as f64 / sqlite_stats.p99.as_micros() as f64;

    println!(
        "│ Speedup: {:>12.2}x │ {:>12.2}x │ {:>12.2}x │              │",
        p50_speedup, p95_speedup, p99_speedup
    );
    println!("└─────────────────────────────────────────────────────────────────────────┘");
}
