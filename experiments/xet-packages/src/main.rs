//! Experiment: what would Xet chunk-level deduplication buy conda packages?
//!
//! Two subcommands:
//!
//! * `analyze` extracts a series of package archives (oldest first), splits every
//!   file with Xet's content defined chunker (the exact chunker `hf_xet` uses) and
//!   reports, per package, how many bytes are new compared to the previous version
//!   and compared to all earlier versions. It also reports what the same analysis
//!   gives on the compressed archive bytes and at whole-file granularity, so the
//!   value of chunking can be separated from the value of simply not re-downloading
//!   unchanged files.
//! * `roundtrip` pushes the extracted packages through the real Xet upload pipeline
//!   (`xet-data`'s `FileUploadSession`) into a local CAS, one upload session per
//!   package version, then downloads every file of the last version again with
//!   `FileDownloadSession` and verifies the bytes. This gives the real storage and
//!   upload numbers, including xorb framing and shard metadata.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use xet_core_structures::merklehash::{MerkleHash, file_hash};
use xet_core_structures::xorb_object::CompressionScheme;
use xet_data::deduplication::Chunker;
use xet_data::processing::configurations::TranslatorConfig;
use xet_data::processing::{FileDownloadSession, FileUploadSession, Sha256Policy, XetFileInfo};
use xet_runtime::core::XetContext;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Chunk a series of packages and report dedup statistics.
    Analyze {
        /// Name of the series, used in the output.
        #[arg(long)]
        name: String,
        /// Write the results as JSON to this file.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Also compress every new chunk on its own with zstd at this level, to see what a
        /// zstd compression scheme in xorbs would give (slow for high levels).
        #[arg(long)]
        zstd_chunk_level: Option<i32>,
        /// Package archives (.conda or .tar.bz2), oldest first.
        packages: Vec<PathBuf>,
    },
    /// Upload a series through the real Xet pipeline into a local CAS and download the
    /// last package again.
    Roundtrip {
        #[arg(long)]
        name: String,
        /// Directory for the local CAS (created if missing; reuse it to dedup across runs).
        #[arg(long)]
        cas: PathBuf,
        #[arg(long)]
        json: Option<PathBuf>,
        packages: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Analyze {
            name,
            json,
            zstd_chunk_level,
            packages,
        } => analyze(&name, &packages, json.as_deref(), zstd_chunk_level),
        Command::Roundtrip {
            name,
            cas,
            json,
            packages,
        } => roundtrip(&name, &cas, &packages, json.as_deref()),
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Extract a package archive into a fresh temporary directory.
fn extract(archive: &Path) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new().prefix("xet-lab-").tempdir_in(
        std::env::var_os("XET_LAB_TMP").map_or_else(std::env::temp_dir, PathBuf::from),
    )?;
    rattler_package_streaming::fs::extract(archive, dir.path())
        .with_context(|| format!("extracting {}", archive.display()))?;
    Ok(dir)
}

/// All regular files below `root`, sorted, as (relative path, absolute path).
fn regular_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut files: Vec<_> = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (rel, e.into_path())
        })
        .collect();
    files.sort();
    files
}

/// Stream `reader` through the Xet chunker and call `on_chunk` for each chunk.
/// Returns the (hash, size) list, from which the Xet file hash is derived.
fn chunk_stream(
    mut reader: impl Read,
    mut on_chunk: impl FnMut(&MerkleHash, &[u8]),
) -> Result<Vec<(MerkleHash, u64)>> {
    let mut chunker = Chunker::default();
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let mut out = Vec::new();
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for c in chunker.next_block(&buf[..n], false) {
            on_chunk(&c.hash, &c.data);
            out.push((c.hash, c.data.len() as u64));
        }
    }
    if let Some(c) = chunker.finish() {
        on_chunk(&c.hash, &c.data);
        out.push((c.hash, c.data.len() as u64));
    }
    Ok(out)
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

// ---------------------------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------------------------

#[derive(Default, Serialize, Clone)]
struct PackageStats {
    package: String,
    /// Size of the .conda / .tar.bz2 as downloaded today.
    archive_bytes: u64,
    files: u64,
    unpacked_bytes: u64,
    chunks: u64,
    /// Unique chunk bytes within this package (intra package dedup).
    unique_bytes: u64,
    /// Unique chunks of this package compressed with Xet's per-chunk compression
    /// (what a cold download of this package costs in xorb bytes).
    unique_xet_compressed: u64,

