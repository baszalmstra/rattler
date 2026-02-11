# Comprehensive benchmark script testing different shard counts
# Shows how SQLite optimizations scale with dataset size

$shard_counts = @(100, 500, 1000, 2000, 5000, 10000)
$results = @()

Write-Host "╔═══════════════════════════════════════════════════════════════════════════╗" -ForegroundColor Cyan
Write-Host "║           COMPREHENSIVE SCALABILITY BENCHMARK                             ║" -ForegroundColor Cyan
Write-Host "║           Testing File vs SQLite vs SQLite Optimized                      ║" -ForegroundColor Cyan
Write-Host "╚═══════════════════════════════════════════════════════════════════════════╝" -ForegroundColor Cyan
Write-Host ""

foreach ($count in $shard_counts) {
    Write-Host "`n═══════════════════════════════════════════════════════════════════════════" -ForegroundColor Green
    Write-Host "  Testing with $count shards" -ForegroundColor Green
    Write-Host "═══════════════════════════════════════════════════════════════════════════" -ForegroundColor Green

    $benchmark_dir = "E:/benchmark_scale_$count"

    # Run the benchmark
    $output = cargo run --release --bin shard-bench -- --shard-count $count --benchmark-dir $benchmark_dir 2>&1 | Out-String

    # Extract write performance for each storage type
    if ($output -match "File Storage:\s+(\d+\.?\d*)ms\s+\(\s*(\d+\.?\d*)\s+MB/s\)") {
        $file_time = $matches[1]
        $file_throughput = $matches[2]
    }

    # Find all SQLite results (baseline and optimized)
    $sqlite_matches = [regex]::Matches($output, "SQLite.*?(\d+\.?\d*)ms\s+\(\s*(\d+\.?\d*)\s+MB/s\)")
    if ($sqlite_matches.Count -ge 2) {
        $sqlite_time = $sqlite_matches[0].Groups[1].Value
        $sqlite_throughput = $sqlite_matches[0].Groups[2].Value
        $sqlite_opt_time = $sqlite_matches[1].Groups[1].Value
        $sqlite_opt_throughput = $sqlite_matches[1].Groups[2].Value
    }

    $result = [PSCustomObject]@{
        Shards = $count
        File_Time = $file_time
        File_Throughput = $file_throughput
        SQLite_Time = $sqlite_time
        SQLite_Throughput = $sqlite_throughput
        SQLite_Opt_Time = $sqlite_opt_time
        SQLite_Opt_Throughput = $sqlite_opt_throughput
        Speedup_vs_File = [math]::Round([double]$file_time / [double]$sqlite_opt_time, 2)
        Speedup_vs_Baseline = [math]::Round([double]$sqlite_time / [double]$sqlite_opt_time, 2)
    }

    $results += $result

    Write-Host "`nResults for $count shards:" -ForegroundColor Yellow
    Write-Host "  File:             ${file_time}ms (${file_throughput} MB/s)"
    Write-Host "  SQLite Baseline:  ${sqlite_time}ms (${sqlite_throughput} MB/s)"
    Write-Host "  SQLite Optimized: ${sqlite_opt_time}ms (${sqlite_opt_throughput} MB/s)"
    Write-Host "  Speedup vs File: $($result.Speedup_vs_File)x" -ForegroundColor Cyan
    Write-Host "  Speedup vs Baseline: $($result.Speedup_vs_Baseline)x" -ForegroundColor Cyan
}

Write-Host "`n`n╔═══════════════════════════════════════════════════════════════════════════╗" -ForegroundColor Magenta
Write-Host "║                        SCALABILITY SUMMARY                                ║" -ForegroundColor Magenta
Write-Host "╚═══════════════════════════════════════════════════════════════════════════╝" -ForegroundColor Magenta
Write-Host ""

$results | Format-Table -AutoSize

Write-Host "`n✓ Comprehensive benchmark complete!" -ForegroundColor Green
