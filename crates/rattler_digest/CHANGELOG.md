# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_digest-v0.18.0...rattler_digest-v0.19.0) - 2024-02-26

### Added
- compute hashes while extracting ([#176](https://github.com/baszalmstra/rattler/pull/176))
- added reading of conda-lock files ([#69](https://github.com/baszalmstra/rattler/pull/69))
- download and cache repodata.json ([#55](https://github.com/baszalmstra/rattler/pull/55))

### Fixed
- jlap wrong hash ([#390](https://github.com/baszalmstra/rattler/pull/390))
- ensure consistent sorting of locked packages

### Other
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- Add `read_package_file` function ([#472](https://github.com/baszalmstra/rattler/pull/472))
- more clippy lint ([#462](https://github.com/baszalmstra/rattler/pull/462))
- improve lockfile version mismatch error ([#423](https://github.com/baszalmstra/rattler/pull/423))
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- JLAP support ([#197](https://github.com/baszalmstra/rattler/pull/197))
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- re-export hash types from rattler_digest and use them more ([#137](https://github.com/baszalmstra/rattler/pull/137))
- inherit workspace properties for crates
- update rstest requirement from 0.16.0 to 0.17.0 ([#121](https://github.com/baszalmstra/rattler/pull/121))
- Feat/write conda lock ([#87](https://github.com/baszalmstra/rattler/pull/87))
