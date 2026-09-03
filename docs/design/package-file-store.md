# Package extraction performance and the package file store

Status, 3 September 2026. Working notes on extraction performance and a
content-addressed store for extracted package files. Two parts have PRs; the
store itself lives on this branch and is not wired into pixi yet.

- [#2748](https://github.com/conda/rattler/pull/2748) extracts packages on a
  blocking worker, hashes the package on the async pump and fixes two
  per-file costs in the tar loop.
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

- Files are extracted into the package directory as usual. A regular file
  up to 64 KiB is hashed in memory and linked from an existing object before
  anything is written; the lookup is skipped when the object's shard
  directory did not exist when extraction started, so a cold store costs no
  failed link calls. Larger files are written exactly like a plain
  extraction, without hashing on the extraction thread.
- After the archive is done, `FileStoreSession::publish` hashes the large
  files from disk on a rayon pool, then hard links every file into the
  store, grouped by shard so each shard directory is created once and linked
  by one worker. `hard_link(package_file, object)` is the existence probe:
  success means a new object; `AlreadyExists` means the private copy is
  replaced by a link to the object.
- Object identity is the hex SHA-256 of the contents, with an `x` suffix for
  files with an executable bit, sharded by the first two hex characters into
  256 directories. Mode and mtime are set while the file is still private and
  never touched afterwards.
- When `info/paths.json` is already on disk and the store has objects, a
  large file is looked up through the SHA-256 that `paths.json` records for
  it before its contents are read. On a hit the object is linked and the
  entry is only drained to verify that hash; a mismatch rejects the package
  (`InvalidData`) instead of linking wrong contents, and a size disagreement
  ignores the hint. See below for when this can actually fire.
- `info/` is never shared, so package metadata can be rewritten in place.
- Nothing fails: a store that cannot be written, a cross-filesystem store, or
  a file that hits the link limit leaves a private copy and is counted as
  `unshared` in `FileStoreStats`.
- `FileStore::prune` removes objects whose link count dropped to one and the
  shard directories that became empty. It is safe next to extractions: an
  object is only removed while no package links to it, and an extraction
  that loses the race writes the file and creates the object again. Link
  counts come from `nlink` on unix and `GetFileInformationByHandle` on
  Windows.

It is reached through `ExtractOptions { file_store: Option<FileStore> }` on
the `_with_options` variants of every extract function,
`PackageCache::with_file_store(FileStore)`, and `rattler extract --file-store
<dir> [--file-store-min-size <bytes>]`. `FileStore::with_min_shared_size`
leaves files below a size private.

### `paths.json` hints do not fire for streamed `.conda` files

The hint was planned on the assumption that `info-*.tar.zst` precedes
`pkg-*.tar.zst` in a `.conda`. It does not: every conda-forge package checked
(python, numpy, rust, pip, setuptools, wheel) stores `metadata.json`, then
`pkg-`, then `info-`, so while streaming a `.conda` the metadata arrives
after the files it describes. `.tar.bz2` packages do put `info/` first
(conda 22.9.0: entries 1 to 24), so the hint works for the legacy format and
for any caller that extracts the metadata first. For the streaming `.conda`
path, which is what the package cache uses, it would take one or two range
requests for the central directory and the info member before the download,
or a local seek for `.conda` files on disk. Neither is implemented.

This also settled where hashing happens. With the hint out of reach, the
SHA-256 of a large file is only needed after it is written, and hashing on
the extraction thread runs in series with decompression: it cost 330 ms of
the 900 MiB rust package on every filesystem. Hashing from disk in the
publish post-pass runs on all cores and reads from the page cache, which
removed most of that on ext4 and ReFS. On NTFS the read-back pays the
Defender tax instead (system time 495 → 935 ms), so there it is a wash.

### Measurements

Loopback HTTP, hyperfine means with one warmup and ten runs, one binary
with and without a store. "cold" clears the store before every run, "warm"
keeps it populated. ext4 is WSL2 on the same machine; all runs are on an
otherwise idle machine.

| | ext4 none / cold / warm | ReFS none / cold / warm | NTFS none / cold / warm |
| --- | --- | --- | --- |
| python 3.13 | 169 / 277 / 169 ms | 170 / 234 / 185 ms | 654 / 741 / 516 ms |
| numpy | 124 / 224 / 123 ms | 102 / 147 / 109 ms | 410 / 503 / 392 ms |
| rust | 834 / 945 / 938 ms | 854 / 979 / 1021 ms | 1006 / 1320 / 1438 ms |

The rust package across the three hashing designs, cold / warm:

| | ext4 | ReFS | NTFS |
| --- | --- | --- | --- |
| BLAKE3 on the extraction thread | 1009 / 978 ms | | |
| SHA-256 on the extraction thread | 1196 / 1230 ms | 1179 / 1216 ms | 1382 / 1491 ms |
| SHA-256 in the post-pass | 945 / 938 ms | 979 / 1021 ms | 1320 / 1438 ms |

The minimum shared size, python and numpy, cold / warm:

| | ext4 python | ext4 numpy | NTFS python | NTFS numpy |
| --- | --- | --- | --- | --- |
| 0 | 283 / 174 ms | 229 / 127 ms | 741 / 516 ms | 503 / 392 ms |
| 4 KiB | 277 / 183 ms | 214 / 138 ms | 740 / 631 ms | 599 / 498 ms |
| 16 KiB | 264 / 193 ms | 209 / 137 ms | | |
| 64 KiB | 218 / 189 ms | 164 / 136 ms | 684 / 899 ms | 438 / 599 ms |

What the numbers say:

- A warm store is neutral on ext4 and ReFS for many-small-file packages and
  a win on NTFS: python 654 → 516 ms, because a hard link is cheaper than a
  write that Defender inspects. The earlier note that the store could never
  be a time win on NTFS predates hashing on the pump and the post-pass.
- A cold store costs about 100 ms on python and numpy on every filesystem.
  That is the link call per file; hashing small files in memory is not
  visible.
- Large files: cold overhead is now 13% on ext4, 15% on ReFS and 31% on
  NTFS for the rust package. Warm is no better than cold because a file
  above 64 KiB is still written before it is replaced by a link. Only the
  `paths.json` hint avoids that write, and it needs the metadata first.
- Not sharing small files buys a little cold time and loses more warm time,
  on NTFS a lot (python warm 516 → 899 ms at 64 KiB), because every file
  that is not linked has to be written. The default shares every file.
- Removing the verification hash from the hinted path changed nothing,
  because the hint never fired for `.conda`; the scratch build was
  discarded.

### Open items

1. Hints for streamed `.conda` packages: fetch the central directory and
   the `info-` member by range request before streaming, only when the
   store has objects, and hand the parsed `paths.json` to the session. Local
   `.conda` files can seek instead. This is the only way to skip the write
   of a large file on a warm store.
2. Pruning is not called from anywhere yet; the package cache's cleanup
   should call `FileStore::prune` after it removes package directories.
3. Wire it into pixi behind a setting. The measurements no longer argue for
   keeping it off on Windows; NTFS warm runs are faster with the store.
4. `SMALL_FILE_LIMIT` (64 KiB) is untested as a knob; files between 64 KiB
   and a few MiB might be worth hashing in memory too, since that is the
   only path that avoids the write on a warm store without hints.

## Methodology

Numbers come from hyperfine around `rattler extract`, which #2748 teaches to
take several packages, `--mode sync|async`, `--concurrency`, and to print the
time per package. The store measurements use the win-64 packages
`rust-1.98.0-hf8d6059_0.conda` (176 files, 901 MiB extracted),
`python-3.13.7-hdf00ec1_100_cp313.conda` and
`numpy-2.5.2-py313ha8dc839_1.conda`, on every filesystem, so the trees are
identical across platforms. The earlier #2748 table used
`python-3.13.15-h254dcb4_101_cp313.conda`, `numpy-2.5.2-py314hffb9209_1.conda`,
`pytweening-1.2.0-pyhd8ed1ab_1.conda` and `mamba-1.0.0-py38hecfeebb_2.tar.bz2`.
The HTTP runs serve that directory with `python -m http.server` on loopback,
which is where pixi's package cache reads from; local-file runs on NTFS carry
a Defender read tax that HTTP runs do not, so the two are not comparable.

```
hyperfine --warmup 1 --runs 10 \
  --prepare '<clear the output dir, and the store for cold runs>' \
  -n python/none '<binary> extract --mode async --destination out http://127.0.0.1:8765/<pkg>' \
  -n python/cold '<binary> extract --mode async --destination out --file-store store http://127.0.0.1:8765/<pkg>'
```

Windows numbers must be taken on NTFS with Defender on; the E: dev drive is
ReFS with Defender in performance mode and shows a completely different
profile. Linux numbers are WSL2 Ubuntu 22.04 on ext4. Anything else running on
the machine, including a build in the WSL VM, moves NTFS numbers by 30% or
more, so only interleaved runs on an idle machine are comparable.
