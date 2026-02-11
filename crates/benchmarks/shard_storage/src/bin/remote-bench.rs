use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use shard_storage_benchmark::remote_benchmark::{run_remote_benchmark, print_remote_comparison};
use shard_storage_benchmark::storage::{file::FileStorage, sqlite::SqliteStorage, sqlite_optimized::SqliteStorageOptimized};

#[derive(Parser)]
#[command(name = "remote-bench")]
#[command(about = "Benchmark file vs SQLite storage with real remote conda-forge-sharded data")]
struct Args {
    /// Number of shards to download and test
    #[arg(short, long, default_value = "50")]
    shard_count: usize,

    /// Directory to store benchmark databases
    #[arg(long)]
    benchmark_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!("╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║    Remote Caching Benchmark: File vs SQLite vs SQLite Optimized          ║");
    println!("║    Testing against: conda-forge-sharded                                   ║");
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Create benchmark directories
    let (file_storage_dir, sqlite_db_path, sqlite_optimized_db_path) = if let Some(bench_dir) = &args.benchmark_dir {
        std::fs::create_dir_all(bench_dir)?;
        (
            bench_dir.join("file_storage_remote"),
            bench_dir.join("sqlite_storage_remote.db"),
            bench_dir.join("sqlite_optimized_storage_remote.db"),
        )
    } else {
        let temp_dir = tempfile::tempdir()?;
        (
            temp_dir.path().join("file_storage_remote"),
            temp_dir.path().join("sqlite_storage_remote.db"),
            temp_dir.path().join("sqlite_optimized_storage_remote.db"),
        )
    };

    println!("Benchmark directories:");
    println!("  File storage:        {}", file_storage_dir.display());
    println!("  SQLite storage:      {}", sqlite_db_path.display());
    println!("  SQLite (optimized):  {}", sqlite_optimized_db_path.display());
    println!();

    // Run file storage benchmarks
    println!("═══════════════════════════════════════════════════════════════════════════");
    println!("                    FILE STORAGE REMOTE BENCHMARK                          ");
    println!("═══════════════════════════════════════════════════════════════════════════");

    let file_storage = FileStorage::new(file_storage_dir)?;
    let file_results = run_remote_benchmark(file_storage, args.shard_count).await?;

    // Run SQLite storage benchmarks
    println!("\n═══════════════════════════════════════════════════════════════════════════");
    println!("                    SQLITE STORAGE REMOTE BENCHMARK                        ");
    println!("═══════════════════════════════════════════════════════════════════════════");

    let sqlite_storage = SqliteStorage::new(sqlite_db_path)?;
    let sqlite_results = run_remote_benchmark(sqlite_storage, args.shard_count).await?;

    // Run OPTIMIZED SQLite storage benchmarks
    println!("\n═══════════════════════════════════════════════════════════════════════════");
    println!("               SQLITE OPTIMIZED STORAGE REMOTE BENCHMARK                   ");
    println!("═══════════════════════════════════════════════════════════════════════════");

    let sqlite_optimized_storage = SqliteStorageOptimized::new(sqlite_optimized_db_path)?;
    let sqlite_optimized_results = run_remote_benchmark(sqlite_optimized_storage, args.shard_count).await?;

    // Print comparison
    print_remote_comparison(&file_results, &sqlite_results, &sqlite_optimized_results);

    println!("\n✓ Remote benchmark complete!");

    Ok(())
}
