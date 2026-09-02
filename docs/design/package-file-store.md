# Package extraction performance and the package file store

Status, 2 September 2026. Working notes from a day spent on extraction
performance and a content-addressed store for extracted package files. Two
parts have PRs; the store itself lives on this branch and is not finished.

- [#2748](https://github.com/conda/rattler/pull/2748) extracts packages on a
  blocking worker and fixes two per-file costs in the tar loop. Draft.
- [#2750](https://github.com/conda/rattler/pull/2750) hard links executables
  and native libraries on macOS instead of reflinking them. Draft.
- This branch adds `rattler_package_streaming::file_store` on top of #2748.

## Background

[#2031](https://github.com/conda/rattler/pull/2031) added a content-addressed
store for extracted files and found that extraction time doubled on Windows
for the `rust` package while saving 2% of disk space. uv shipped the same idea
in [astral-sh/uv#21327](https://github.com/astral-sh/uv/pull/21327) at under
4% overhead. The difference is where the work happens and how many filesystem
operations each file pays.

Per regular file, #2031 did roughly twelve filesystem operations inside the
serial tar loop: stat the store path, `create_dir_all` on a two-level shard
tree, `create_dir_all` on a temp directory, create a temp file, write, close,
rename into the store, hard link back, then chmod and set the mtime on the
link. Plain extraction does four. uv leaves the extraction loop untouched,
hashes with BLAKE3 in the write buffer, and runs one post-pass on a thread
pool that costs one hard link per new file. uv's own inline prototype was
2.5 to 3.4 times slower; the restructuring brought it to +4%.

#2031 also had three correctness problems that uv's design avoids: it
chmods and re-stamps the mtime on a hard link, which mutates the inode shared
with every other package; it keys objects by content only, so two packages
whose files share bytes but differ in mode collide; and a failed hard link
aborts extraction, which will fire on NTFS where a file may have at most 1023
links.

Other uv changes reviewed: #21372 (streaming extraction in one blocking
task, the basis of #2748), #21340 (buffer reuse, not applicable because
rattler hashes at the stream level), #21375 (link small files before writing,
folded into this branch), #21344 (bulk link counts for pruning, needed later),
#21324 (hard link executables, became #2750), #21334/#21333/#21338/#21339
(ways to learn ZIP file modes before the central directory; tar headers carry
the mode so rattler does not need them).

## What #2748 changed and why

The fully async extraction path used astral-tokio-tar and async-zip with a
thread-pool dispatch per filesystem call. It now pumps the download into a
channel of 128 KiB chunks and runs the existing synchronous extractors on a
`spawn_blocking` worker. Local files skip the channel.

That alone was slower on Windows. Two per-file costs in the `tar` crate had to
be fixed in rattler's own unpack loop before the sync path won everywhere:

1. `Entry::unpack_in` canonicalizes the parent and the destination for every
   entry. rattler now validates each parent directory once.
2. `Entry::unpack` copies file contents with 8 KiB writes. rattler now writes
   through a buffer sized to the file and sets the mtime through the open
   handle.

Wakeup granularity (duplex pipe versus full chunks) made no measurable
difference.

Cold extraction over loopback HTTP, hyperfine means, main versus #2748:

| Workload | Linux ext4 (WSL2) | Windows NTFS | Windows ReFS |
| --- | ---: | ---: | ---: |
| python 3.13 | 392 → 195 ms | 1.91 → 1.50 s | 0.55 → 0.32 s |
| numpy | 231 → 96 ms | 1.28 → 0.85 s | 0.35 → 0.19 s |
| rust (183 MiB) | 1.66 → 1.70 s | 2.36 → 2.49 s | |
| 5-package env, sequential | 2.33 → 1.99 s | 5.8 → 7.8 s | 2.75 → 2.71 s |
| 5-package env, concurrency 8 | 2.41 → 1.99 s | 6.4 → 7.9 s ± 3.4 | |

Local `.conda` files on NTFS improve as well (python 2.30 → 1.17 s, numpy
1.57 → 0.82 s) because local extraction already used the `tar` crate.

The sequential NTFS env number is not a code regression. Whichever package is
extracted second in the same process after a package with thousands of small
files takes one to two seconds longer than it does alone, in sync and async
mode alike, not in a fresh process a few seconds later, and not on ReFS or
Linux. That is the Defender filter catching up on the previous package's
files and throttling the process; a faster writer gets hit harder.

## The file store on this branch

`rattler_package_streaming::file_store` follows uv's shape:

- Files are extracted into the package directory as usual. Each regular file
  is hashed with BLAKE3 while it is written.
- After the archive is done, a post-pass on a rayon pool hard links every
  file into the store, grouped by shard so each shard directory is created
  once and linked by one worker. `hard_link(package_file, object)` is the
  existence probe: success means a new object; `AlreadyExists` means the
  private copy is replaced by a link to the object.
- Object identity is `derive_key("rattler package file store v1", blake3 ‖
  executable bit)`, sharded by the first two hex characters into 256
  directories. Mode and mtime are set while the file is still private and
  never touched afterwards.
- Files up to 64 KiB are hashed in memory and linked from an existing object
  before anything is written. The lookup is skipped when the object's shard
  directory did not exist when extraction started, so a cold store costs no
  failed link calls.
- `info/` is never shared, so package metadata can be rewritten in place.
- Nothing fails: a store that cannot be written, a cross-filesystem store, or
  a file that hits the link limit leaves a private copy and is counted as
  `unshared` in `FileStoreStats`.

It is reached through `ExtractOptions { file_store }` on the `_with_options`
variants of every extract function, `PackageCache::with_file_store`, and
`rattler extract --file-store <dir>`.

### Measurements

Loopback HTTP, hyperfine means, one binary with and without a store. "cold"
clears the store before every run, "warm" keeps it populated.

| | Linux: none / cold / warm | NTFS: none / cold / warm |
| --- | --- | --- |
| python 3.13 | 321 / 430 / 334 ms | 1.37 / 3.64 / 2.24 s |
| numpy | 169 / 240 / 160 ms | 0.93 / 1.66 / 1.55 s |
| rust | 2.75 / 8.0 ± 5.2 / 2.64 s | ~3.2 / 3.43 / 3.43 s |

Local files, Linux: python 266 / 296 / 250 ms, five-package env 2.43 / 2.62 /
2.82 s. Windows local: python 1.77 / 3.46 / 2.56 s, env 12.4 / 17.4 / 21.8 s.

Skipping the object lookup for shards that do not exist yet brought NTFS cold
python from 3.64 to 2.48 s and numpy from 1.66 to 1.54 s. A size threshold
below which files are not shared was set up but the runs were taken on a busy
machine and are not usable; the value in `min_shared_file_size` is a
placeholder.

What the numbers say:

- On Linux a cold store costs about a third on many-small-file packages and a
  warm store is neutral to slightly faster. The rust cold outlier needs a
  rerun.
- On NTFS a hard link costs about as much as writing the file, and system
  time goes up four to seven times. The store is a disk-space feature there,
  never a time win, and a warm store with large files is the worst case:
  each file is written in full and then replaced by a link, while Defender
  still holds the fresh file.

### Open items

1. Choose the minimum shared size from a quiet measurement (0, 4 KiB, 16 KiB
   were prepared). uv's cache analysis suggests files under 1 KiB carry under
   2% of the savings.
2. Warm large files: use `info/paths.json` (which precedes `pkg-*.tar.zst`
   in a `.conda`) as a hint, link the object for the listed SHA-256 before
   reading the entry, and verify the hash while draining the entry. That
   removes the write entirely on a warm store. Would move the key to SHA-256.
3. Pruning: remove objects whose link count is 1 during cache cleanup;
   `GetFileInformationByHandle` on Windows, `nlink` on unix, uv #21344 for a
   macOS bulk path. Needed before the store can be on by default.
4. Wire it into pixi behind a setting, off by default on Windows.
5. Windows only: consider sharing files above a larger threshold, since the
   per-link cost dominates and the savings are in large files.

## Methodology

Numbers come from hyperfine around `rattler extract`, which #2748 teaches to
take several packages, `--mode sync|async`, `--concurrency`, and to print the
time per package. Packages: conda-forge `rust-1.98.0-hf8d6059_0.conda`,
`python-3.13.15-h254dcb4_101_cp313.conda`, `numpy-2.5.2-py314hffb9209_1.conda`,
`pytweening-1.2.0-pyhd8ed1ab_1.conda`, `mamba-1.0.0-py38hecfeebb_2.tar.bz2`.
The HTTP runs serve that directory with `python -m http.server` on loopback,
which is where pixi's package cache reads from; local-file runs on NTFS carry
a Defender read tax that HTTP runs do not, so the two are not comparable.

```
hyperfine --shell=none --warmup 2 --runs 8 \
  --prepare '<clear the output dir, and the store for cold runs>' \
  -n main '<main binary> extract --mode async -d out http://127.0.0.1:8765/<pkg>' \
  -n new  '<new binary>  extract --mode async -d out http://127.0.0.1:8765/<pkg>'
```

Windows numbers must be taken on NTFS with Defender on; the E: dev drive is
ReFS with Defender in performance mode and shows a completely different
profile. Linux numbers are WSL2 Ubuntu 22.04 on ext4. Anything else running on
the machine, including a build in the WSL VM, moves NTFS numbers by 30% or
more, so only interleaved runs on an idle machine are comparable.
