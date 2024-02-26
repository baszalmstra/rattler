# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.1](https://github.com/baszalmstra/rattler/compare/rattler-v0.19.0...rattler-v0.19.1) - 2024-02-26

### Added
- expose `get_windows_launcher` function ([#478](https://github.com/baszalmstra/rattler/pull/478))
- better detection of hardlinks and fallback to copy ([#461](https://github.com/baszalmstra/rattler/pull/461))
- added experimental fields to conda lock ([#221](https://github.com/baszalmstra/rattler/pull/221))
- add rattler_networking and AuthenticatedClient to perform authenticated requests ([#191](https://github.com/baszalmstra/rattler/pull/191))
- compute hashes while extracting ([#176](https://github.com/baszalmstra/rattler/pull/176))
- prepare for release ([#119](https://github.com/baszalmstra/rattler/pull/119))
- install python entry points ([#83](https://github.com/baszalmstra/rattler/pull/83))
- create command ([#72](https://github.com/baszalmstra/rattler/pull/72))
- can now install python from lock
- support installed (virtual) packages in libsolv ([#51](https://github.com/baszalmstra/rattler/pull/51))
- add ArchiveIdentifier type ([#65](https://github.com/baszalmstra/rattler/pull/65))
- get prefix paths from linking ([#62](https://github.com/baszalmstra/rattler/pull/62))
- download and cache repodata.json ([#55](https://github.com/baszalmstra/rattler/pull/55))
- add PrefixRecord and RepoDataRecord ([#56](https://github.com/baszalmstra/rattler/pull/56))
- rustdoc ([#59](https://github.com/baszalmstra/rattler/pull/59))
- adds the PackageFile trait for consistency ([#58](https://github.com/baszalmstra/rattler/pull/58))
- expose link module
- added function that generalizes replacement
- expose linking functions
- install individual packages ([#48](https://github.com/baszalmstra/rattler/pull/48))
- package cache ([#42](https://github.com/baszalmstra/rattler/pull/42))
- validate package content ([#39](https://github.com/baszalmstra/rattler/pull/39))
- move all conda types to seperate crate
- data models for extracting channel information ([#14](https://github.com/baszalmstra/rattler/pull/14))
- adds libsolv solver ([#13](https://github.com/baszalmstra/rattler/pull/13))
- solver crashes
- downloading of repodata
- parsing of version specs
- parsing of conda version

### Fixed
- flaky package extract error ([#535](https://github.com/baszalmstra/rattler/pull/535))
- keep in mind `noarch: python` packages in clobber calculations ([#511](https://github.com/baszalmstra/rattler/pull/511))
- remove drop-bomb, move empty folder removal to `post_process` ([#519](https://github.com/baszalmstra/rattler/pull/519))
- allow multiple clobbers per package ([#526](https://github.com/baszalmstra/rattler/pull/526))
- dev-dependencies should use path deps
- self-clobbering when updating a package ([#494](https://github.com/baszalmstra/rattler/pull/494))
- do not unwrap as much in clobberregistry ([#489](https://github.com/baszalmstra/rattler/pull/489))
- copy over file permissions after reflink ([#485](https://github.com/baszalmstra/rattler/pull/485))
- consistent clobbering & removal of `__pycache__` ([#437](https://github.com/baszalmstra/rattler/pull/437))
- reduce tracing level for reflink ([#475](https://github.com/baszalmstra/rattler/pull/475))
- reflink files to destination if supported ([#463](https://github.com/baszalmstra/rattler/pull/463))
- expose previous python version information ([#384](https://github.com/baszalmstra/rattler/pull/384))
- ensure consistent sorting of locked packages
- add retry behavior for package cache downloads ([#280](https://github.com/baszalmstra/rattler/pull/280))
- mmap permission denied ([#273](https://github.com/baszalmstra/rattler/pull/273))
- suppress stderr and stdout of codesigning ([#265](https://github.com/baszalmstra/rattler/pull/265))
- clippy warnings
- less strict checking of identical packages in transaction ([#186](https://github.com/baszalmstra/rattler/pull/186))
- couple file_mode and prefix_placeholder ([#136](https://github.com/baszalmstra/rattler/pull/136))
- issue with forward slash on windows ([#92](https://github.com/baszalmstra/rattler/pull/92))
- dont share package cache
- windows build issues ([#60](https://github.com/baszalmstra/rattler/pull/60))
- remove deprecated package archive functions ([#49](https://github.com/baszalmstra/rattler/pull/49))
- tests and clippy
- formatting
- remove dependencies required for the library ([#15](https://github.com/baszalmstra/rattler/pull/15))
- channel base url test
- progressbars
- clippy warnings
- compiler warnings
- fix
- typo
- fix build
- openssl compatibility

### Other
- v0.19.0
- improve logging in package validation to include package path ([#521](https://github.com/baszalmstra/rattler/pull/521))
- try to upgrade most dependencies ([#492](https://github.com/baszalmstra/rattler/pull/492))
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- Convert authenticated client to reqwest middleware ([#488](https://github.com/baszalmstra/rattler/pull/488))
- lock-file v4 ([#484](https://github.com/baszalmstra/rattler/pull/484))
- add get_windows_launcher function ([#477](https://github.com/baszalmstra/rattler/pull/477))
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
- Release
- split lockfile implementation and add pip ([#378](https://github.com/baszalmstra/rattler/pull/378))
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- Add `link` function to py-rattler ([#364](https://github.com/baszalmstra/rattler/pull/364))
- Release
- Release
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- Merge remote-tracking branch 'upstream/main' into feat/normalize_package_names
- Release
- Release
- add documentation to all the crates ([#268](https://github.com/baszalmstra/rattler/pull/268))
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- propose a repodata patch function to be able to add `pip` to the `pyt… ([#238](https://github.com/baszalmstra/rattler/pull/238))
- Release
- Release
- version implementation ([#227](https://github.com/baszalmstra/rattler/pull/227))
- Release
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- Make entry point template reusable ([#190](https://github.com/baszalmstra/rattler/pull/190))
- Small fixes for link.json parsing, and making entry point template public ([#189](https://github.com/baszalmstra/rattler/pull/189))
- add native-tls/rustls-tls features to rattler & more dependencies ([#181](https://github.com/baszalmstra/rattler/pull/181))
- re-export hash types from rattler_digest and use them more ([#137](https://github.com/baszalmstra/rattler/pull/137))
- Release
- inherit workspace properties for crates
- update rstest requirement from 0.16.0 to 0.17.0 ([#121](https://github.com/baszalmstra/rattler/pull/121))
- update dirs requirement from 4.0.0 to 5.0.0 ([#122](https://github.com/baszalmstra/rattler/pull/122))
- add features and track_features to index.json ([#104](https://github.com/baszalmstra/rattler/pull/104))
- matchspec & version docs ([#82](https://github.com/baszalmstra/rattler/pull/82))
- Merge pull request [#71](https://github.com/baszalmstra/rattler/pull/71) from mamba-org/feat/use-conda-lock
- Polish libsolv ffi wrapper ([#50](https://github.com/baszalmstra/rattler/pull/50))
- update bindgen requirement from 0.63 to 0.64 ([#46](https://github.com/baszalmstra/rattler/pull/46))
- add licenses ([#37](https://github.com/baszalmstra/rattler/pull/37))
- update axum requirement from 0.5.13 to 0.6.2 ([#33](https://github.com/baszalmstra/rattler/pull/33))
- update bindgen requirement from 0.60 to 0.63 ([#27](https://github.com/baszalmstra/rattler/pull/27))
- added lots of channel parsing tests ([#12](https://github.com/baszalmstra/rattler/pull/12))
- Merge pull request [#10](https://github.com/baszalmstra/rattler/pull/10) from baszalmstra/fix/channel_base_url
- written down a bit shorter
- fetch repodata from channel subdirs
- move to clap
- update insta requirement from 0.12.0 to 1.16.0 ([#6](https://github.com/baszalmstra/rattler/pull/6))
- update serde_with requirement from 1.12.0 to 2.0.0 ([#7](https://github.com/baszalmstra/rattler/pull/7))
- update http-cache-reqwest requirement from 0.3.0 to 0.5.0 ([#5](https://github.com/baszalmstra/rattler/pull/5))
- update tokio-util requirement from 0.6.9 to 0.7.3 ([#4](https://github.com/baszalmstra/rattler/pull/4))
- more channel docs
- wip
- cache of complement
- fix complement of full
- infinite loop while solving
- multi version constraint set
- use logging instead of print
- fixes
- resolve is sort of working!
- wip
- wip
- fixing versionset
- parsing of matchspecs
- version spec tree parsing
- matchspec parsing
- cleanup
- initial commit

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler-v0.18.0...rattler-v0.19.0) - 2024-02-26