    /// Same as `unique_xet_compressed` but with per-chunk zstd (only with --zstd-chunk-level).
    unique_zstd_chunk: u64,

    /// Compared against the directly preceding package (a client upgrading).
    prev_new_bytes: u64,
    prev_new_xet_compressed: u64,
    prev_new_zstd3: u64,
    prev_new_zstd_chunk: u64,
    prev_new_file_level_bytes: u64,
    prev_new_archive_chunk_bytes: u64,

    /// Compared against all preceding packages (server storage / uploader).
    all_new_bytes: u64,
    all_new_xet_compressed: u64,
    all_new_file_level_bytes: u64,
    all_new_archive_chunk_bytes: u64,

    /// Number of contiguous runs of known/unknown chunks vs. the previous package,
    /// a proxy for the number of reconstruction terms/range requests.
    prev_runs: u64,
}

#[derive(Default)]
struct Seen {
    chunks: HashSet<MerkleHash>,
    files: HashSet<MerkleHash>,
    archive_chunks: HashSet<MerkleHash>,
}

fn analyze(
    name: &str,
    packages: &[PathBuf],
    json: Option<&Path>,
    zstd_chunk_level: Option<i32>,
) -> Result<()> {
    let mut all = Seen::default();
    let mut prev = Seen::default();
    let mut results = Vec::new();

    for (idx, pkg) in packages.iter().enumerate() {
        let start = Instant::now();
        let mut cur = Seen::default();
        let mut s = PackageStats {
            package: pkg.file_name().unwrap().to_string_lossy().into_owned(),
            archive_bytes: std::fs::metadata(pkg)?.len(),
            ..Default::default()
        };

        // 1. Chunk the compressed archive as-is (what you'd get by putting today's
        //    .conda files into Xet unchanged).
        chunk_stream(File::open(pkg)?, |h, d| {
            if cur.archive_chunks.insert(*h) {
                if !prev.archive_chunks.contains(h) {
                    s.prev_new_archive_chunk_bytes += d.len() as u64;
                }
                if !all.archive_chunks.contains(h) {
                    s.all_new_archive_chunk_bytes += d.len() as u64;
                }
            }
        })?;

        // 2. Chunk every file of the extracted package.
        let dir = extract(pkg)?;
        let mut zstd_prev_new = zstd::Encoder::new(CountingSink::default(), 3)?;
        let mut last_known: Option<bool> = None;
        for (_rel, path) in regular_files(dir.path()) {
            s.files += 1;
            let mut file_new_prev = 0u64;
            let mut file_new_all = 0u64;
            let mut file_size = 0u64;
            let mut zerr = None;
            let hashes = chunk_stream(File::open(&path)?, |h, d| {
                let len = d.len() as u64;
                s.chunks += 1;
                file_size += len;
                let known_prev = prev.chunks.contains(h);
                if last_known != Some(known_prev) {
                    s.prev_runs += 1;
                    last_known = Some(known_prev);
                }
                if cur.chunks.insert(*h) {
                    s.unique_bytes += len;
                    let compressed = CompressionScheme::Auto
                        .compress_from_slice(d)
                        .map_or(len, |c| c.len().min(d.len()) as u64);
                    s.unique_xet_compressed += compressed;
                    let zchunk = zstd_chunk_level.map_or(0, |lvl| {
                        zstd::bulk::compress(d, lvl).map_or(len, |c| (c.len() as u64).min(len))
                    });
                    s.unique_zstd_chunk += zchunk;
                    if !known_prev {
                        s.prev_new_zstd_chunk += zchunk;
                        file_new_prev += len;
                        s.prev_new_xet_compressed += compressed;
                        if let Err(e) = zstd_prev_new.write_all(d) {
                            zerr = Some(e);
                        }
                    }
                    if !all.chunks.contains(h) {
                        file_new_all += len;
                        s.all_new_xet_compressed += compressed;
                    }
                }
            })?;
            if let Some(e) = zerr {
                return Err(e.into());
            }
            s.unpacked_bytes += file_size;
            s.prev_new_bytes += file_new_prev;
            s.all_new_bytes += file_new_all;

            let fh = file_hash(&hashes);
            if cur.files.insert(fh) {
                if !prev.files.contains(&fh) {
                    s.prev_new_file_level_bytes += file_size;
                }
                if !all.files.contains(&fh) {
                    s.all_new_file_level_bytes += file_size;
                }
            }
        }
        s.prev_new_zstd3 = zstd_prev_new.finish()?.0;
        drop(dir);

        eprintln!(
            "[{name}] {} analysed in {:.1}s",
            s.package,
            start.elapsed().as_secs_f64()
        );
        print_row(idx, &s);

        all.chunks.extend(cur.chunks.iter().copied());
        all.files.extend(cur.files.iter().copied());
        all.archive_chunks
            .extend(cur.archive_chunks.iter().copied());
        prev = cur;
        results.push(s);
    }

    if let Some(path) = json {
        let doc = serde_json::json!({ "series": name, "packages": results });
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(())
}

fn print_row(idx: usize, s: &PackageStats) {
    println!("{}", s.package);
    println!(
        "  archive {:8.1} MiB | unpacked {:8.1} MiB in {} files, {} chunks | cold xet download {:8.1} MiB ({:.0}% of archive)",
        mib(s.archive_bytes),
        mib(s.unpacked_bytes),
        s.files,
        s.chunks,
        mib(s.unique_xet_compressed),
        pct(s.unique_xet_compressed, s.archive_bytes),
    );
    if s.unique_zstd_chunk > 0 {
        println!(
            "  per-chunk zstd: cold {:8.1} MiB ({:.0}% of archive), new vs previous {:8.1} MiB ({:.1}% of archive)",
            mib(s.unique_zstd_chunk),
            pct(s.unique_zstd_chunk, s.archive_bytes),
            mib(s.prev_new_zstd_chunk),
            pct(s.prev_new_zstd_chunk, s.archive_bytes),
        );
    }
    if idx == 0 {
        return;
    }
    println!(
        "  vs previous: new chunk data {:8.1} MiB ({:5.1}% of unpacked) -> xet-compressed {:8.1} MiB ({:5.1}% of archive), zstd3 {:8.1} MiB | file-level {:8.1} MiB ({:5.1}%) | chunked archive {:5.1}% new | {} runs",
        mib(s.prev_new_bytes),
        pct(s.prev_new_bytes, s.unpacked_bytes),
        mib(s.prev_new_xet_compressed),
        pct(s.prev_new_xet_compressed, s.archive_bytes),
        mib(s.prev_new_zstd3),
        mib(s.prev_new_file_level_bytes),
        pct(s.prev_new_file_level_bytes, s.unpacked_bytes),
        pct(s.prev_new_archive_chunk_bytes, s.archive_bytes),
        s.prev_runs,
    );
    println!(
        "  vs all prev: new chunk data {:8.1} MiB ({:5.1}% of unpacked) -> xet-compressed {:8.1} MiB ({:5.1}% of archive) | file-level {:5.1}% | chunked archive {:5.1}% new",
        mib(s.all_new_bytes),
        pct(s.all_new_bytes, s.unpacked_bytes),
        mib(s.all_new_xet_compressed),
        pct(s.all_new_xet_compressed, s.archive_bytes),
        pct(s.all_new_file_level_bytes, s.unpacked_bytes),
        pct(s.all_new_archive_chunk_bytes, s.archive_bytes),
    );
}

/// A `Write` sink that only counts bytes.
#[derive(Default)]
struct CountingSink(u64);

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// roundtrip
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct RoundtripStats {
    package: String,
    archive_bytes: u64,
    files: u64,
    total_bytes: u64,
    new_bytes: u64,
    deduped_bytes: u64,
    deduped_bytes_by_global_dedup: u64,
    xorb_bytes_uploaded: u64,
    shard_bytes_uploaded: u64,
    total_bytes_uploaded: u64,
    upload_seconds: f64,
    /// Xet file hashes of every file, keyed by relative path. This is what a package
    /// "manifest" in this format would carry next to `paths.json`.
    #[serde(skip)]
    manifest: BTreeMap<String, XetFileInfo>,
}

fn roundtrip(name: &str, cas: &Path, packages: &[PathBuf], json: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(cas)?;
    let ctx = XetContext::default()?;
    let cas = cas.to_path_buf();
    let packages = packages.to_vec();
    let name = name.to_string();
    let json = json.map(Path::to_path_buf);

    ctx.runtime.clone().bridge_sync(async move {
        let mut results: Vec<RoundtripStats> = Vec::new();
        let mut last_extracted = None;

        for pkg in &packages {
            let dir = extract(pkg)?;
            let files = regular_files(dir.path());
            let start = Instant::now();

            // One upload session per package, like one `rattler-build upload` call.
            let config = Arc::new(TranslatorConfig::local_config(&ctx, &cas)?);
            let session = FileUploadSession::new(config).await?;
            let infos = session
                .upload_files(files.iter().map(|(_, p)| (p.clone(), Sha256Policy::Compute)))
                .await?;
            let metrics = session.finalize().await?;

            let manifest: BTreeMap<_, _> = files
                .iter()
                .map(|(rel, _)| rel.clone())
                .zip(infos)
                .collect();

            let s = RoundtripStats {
                package: pkg.file_name().unwrap().to_string_lossy().into_owned(),
                archive_bytes: std::fs::metadata(pkg)?.len(),
                files: files.len() as u64,
                total_bytes: metrics.total_bytes,
                new_bytes: metrics.new_bytes,
                deduped_bytes: metrics.deduped_bytes,
                deduped_bytes_by_global_dedup: metrics.deduped_bytes_by_global_dedup,
                xorb_bytes_uploaded: metrics.xorb_bytes_uploaded,
                shard_bytes_uploaded: metrics.shard_bytes_uploaded,
                total_bytes_uploaded: metrics.total_bytes_uploaded,
                upload_seconds: start.elapsed().as_secs_f64(),
                manifest,
            };
            println!(
                "[{name}] {}: {:.1} MiB unpacked, uploaded {:.1} MiB xorbs + {:.2} MiB shards = {:.1}% of archive ({:.1} MiB) in {:.1}s",
                s.package,
                mib(s.total_bytes),
                mib(s.xorb_bytes_uploaded),
                mib(s.shard_bytes_uploaded),
                pct(s.total_bytes_uploaded, s.archive_bytes),
                mib(s.archive_bytes),
                s.upload_seconds,
            );
            results.push(s);
            last_extracted = Some((dir, files));
        }

        // Download every file of the last package by its Xet hash and verify it.
        let Some((dir, files)) = last_extracted else {
            return Ok(());
        };
        let last = results.last().unwrap();
        let out = tempfile::tempdir_in(&cas)?;
        let config = Arc::new(TranslatorConfig::local_config(&ctx, &cas)?);
        let download = FileDownloadSession::new(config, None).await?;
        let start = Instant::now();
        let mut bytes = 0u64;
        for (rel, src) in &files {
            let info = &last.manifest[rel];
            let dst = out.path().join(rel);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            let (_, n) = download.download_file(info, &dst).await?;
            bytes += n;
            if sha256_file(src)? != sha256_file(&dst)? {
                bail!("{rel}: downloaded content differs from the original");
            }
            // Note: xet reports an all-zero sha256 for empty files, so skip those.
            if let Some(expected) = info.sha256.as_ref().filter(|_| n > 0)
                && *expected != hex(&sha256_file(&dst)?) {
                    bail!("{rel}: sha256 in manifest does not match: {expected} vs {}", hex(&sha256_file(&dst)?));
                }
        }
        download.finalize().await?;
        println!(
            "[{name}] downloaded and verified {} files ({:.1} MiB) of {} in {:.1}s",
            files.len(),
            mib(bytes),
            last.package,
            start.elapsed().as_secs_f64()
        );
        drop(dir);

        if let Some(path) = json {
            let doc = serde_json::json!({ "series": name, "packages": results });
            std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        }
        Ok::<_, anyhow::Error>(())
    })??;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut h = Sha256::new();
    std::io::copy(&mut File::open(path)?, &mut h)?;
    Ok(h.finalize().into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
