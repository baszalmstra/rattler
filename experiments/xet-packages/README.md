# Experiment: Xet-based conda packages

Question: can we add a conda package format on top of Hugging Face's
[Xet](https://huggingface.co/docs/xet) storage (chunk-level deduplication), so
that large packages that change little between versions (models, data,
big binaries) only move and store the changed bytes? And can we run the
server side ourselves, for example with [OpenXet](https://github.com/stubbi/openxet)?

Short answer: **it works technically, and it pays off a lot for data-like
packages (data files, jars, rebuilds of models), but not for compiled
binaries. Xet's per-chunk LZ4 compression makes a cold download of a binary
package up to 2x the size of today's `.conda`.** A Xet format is only worth
it as an *extra* path for data-heavy packages, next to `.conda`, not as a
replacement.

## What was built

`xet-packages-lab` (this directory, a standalone Cargo workspace that uses
the published `xet-data` / `xet-core-structures` 1.8.0 crates, the same code
`hf_xet` and OpenXet use):

- `analyze` extracts a series of real conda-forge packages (oldest first)
  with `rattler_package_streaming`, runs every file through Xet's
  content-defined chunker (Gearhash, 64 KiB target) and reports per package:
  - **cold**: what a first download costs in Xet's compressed chunk format;
  - **vs previous**: new bytes compared to the version directly before it
    (a client upgrading);
  - **vs all previous**: new bytes compared to all earlier versions (what
    the server stores, what an uploader sends);
  - the same at whole-file granularity, and for chunking the compressed
    `.conda` itself, to separate the gain of chunking from the gain of
    "skip unchanged files";
  - optionally (`--zstd-chunk-level`) what a zstd scheme per chunk would give.
- `roundtrip` pushes the extracted files through the real Xet upload
  pipeline (`FileUploadSession`, one session per package version) into a
  local CAS, then downloads every file of the last version by its Xet hash
  (`FileDownloadSession`) and checks the bytes and sha256.

`./run.sh` reproduces everything (downloads ~6.5 GiB). Raw output is in
[`results/`](results/), the series in [`series.txt`](series.txt).

## Results

Sizes as a percentage of the `.conda` / `.tar.bz2` that is downloaded today.
"Xet" is Xet's own per-chunk compression (LZ4 or byte-grouped LZ4).

| Series | Step | Unchanged chunks | Delta via Xet (% of archive) | Changed files (% of unpacked bytes) |
| --- | --- | ---: | ---: | ---: |
| `spacy-model-en_core_web_md` (model) | 3.3.0 build 0 → 1 (rebuild) | 99.8% | **0.1%** | 0.2% |
| | 3.3.0 → 3.4.0 (retrained) | 2% | 116% | 98% |
| | 3.4.0 → 3.5.0 | 75% | **35%** | 25% |
| | 3.5.0 → 3.8.0 (retrained) | 2% | 120% | 98% |
| `proj-data` (data, 800 MiB) | 1.24 → 1.25 | 95% | **5.4%** | 5.4% |
| | 1.25 build 0 → 1 (rebuild) | 100% | **0.0%** | 0.0% |
| `pyspark` (jars, 425 MiB) | 4.1.0 → 4.1.1 | 96% | **3.7%** | 22% |
| | 4.1.1 → 4.1.2 | 92% | **7.6%** | 27% |
| | 4.1.2 → 4.2.0 | 52% | 49% | 59% |
| `libcudnn` (CUDA binaries, 430 MiB) | 9.26.1 → 9.28.0 | 11% | 122% | 100% |
| | 9.28.0 → 9.28.1 | 76% | **25%** | 99% |
| `libllvm20` (shared lib, 41 MiB) | 20.1.5 → 20.1.6 | 34% | 165% | 99.8% |
| | 20.1.8 build 0 → 1 (rebuild) | 51% | 135% | 99.9% |
| `libtorch` CUDA (795 MiB) | 2.13.0 → 2.14.1 | 34% | 139% | 99% |
| | 2.14.1 build 301 → 302 (rebuild) | 68% | 62% | 99% |

Cold download (nothing local yet), Xet vs today's archive:

| Series | Cold Xet / archive | with zstd-9 per chunk |
| --- | ---: | ---: |
| proj-data | 98% | |
| pyspark | 103% | |
| spacy-model | 105–126% | |
| libcudnn | 129–132% | |
| libllvm20 | 204% | 134% |
| libtorch | 213–215% | 162% |

Server storage for the whole series (dedup across all versions):

| Series | Versions | `.conda` total | Xet store | Ratio |
| --- | ---: | ---: | ---: | ---: |
| proj-data | 3 | 2353 MiB | 783 MiB | **33%** |
| pyspark | 4 | 1698 MiB | 690 MiB | **41%** |
| spacy-model | 5 | 170 MiB | 114 MiB | 67% |
| libcudnn | 3 | 1295 MiB | 1193 MiB | 92% |
| libllvm20 | 5 | 207 MiB | 354 MiB | 171% |
| libtorch | 3 | 2370 MiB | 3260 MiB | 138% |

The real pipeline (`roundtrip`) matches the estimate within 1%: pyspark
4.1.0 uploads 440.7 MiB of xorbs + 0.56 MiB of shards (estimate 437.1),
4.1.1 uploads 16.4 MiB + 0.2 MiB. Dedup across separate upload sessions works
out of the box, and all 1026 files of pyspark 4.2.0 come back byte-identical.
Shard (metadata) overhead is 0.1–0.6 MiB per package.

## What we learned

1. **Chunking beats file-level dedup where it matters.** For pyspark a
   patch release changes 22–27% of the files but only 4–8% of the chunks
   (jars are zip files with mostly stored entries, so CDC finds the
   unchanged classes). For libcudnn and libtorch file-level dedup finds
   nothing (every `.so` changes) while chunks still find 34–76% overlap.
2. **Chunking today's `.conda` files is useless.** zstd output changes
   completely, so 90–100% of the archive chunks are new every time. The
   format has to store *uncompressed* file contents in Xet; Xet then
   compresses per chunk.
3. **Compression is the deal breaker for binaries.** conda-forge compresses
   with zstd-19 over the whole archive; Xet uses LZ4 per 64 KiB chunk. For
   machine code that is 2x bigger. Even a 50% overlap between versions does
   not make up for that, so for libllvm and libtorch Xet is worse than today
   on *every* path except a libtorch rebuild. A zstd scheme per chunk
   (not in the Xet spec, `CompressionScheme` only knows none/LZ4/BG4-LZ4)
   would close part of the gap (213% → 162%), not all of it: per-chunk
   compression loses the long-range context.
4. **Data, models and jars are a good fit.** Data that is already compressed
   (proj-data's GeoTIFFs, model weights) costs the same in Xet as in
   `.conda`, so every unchanged byte is pure profit: proj-data 1.24 → 1.25
   moves 43 MiB instead of 798 MiB, storage for three versions drops to a
   third.
5. **A retrained model is new data.** spacy 3.3 → 3.4 and 3.5 → 3.8 change
   98% of the bytes. Xet helps for rebuilds, metadata-only changes and models
   that are fine-tuned or partly frozen, not for full retrains. Models that
   pin the same weights across package versions (wrapper/code changes) are
   the sweet spot.
6. **Request count is manageable.** A libtorch upgrade has ~6000 runs of
   known/unknown chunks over 9330 files, but that is mostly a metadata
   question. Download plans can be fetched in batches
   (`POST /v1/reconstructions`, up to 1000 files per call in OpenXet), so
   about 10 calls for libtorch. The new chunks of a version are written
   contiguously into new xorbs (up to 64 MiB each) during upload, so a
   client that coalesces neighbouring ranges per xorb needs roughly one data
   request per new xorb, not one per run. Shards do not help on the download
   side: they are the upload/registration and global-dedup format, and
   their chunk offsets refer to the uncompressed xorb, while the compressed
   byte ranges come from the server's xorb layout. A static channel would
   publish one plan per package that already contains those ranges.
7. Small quirk: Xet reports an all-zero sha256 for empty files.

## Server side

- **OpenXet** ([stubbi/openxet](https://github.com/stubbi/openxet), Apache-2.0,
  no release yet, last commit 2026-11-05) implements the full CAS API
  (`/v1/reconstructions`, `/v2/reconstructions`, `/v1/chunks` global dedup,
  `/v1/xorbs`, `/v1/shards`) on top of the same `xet-core` crates, with
  scoped tokens, range-bound presigned URLs, GC with grace period and a Hub
  API. Storage today is local disk + SQLite; S3/GCS/Azure + Postgres are in
  progress (ADR 0015). A second, smaller implementation exists at
  [ggoggam/openxet](https://github.com/ggoggam/openxet).
  Building and running OpenXet was not allowed in this sandbox, so the
  roundtrip uses `xet-data`'s local CAS client instead; OpenXet's docs and
  code were read, not executed. The local CAS uses the same xorb and shard
  formats, so the upload/storage numbers carry over.
- **We may not need a Xet server at all for downloads.** A reconstruction is
  a small JSON document listing xorb byte ranges. Xorbs are immutable blobs
  that any CDN/S3 can serve with HTTP `Range`. A channel indexer
  (`rattler-index`) could chunk the uploaded `.conda` files itself, write
  xorbs and one precomputed reconstruction per package next to the
  repodata, and serve it all statically. Producers keep uploading `.conda`;
  no change to rattler-build or upload tooling. A dynamic CAS (OpenXet) is
  only needed if we want clients to *upload* deltas.
- **Stock `hf_xet` download dedup is not enough for conda.** It reuses its own
  chunk cache, not files already installed. For conda the client should
  index the chunks of packages in the rattler cache (chunk hash → file,
  offset) and only fetch missing ranges. The numbers in the "delta" column
  assume that.

## Possible design

1. Repodata gets an optional field per package, for example
   `"xet": {"manifest": "<xet hash>", "size": ...}`. Old clients ignore it
   and keep using `.conda`.
2. The manifest lists, per path in `paths.json`, the Xet file hash and size,
   plus the info files. Package identity (`sha256`/`md5` of the `.conda`) stays
   as it is; files are verified with their Xet (Merkle) hashes.
3. The client picks the cheapest path: if no earlier version of the package
   is in the cache, or the package is binary-heavy, download the `.conda`;
   otherwise fetch the reconstruction and only the missing xorb ranges, and
   copy the rest from the local cache. The channel can precompute "bytes to
   fetch vs version X" so the client does not have to guess.
4. Opt-in per package (or automatic based on measured overlap), so channels
   don't pay double storage for packages where Xet is worse.

## Pros and cons

Pros:
- 92–100% less transfer for patch releases and rebuilds of data, model and
  jar packages; storage for such series drops to 33–41%.
- Rebuilds (migrations, build-number bumps) of large noarch data/model
  packages become nearly free.
- Proven, open-source (Apache-2.0) building blocks in Rust; the chunker,
  hashing and xorb format can be used as a library today.
- Files are addressed and verified per file, which fits rattler's per-file
  cache and could enable partial/lazy installs.
- Self-hosting is possible (OpenXet), and downloads can even be served
  statically from a CDN.

Cons:
- 1.3–2.2x larger cold downloads and storage for binary packages because of
  LZ4 per chunk; the format is only a win for a subset of packages.
- Two formats per package doubles storage unless the channel picks per
  package; mirrors and caches (artifactory, proxies) have to learn a new
  layout.
- The `xet-*` crates have no semver promise (pin exact versions), and the
  wire protocol is Hugging Face's; extending it (zstd scheme) needs
  upstream agreement or a fork.
- Stock clients ask for one download plan per file and OpenXet signs one
  URL per byte range; rattler needs batched plans (or one plan per package)
  and range coalescing to keep the request count low.
- New security surface: per-file Merkle hashes instead of one archive hash,
  signing (sigstore attestations cover the `.conda` today), GC of shared
  xorbs across packages and channels.
- Client complexity: a local chunk index over the package cache, and
  fallback logic.

## Next steps, if we continue

- Measure on a model-in-a-package series where weights are reused across
  versions (for example a wrapper package around HF weights), and on
  fine-tuned model families.
- Prototype the static layout in `rattler-index` (xorbs + per-package
  reconstruction) and a fetcher in `rattler_cache` that uses local chunks.
- Run OpenXet with S3 behind it and test `hf_xet` and our fetcher against it.
- Discuss a zstd compression scheme with the xet-core maintainers.
