# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_package_streaming-v0.18.0...rattler_package_streaming-v0.19.0) - 2024-02-26

### Added
- add `ProgressBar` trait and progress bar for package writing ([#525](https://github.com/baszalmstra/rattler/pull/525))
- make compression conversion functions `pub` ([#480](https://github.com/baszalmstra/rattler/pull/480))
- improve zstd compression ([#469](https://github.com/baszalmstra/rattler/pull/469))
- added experimental fields to conda lock ([#221](https://github.com/baszalmstra/rattler/pull/221))
- add rattler_networking and AuthenticatedClient to perform authenticated requests ([#191](https://github.com/baszalmstra/rattler/pull/191))
- compute hashes while extracting ([#176](https://github.com/baszalmstra/rattler/pull/176))
- now determines subdir ([#145](https://github.com/baszalmstra/rattler/pull/145))
- add ArchiveIdentifier type ([#65](https://github.com/baszalmstra/rattler/pull/65))
- download and cache repodata.json ([#55](https://github.com/baszalmstra/rattler/pull/55))
- install individual packages ([#48](https://github.com/baszalmstra/rattler/pull/48))
- package cache ([#42](https://github.com/baszalmstra/rattler/pull/42))
- validate package content ([#39](https://github.com/baszalmstra/rattler/pull/39))
- extract from path ([#38](https://github.com/baszalmstra/rattler/pull/38))
- async extraction methods ([#35](https://github.com/baszalmstra/rattler/pull/35))
- extract packages directly from urls ([#31](https://github.com/baszalmstra/rattler/pull/31))
- sync extraction of existing package formats ([#30](https://github.com/baszalmstra/rattler/pull/30))

### Fixed
- redaction ([#539](https://github.com/baszalmstra/rattler/pull/539))
- flaky package extract error ([#535](https://github.com/baszalmstra/rattler/pull/535))
- allow the full range of compression levels for zstd ([#479](https://github.com/baszalmstra/rattler/pull/479))
- clippy lints ([#470](https://github.com/baszalmstra/rattler/pull/470))
- support channel names with slashes ([#413](https://github.com/baszalmstra/rattler/pull/413))
- redact tokens from urls in errors ([#407](https://github.com/baszalmstra/rattler/pull/407))
- clippy warnings
- change urls from baszalmstra to mamba-org

### Other
- try to upgrade most dependencies ([#492](https://github.com/baszalmstra/rattler/pull/492))
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- Convert authenticated client to reqwest middleware ([#488](https://github.com/baszalmstra/rattler/pull/488))
- Release
- Release
- Add `read_package_file` function ([#472](https://github.com/baszalmstra/rattler/pull/472))
- Release
- more clippy lint ([#462](https://github.com/baszalmstra/rattler/pull/462))
- Release
- Release
- Release
- Release
- Release
- Release
- make builds deterministic ([#392](https://github.com/baszalmstra/rattler/pull/392))
- Release
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- Release
- Release
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- make rattler-package-streaming compile with wasm ([#287](https://github.com/baszalmstra/rattler/pull/287))
- Release
- Release
- Generate valid tar archives ([#276](https://github.com/baszalmstra/rattler/pull/276))
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- Release
- Release
- Release
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- expose reqwest tls features ([#179](https://github.com/baszalmstra/rattler/pull/179))
- Allow setting timestamp when writing archives ([#171](https://github.com/baszalmstra/rattler/pull/171))
- add url::Url to doctests
- change interface to use url directly and use Either for the stream
- add support for local files
- Release
- inherit workspace properties for crates
- add package writing functions ([#112](https://github.com/baszalmstra/rattler/pull/112))
- *(docs)* add deny missing docs to package streaming ([#64](https://github.com/baszalmstra/rattler/pull/64))
- add licenses ([#37](https://github.com/baszalmstra/rattler/pull/37))
