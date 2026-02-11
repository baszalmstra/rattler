mod benchmark;
mod data;
mod remote_benchmark;
mod storage;
mod synthetic;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use benchmark::{print_comparison, BenchmarkRunner};
use storage::{file::FileStorage, sqlite::SqliteStorage, sqlite_optimized::SqliteStorageOptimized};

#[derive(Parser)]
#[command(name = "shard-bench")]
#[command(about = "Benchmark file vs SQLite storage for sharded repodata")]
struct Args {
    /// Number of shards to download and test
    #[arg(short, long, default_value = "100")]
    shard_count: usize,

    /// Directory to cache downloaded test data
    #[arg(short = 'd', long, default_value = "test_data")]
    test_data_dir: PathBuf,

    /// Skip downloading and use cached data
    #[arg(long)]
    use_cache: bool,

    /// Conda subdirectory to test (e.g., linux-64, osx-64)
    #[arg(long, default_value = "linux-64")]
    subdir: String,

    /// Directory to store benchmark databases (for testing different drives)
    #[arg(long)]
    benchmark_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!("╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║         Shard Storage Benchmark: File vs SQLite                          ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Generate synthetic test data for benchmarking
    println!("Generating synthetic test data...");
    println!("  Number of shards: {}", args.shard_count);
    println!("  Packages per shard: 10");
    println!();

    let (index, shards) = synthetic::generate_synthetic_data(args.shard_count, 10)?;

    println!("\nTest data ready:");
    println!("  Index contains {} total shards", index.shards.len());
    println!("  Testing with {} shards", shards.len());
    println!();

    // Create benchmark directories - use specified dir or temp
    let (file_storage_dir, sqlite_db_path, sqlite_optimized_db_path) = if let Some(bench_dir) = &args.benchmark_dir {
        std::fs::create_dir_all(bench_dir)?;
        (
            bench_dir.join("file_storage"),
            bench_dir.join("sqlite_storage.db"),
            bench_dir.join("sqlite_optimized_storage.db"),
        )
    } else {
        let temp_dir = tempfile::tempdir()?;
        (
            temp_dir.path().join("file_storage"),
            temp_dir.path().join("sqlite_storage.db"),
            temp_dir.path().join("sqlite_optimized_storage.db"),
        )
    };

    println!("Benchmark directories:");
    println!("  File storage:        {}", file_storage_dir.display());
    println!("  SQLite storage:      {}", sqlite_db_path.display());
    println!("  SQLite (optimized):  {}", sqlite_optimized_db_path.display());
    println!();

    // Run file storage benchmarks
    println!("═══════════════════════════════════════════════════════════════════════════");
    println!("                       FILE STORAGE BENCHMARKS                             ");
    println!("═══════════════════════════════════════════════════════════════════════════");

    let file_storage = FileStorage::new(file_storage_dir)?;
    let file_runner = BenchmarkRunner::new(file_storage, index.clone(), shards.clone());
    let file_results = file_runner.run_all_benchmarks()?;

    // Run SQLite storage benchmarks
    println!("\n═══════════════════════════════════════════════════════════════════════════");
    println!("                       SQLITE STORAGE BENCHMARKS                           ");
    println!("═══════════════════════════════════════════════════════════════════════════");

    let sqlite_storage = SqliteStorage::new(sqlite_db_path)?;
    let sqlite_runner = BenchmarkRunner::new(sqlite_storage, index.clone(), shards.clone());
    let sqlite_results = sqlite_runner.run_all_benchmarks()?;

    // Run OPTIMIZED SQLite storage benchmarks
    println!("\n═══════════════════════════════════════════════════════════════════════════");
    println!("                  SQLITE OPTIMIZED STORAGE BENCHMARKS                      ");
    println!("═══════════════════════════════════════════════════════════════════════════");

    let sqlite_optimized_storage = SqliteStorageOptimized::new(sqlite_optimized_db_path)?;
    let sqlite_optimized_runner = BenchmarkRunner::new(sqlite_optimized_storage, index, shards);
    let sqlite_optimized_results = sqlite_optimized_runner.run_all_benchmarks()?;

    // Print comparisons
    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║                    FILE vs SQLITE (BASELINE)                             ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    print_comparison(&file_results, &sqlite_results);

    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║                  FILE vs SQLITE (OPTIMIZED)                              ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    print_comparison(&file_results, &sqlite_optimized_results);

    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║              SQLITE BASELINE vs SQLITE OPTIMIZED                         ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    print_comparison(&sqlite_results, &sqlite_optimized_results);

    println!("\n✓ Benchmark complete!");

    Ok(())
}
