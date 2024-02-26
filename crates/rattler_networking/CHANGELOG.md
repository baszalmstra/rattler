# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_networking-v0.18.0...rattler_networking-v0.19.0) - 2024-02-26

### Added
- add support for netrc files as a secondary fallback tier ([#395](https://github.com/baszalmstra/rattler/pull/395))
- refactor `Version.bump()` to accept bumping `major/minor/patch/last` ([#452](https://github.com/baszalmstra/rattler/pull/452))
- implement seperate auth stores and allow using only disk auth ([#435](https://github.com/baszalmstra/rattler/pull/435))
- add rattler_networking and AuthenticatedClient to perform authenticated requests ([#191](https://github.com/baszalmstra/rattler/pull/191))

### Fixed
- redaction ([#539](https://github.com/baszalmstra/rattler/pull/539))
- depend on derive feature of serde
- netrc parsing into BasicAuth ([#506](https://github.com/baszalmstra/rattler/pull/506))
- return Ok(None) on Err(keyring::Error::NoEntry) ([#474](https://github.com/baszalmstra/rattler/pull/474))
- fix wildcard expansion for stored credentials of domains ([#442](https://github.com/baszalmstra/rattler/pull/442))
- use filelock for authentication fallback storage  ([#427](https://github.com/baszalmstra/rattler/pull/427))
- make redaction work by using `From` explicitly ([#408](https://github.com/baszalmstra/rattler/pull/408))
- redact tokens from urls in errors ([#407](https://github.com/baszalmstra/rattler/pull/407))
- add retry behavior for package cache downloads ([#280](https://github.com/baszalmstra/rattler/pull/280))

### Other
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- add basic test to validate that we can parse .netrc files properly ([#503](https://github.com/baszalmstra/rattler/pull/503))
- Convert authenticated client to reqwest middleware ([#488](https://github.com/baszalmstra/rattler/pull/488))
- Move async http range reader crate into rattler-networking ([#482](https://github.com/baszalmstra/rattler/pull/482))
- more clippy lint ([#462](https://github.com/baszalmstra/rattler/pull/462))
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- simplify code and change warning to debug ([#365](https://github.com/baszalmstra/rattler/pull/365))
- Fix/auth fallback ([#347](https://github.com/baszalmstra/rattler/pull/347))
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- make rattler-package-streaming compile with wasm ([#287](https://github.com/baszalmstra/rattler/pull/287))
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- also check if credentials stored under wildcard host ([#252](https://github.com/baszalmstra/rattler/pull/252))
