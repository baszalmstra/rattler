# Package extraction performance and the package file store

Status, 4 September 2026. Working notes on extraction performance and a
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
in [astral-sh/uv#21327](https://github.com/astral-sh/uv/pull/21327) with about
3–4% overhead on cold, complete `uv pip install` benchmarks. That percentage
does not use the same denominator as the extraction-only benchmarks below:
uv includes the rest of installation, and its warm path normally reuses an
already-unpacked wheel instead of extracting it again. A direct benchmark of
both uv revisions and a forced-extraction warm-store mode is included below.

Per regular file, #2031 did roughly twelve filesystem operations inside the
serial tar loop: stat the store path, `create_dir_all` on a two-level shard
tree, `create_dir_all` on a temp directory, create a temp file, write, close,
rename into the store, hard link back, then chmod and set the mtime on the
link. Plain extraction does four. uv leaves the extraction loop untouched,
hashes with BLAKE3 in the write buffer, and runs one post-pass on a thread
pool that costs one hard link per new file. uv's own inline prototype was
2.5 to 3.4 times slower; its native-Linux benchmark reported +4% after the
restructuring.

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
  store. Targets are grouped in a 256-entry array by digest byte, avoiding
  path hashing. On Unix and ReFS, shard creation runs serially, largest shard
  first, while workers link shards that are already ready. NTFS creates all
  shards before linking because overlapping those metadata operations
  regresses there. Failure to identify the Windows filesystem takes the NTFS
  barrier. `hard_link(package_file, object)` is the existence probe:
  success means a new object; `AlreadyExists` means the private copy is
  replaced by a link to the object.
- Object identity is the hex BLAKE3 digest of the contents, with an `x`
  suffix for files with an executable bit, sharded by the first two hex
  characters into 256 directories. Mode and mtime are set while the file is
  still private and never touched afterwards. Package archive MD5 and
  SHA-256 verification is separate and unchanged.
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
- Existing shards are represented by a 256-bit set. Cold lookups test the raw
  digest byte before formatting an object path. Files linked during extraction
  update aggregate counters directly instead of retaining a path and digest
  for the publish pass.

It is reached through `ExtractOptions { file_store: Option<FileStore> }` on
the `_with_options` variants of every extract function,
`PackageCache::with_file_store(FileStore)`, and `rattler extract --file-store
<dir> [--file-store-min-size <bytes>]`. `FileStore::with_min_shared_size`
leaves files below a size private.

### Why the store uses BLAKE3, not `paths.json` SHA-256

The first implementation used SHA-256 so the digest in `info/paths.json`
could look up a large object before reading its contents. Those digests are
untrusted input, so the entry still had to be hashed and checked before
extraction could succeed. More importantly, the hint is unavailable when it
would matter: every conda-forge `.conda` package checked (python, numpy, rust,
pip, setuptools and wheel) stores `metadata.json`, then `pkg-*.tar.zst`, then
`info-*.tar.zst`. The metadata arrives after the payload it describes.
`.tar.bz2` packages put `info/` first, but retaining a second, slower object
identity for the legacy format is not worth its weight.

The store therefore follows uv and keys objects directly by BLAKE3. Small
files are hashed before writing. Large files are written normally, then
hashed from the page cache in the parallel publish pass. This removes all
`paths.json` parsing, speculative linking and verification from the store.
It also keeps per-file CAS hashing distinct from the package stream's MD5 and
SHA-256, which still authenticate and describe the complete archive.

Knowing a BLAKE3 digest before extraction could avoid large writes on a warm
store, but conda package metadata does not provide one. A SHA-256-to-BLAKE3
index or extra range requests would add persistent state and I/O to optimize
a path that has not justified that complexity.

### Measurements

Loopback HTTP, hyperfine means with one warmup and ten runs, one binary
with and without a store. "cold" clears the store before every run, "warm"
keeps it populated but still deletes and re-extracts the package directory.
ext4 is WSL2 on the same machine. Builds and other benchmark jobs were stopped
before each final group.

| | ext4 none / cold / warm | ReFS none / cold / warm | NTFS none / cold / warm |
| --- | --- | --- | --- |
| python 3.13 | 178 / 279 / 168 ms | 171 / 227 / 172 ms | 703 / 792 / 550 ms |
| numpy | 127 / 245 / 125 ms | 99 / 139 / 102 ms | 430 / 507 / 391 ms |
| rust | 843 / 877 / 920 ms | 860 / 925 / 999 ms | 1166 / 1364 / 1473 ms |

The fixed session cost was isolated by setting the minimum shared size above
every file. It was 0–5% on ext4 and indistinguishable from run-to-run noise on
ReFS and NTFS. The remaining overhead is hashing and filesystem mutation, not
store setup or bookkeeping.

The minimum shared size separates the two important workloads:

| | ext4 python | ext4 numpy | ReFS python | ReFS numpy | NTFS python | NTFS numpy |
| --- | --- | --- | --- | --- | --- | --- |
| disabled | 184 ms | 131 ms | 177 ms | 107 ms | 676 ms | 444 ms |
| 64 KiB, cold / warm | 219 / 185 ms | 166 / 142 ms | 186 / 191 ms | 106 / 115 ms | 690 / 857 ms | 445 / 576 ms |
| every file, cold / warm | 279 / 168 ms | 245 / 125 ms | 227 / 172 ms | 139 / 102 ms | 792 / 550 ms | 507 / 391 ms |

Python has 1,842 shareable files, NumPy 1,356, and rust only 147. Publishing
small files therefore dominates cold Python and NumPy: the second directory
entry requires one hard-link call per file, which no hashing or allocation
change can remove. On a warm store, early links avoid those writes and are
neutral or faster on every tested filesystem.

For rust, large-file hashing remains the material cost: 4% cold / 9% warm on
ext4, 8% / 16% on ReFS, and 17% / 26% on NTFS. BLAKE3's
`update_mmap_rayon` was tested and rejected; it raised the rust result to
973 / 981 ms on ext4 and 1159 / 1360 ms on ReFS. Hashing while writing was
worse still on ext4 at 1424 / 1418 ms because it serialized hashing with
decompression. Reading the freshly written files from the page cache in the
parallel post-pass remains the lowest measured design.

Compared with the previous BLAKE3 build, these absolute cold / warm results
improved from 282 / 176 to 279 / 168 ms for Python on ext4, from 146 / 107
to 139 / 102 ms for NumPy on ReFS, and from 1328 / 1463 to 1364 / 1473 ms
for rust on NTFS. The last result is a small regression; the large-file path
is unchanged on Windows and NTFS varies substantially under Defender, so this
does not justify changing the large-file hashing path.

The Windows publication schedule was subsequently measured in paired,
alternating cold-store runs. Overlapping serial shard creation with
per-shard link workers, matching uv's schedule, reduced the ReFS Python median
by 4.1% (95% bootstrap interval -7.0% to -1.2%) and the NumPy median by 2.4%
(-6.9% to +3.5%) over 30 rounds. The same schedule on NTFS was neutral for
Python at +0.3% (-5.7% to +5.9%) and NumPy at -0.5% (-5.0% to +3.4%) over 60
rounds. Rattler therefore selects the overlapping schedule only on ReFS.
The Unix path already used that schedule; an ext4 smoke benchmark remained
neutral at 297.2→296.2 ms for Python and 237.3→237.5 ms for NumPy.

#### Package linking

The production synchronous package linker used the directory barrier on every
filesystem and then applied `with_min_len(100)` to an iterator whose items are
whole destination-directory groups. For packages with fewer than a few hundred
groups, that minimum left most file linking serial. The installer now reuses
the file-store filesystem policy: Unix and ReFS create actual destination
directories parent-first and start each ready group immediately, while NTFS,
unknown Windows filesystems, and macOS retain the barrier. Actual destinations
include clobber paths under `__clobbers__`. A transaction-scoped context shares
completed directory creation between concurrently linked packages.

A warm package cache and fresh prefix isolated production `Installer` linking
for Python, NumPy, pip, setuptools, and wheel. Thirty paired alternating rounds
measured:

| Filesystem | Baseline median | New median | Median paired change | 95% bootstrap interval |
| --- | ---: | ---: | ---: | ---: |
| ext4 | 287.7 ms | 278.6 ms | -3.7% | -4.4% to -2.4% |
| ReFS | 476.3 ms | 105.8 ms | -79.5% | -90.4% to -65.2% |
| NTFS | 366.2 ms | 366.0 ms | +0.4% | -13.5% to +17.1% |

The ReFS samples contained large Defender outliers, but the paired median and
ratio-of-means bootstrap agree on the direction and size of the improvement.
Most of that gain comes from scheduling ready directory groups independently:
with fine-grained groups on both sides, adding overlap on ReFS was
inconclusive. On ext4, the corresponding barrier-only variant regressed by
2.4%, so the complete overlapping schedule is retained. NTFS remains on the
existing barrier and is unchanged within measurement noise.

Three deeper parallelism variants did not improve ReFS: publishing already
hashed small files while large-file hashing ran was 1.0% / 0.7% slower for
Python / NumPy, parallel object-path construction was +0.3% / -0.9%, and
limiting Rayon to eight threads was -1.7% / -0.3%. The percentage overhead
also exaggerates the remaining gap with uv because rattler's store-free
extraction is faster. The observed cold CAS deltas are already of the same
order: 101 / 118 ms on ext4, 56 / 40 ms on ReFS, and 89 / 77 ms on NTFS for
rattler's Python / NumPy packages, versus uv's 99 / 88 ms, 66 / 19 ms, and
151 / 62 ms for the SymPy / NumPy wheels. These workloads are not directly
comparable, but they do not show a general missing-publication-parallelism
bottleneck.

uv's published cold result uses the closest available cache state but a larger
denominator: complete `uv pip install` rather than extraction. Its published
warm result skips extraction by reusing the unpacked archive. The direct
comparison below adds a third state that retains only uv's file store and
therefore forces extraction into a fresh destination.

### uv comparator

The uv benchmark was rerun with profiling builds of the two revisions named in
its report: `a3f977ece` for the binary-only store and `85c3f7485` for the
optimized all-file store. Each sample installs one pinned local wheel
offline, without dependencies or bytecode compilation, through hard links into
a fresh target. The wheel page cache is warm. Results are medians from 30
paired, alternating rounds after three warmups; setup and cleanup are untimed.
The ext4 process was pinned to eight CPUs under WSL2. ReFS and NTFS used the
same Windows binaries and Python 3.12.13.

Three cache states separate the relevant work:

- **cold** removes the entire uv cache and the target;
- **store warm** retains only `files-v0`, forcing unzip, hashing, and
  publication against existing CAS objects;
- **archive warm** retains uv's complete extracted-wheel cache and removes
  only the target.

The table shows binary-only to all-file CAS medians and the relative change:

| wheel | ext4 cold / store warm / archive warm | ReFS cold / store warm / archive warm | NTFS cold / store warm / archive warm |
| --- | --- | --- | --- |
| AnyIO 4.9.0 | 51→69 ms (+35.7%) / 51→53 ms (+3.7%) / 15→15 ms (+0.1%) | 94→99 ms (+5.3%) / 182→191 ms (+5.3%) / 45→45 ms (-0.1%) | 136→149 ms (+9.2%) / 126→157 ms (+24.9%) / 32→32 ms (-0.2%) |
| SymPy 1.14.0 | 259→358 ms (+38.3%) / 257→262 ms (+2.2%) / 88→89 ms (+1.3%) | 420→486 ms (+15.8%) / 468→581 ms (+24.0%) / 118→115 ms (-2.9%) | 818→969 ms (+18.5%) / 831→1057 ms (+27.2%) / 291→291 ms (-0.0%) |
| NumPy 2.2.6 | 211→299 ms (+42.3%) / 206→207 ms (+0.8%) / 54→54 ms (-0.0%) | 282→301 ms (+6.7%) / 279→313 ms (+12.3%) / 120→121 ms (+1.1%) | 525→587 ms (+11.9%) / 514→648 ms (+26.1%) / 205→209 ms (+1.8%) |

On ext4, the paired 95% intervals for cold were 33.2–37.9%, 37.0–39.3%,
and 39.8–44.4%; for store warm they were 2.6–4.8%, 1.3–3.4%, and
-0.1–2.4%. A second ext4 run using fresh virtual environments and default link
mode, matching uv's stated setup more closely, still measured 35.4%, 37.0%,
and 40.9% cold. The difference from uv's published 3–4% is therefore not a
`--target` artifact. These ext4 runs used WSL2 rather than uv's native Linux
VM, so they establish host sensitivity rather than contradicting uv's result.
ReFS had high variance for SymPy, while the NTFS store-warm intervals
were 17.6–30.6%, 16.6–37.5%, and 20.3–31.5%.

The store-warm row is the useful comparison with rattler's warm extraction:
both retain file objects but force extraction into a new destination. uv's
ordinary archive-warm result is near zero overhead because it does not extract.
Absolute times are not comparable across the tools: wheels and conda packages
have different contents, and `uv pip install` still includes resolution and
installation work outside decompression and file-store publication.

### Open items

1. Pruning is not called from anywhere yet; the package cache's cleanup
   should call `FileStore::prune` after it removes package directories.
2. Wire it into pixi behind a setting. The measurements no longer argue for
   keeping it off on Windows; NTFS warm runs are faster with the store.
3. If repeated extraction of large packages remains common after integration,
   evaluate a package-level BLAKE3 manifest. It could supply digests before
   writing, but needs a trusted package identity and persistent invalidation;
   `info/paths.json` arrives too late and cannot provide that contract.

## Methodology

Numbers come from hyperfine around `rattler extract`, which #2748 teaches to
take several packages, `--mode sync|async`, `--concurrency`, and to print the
time per package. The store measurements use the win-64 packages
`rust-1.98.0-hf8d6059_0.conda` (176 files total, 147 shareable, 901 MiB
extracted),
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
