# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_repodata_gateway-v0.18.0...rattler_repodata_gateway-v0.19.0) - 2024-02-26

### Added
- implement seperate auth stores and allow using only disk auth ([#435](https://github.com/baszalmstra/rattler/pull/435))
- add channel priority and channel-specific selectors to solver info ([#394](https://github.com/baszalmstra/rattler/pull/394))
- add strict channel priority option ([#385](https://github.com/baszalmstra/rattler/pull/385))
- implement base_url cep ([#322](https://github.com/baszalmstra/rattler/pull/322))
- allow disabling jlap ([#327](https://github.com/baszalmstra/rattler/pull/327))
- spawn blocking JLAP patching operation ([#214](https://github.com/baszalmstra/rattler/pull/214))
- add rattler_networking and AuthenticatedClient to perform authenticated requests ([#191](https://github.com/baszalmstra/rattler/pull/191))
- enable downloading repodata_from_patches.json ([#109](https://github.com/baszalmstra/rattler/pull/109))
- extra methods to query spare repodata ([#110](https://github.com/baszalmstra/rattler/pull/110))
- allow caching repodata.json as .solv file ([#85](https://github.com/baszalmstra/rattler/pull/85))
- implement sparse repodata loading ([#89](https://github.com/baszalmstra/rattler/pull/89))
- install python entry points ([#83](https://github.com/baszalmstra/rattler/pull/83))
- download and cache repodata.json ([#55](https://github.com/baszalmstra/rattler/pull/55))

### Fixed
- redaction ([#539](https://github.com/baszalmstra/rattler/pull/539))
- clippy lints ([#470](https://github.com/baszalmstra/rattler/pull/470))
- re-download the repodata cache if is out of sync/corrupt ([#466](https://github.com/baszalmstra/rattler/pull/466))
- improve fetch error ([#426](https://github.com/baszalmstra/rattler/pull/426))
- set the blake2 hash nominal ([#411](https://github.com/baszalmstra/rattler/pull/411))
- make redaction work by using `From` explicitly ([#408](https://github.com/baszalmstra/rattler/pull/408))
- redact tokens from urls in errors ([#407](https://github.com/baszalmstra/rattler/pull/407))
- jlap wrong hash ([#390](https://github.com/baszalmstra/rattler/pull/390))
- make FetchRepoDataOptions clonable ([#321](https://github.com/baszalmstra/rattler/pull/321))
- ensure consistent sorting of locked packages
- clippy warnings
- small issues I encountered ([#184](https://github.com/baszalmstra/rattler/pull/184))
- sparse requires serde_json/raw_value ([#182](https://github.com/baszalmstra/rattler/pull/182))
- sort entries by package name in sparse index ([#111](https://github.com/baszalmstra/rattler/pull/111))
- sparse index returns duplicate records ([#94](https://github.com/baszalmstra/rattler/pull/94))
- repodata cache was always out of date ([#74](https://github.com/baszalmstra/rattler/pull/74))

### Other
- try to upgrade most dependencies ([#492](https://github.com/baszalmstra/rattler/pull/492))
- remove async_http_range_reader crate ([#505](https://github.com/baszalmstra/rattler/pull/505))
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- Convert authenticated client to reqwest middleware ([#488](https://github.com/baszalmstra/rattler/pull/488))
- Release
- Release
- Add `read_package_file` function ([#472](https://github.com/baszalmstra/rattler/pull/472))
- Release
- more clippy lint ([#462](https://github.com/baszalmstra/rattler/pull/462))
- Default value for conda_packages in repodata.json ([#441](https://github.com/baszalmstra/rattler/pull/441))
- Release
- add options to disable zstd and bz2 ([#420](https://github.com/baszalmstra/rattler/pull/420))
- Release
- Release
- Release
- Release
- Release
- Release
- match conda with `.state.json` -> `.info.json` ([#377](https://github.com/baszalmstra/rattler/pull/377))
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- Release
- add fetch repo data to py-rattler ([#334](https://github.com/baszalmstra/rattler/pull/334))
- Release
- json-patch 1.1.0 ([#332](https://github.com/baszalmstra/rattler/pull/332))
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- Merge remote-tracking branch 'upstream/main' into feat/normalize_package_names
- Release
- Release
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- add not found error to the fetch repo data errors  ([#256](https://github.com/baszalmstra/rattler/pull/256))
- propose a repodata patch function to be able to add `pip` to the `pyt… ([#238](https://github.com/baszalmstra/rattler/pull/238))
- Release
- Release
- Release
- JLAP support ([#197](https://github.com/baszalmstra/rattler/pull/197))
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- Change blake2 hash to use `blake2b` instead of `blake2s` ([#192](https://github.com/baszalmstra/rattler/pull/192))
- Adding documentation example, fixing typo ([#188](https://github.com/baszalmstra/rattler/pull/188))
- expose reqwest tls features ([#179](https://github.com/baszalmstra/rattler/pull/179))
- add support for local repodata
- update hex-literal requirement from 0.3.4 to 0.4.0 ([#149](https://github.com/baszalmstra/rattler/pull/149))
- update windows-sys requirement from 0.45.0 to 0.48.0 ([#150](https://github.com/baszalmstra/rattler/pull/150))
- Release
- inherit workspace properties for crates
- update rstest requirement from 0.16.0 to 0.17.0 ([#121](https://github.com/baszalmstra/rattler/pull/121))
- update tower-http requirement from 0.3.5 to 0.4.0 ([#76](https://github.com/baszalmstra/rattler/pull/76))
