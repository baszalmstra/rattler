# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_virtual_packages-v0.18.0...rattler_virtual_packages-v0.19.0) - 2024-02-26

### Added
- use nvidia-smi on musl targets ([#290](https://github.com/baszalmstra/rattler/pull/290))
- alternative libc detection ([#209](https://github.com/baszalmstra/rattler/pull/209))
- make it possible to deserialize virtual packages ([#198](https://github.com/baszalmstra/rattler/pull/198))
- add win-arm64 as a known platform ([#93](https://github.com/baszalmstra/rattler/pull/93))
- add riscv ([#84](https://github.com/baszalmstra/rattler/pull/84))
- support installed (virtual) packages in libsolv ([#51](https://github.com/baszalmstra/rattler/pull/51))
- use nvml instead of libcuda to detect available cuda version
- detect virtual packages ([#34](https://github.com/baszalmstra/rattler/pull/34))

### Fixed
- clippy lints ([#470](https://github.com/baszalmstra/rattler/pull/470))
- allow compilation of android target ([#418](https://github.com/baszalmstra/rattler/pull/418))
- change urls from baszalmstra to mamba-org

### Other
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- Release
- Release
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
- use emscripten-wasm32 and wasi-wasm32 ([#333](https://github.com/baszalmstra/rattler/pull/333))
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- Merge remote-tracking branch 'upstream/main' into feat/normalize_package_names
- make rattler-package-streaming compile with wasm ([#287](https://github.com/baszalmstra/rattler/pull/287))
- Release
- Release
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- Release
- Release
- Release
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- update libloading requirement from 0.7.4 to 0.8.0 ([#162](https://github.com/baszalmstra/rattler/pull/162))
- Release
- inherit workspace properties for crates
- *(docs)* add deny missing docs to virtual packages ([#63](https://github.com/baszalmstra/rattler/pull/63))
- add licenses ([#37](https://github.com/baszalmstra/rattler/pull/37))
