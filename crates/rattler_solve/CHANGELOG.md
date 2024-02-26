# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_solve-v0.18.0...rattler_solve-v0.19.0) - 2024-02-26

### Added
- use resolvo 0.4.0 ([#523](https://github.com/baszalmstra/rattler/pull/523))
- add timeout parameter and SolverOptions to return early ([#499](https://github.com/baszalmstra/rattler/pull/499))
- upgrade to latest resolvo functionality ([#497](https://github.com/baszalmstra/rattler/pull/497))
- refactor `Version.bump()` to accept bumping `major/minor/patch/last` ([#452](https://github.com/baszalmstra/rattler/pull/452))
- add purls to PackageRecord and lockfile ([#414](https://github.com/baszalmstra/rattler/pull/414))
- add channel priority and channel-specific selectors to solver info ([#394](https://github.com/baszalmstra/rattler/pull/394))
- add strict channel priority option ([#385](https://github.com/baszalmstra/rattler/pull/385))
- display merged candidates ([#326](https://github.com/baszalmstra/rattler/pull/326))
- new repodata for errors
- added new snap
- refactored  tests and made error messages more correct
- uses name in the problem
- cargo fmt
- cargo clippy
- rattler_solve now uses nameless matchspec instead of matchspec
- pool stores references to package records
- add repr transparent to the types
- comment regarding pool
- cache now lives in dependency provider
- some small improvements
- removed `SortCache` trait replaced it with a `HashMap` for now
- libsolvrs is free from conda types
- compiling of generic solver
- made more types in the solver generic
- also fix bench
- use PackageName everywhere
- normalize package names where applicable
- additional test cases ([#249](https://github.com/baszalmstra/rattler/pull/249))
- make libsolv logging nicer ([#210](https://github.com/baszalmstra/rattler/pull/210))
- add libsolve errors to error message ([#202](https://github.com/baszalmstra/rattler/pull/202))
- use chrono timestamps ([#155](https://github.com/baszalmstra/rattler/pull/155))
- use digest in repodata ([#153](https://github.com/baszalmstra/rattler/pull/153))
- generate bindings statically ([#131](https://github.com/baszalmstra/rattler/pull/131))
- prepare for release ([#119](https://github.com/baszalmstra/rattler/pull/119))
- add some missing fields ([#108](https://github.com/baszalmstra/rattler/pull/108))
- allow caching repodata.json as .solv file ([#85](https://github.com/baszalmstra/rattler/pull/85))
- stateless solver ([#75](https://github.com/baszalmstra/rattler/pull/75))
- support installed (virtual) packages in libsolv ([#51](https://github.com/baszalmstra/rattler/pull/51))

### Fixed
- remove dependency on native-tls ([#522](https://github.com/baszalmstra/rattler/pull/522))
- dev-dependencies should use path deps
- clippy lints ([#470](https://github.com/baszalmstra/rattler/pull/470))
- use the correct channel in the reason for exclude ([#397](https://github.com/baszalmstra/rattler/pull/397))
- expose previous python version information ([#384](https://github.com/baszalmstra/rattler/pull/384))
- non-determinism error
- tests succeed again
- remove matchspec from SolveJobs
- fix typo in snap file ([#291](https://github.com/baszalmstra/rattler/pull/291))
- fix compilation in main branch ([#244](https://github.com/baszalmstra/rattler/pull/244))
- refactor ci to enable building for different targets ([#201](https://github.com/baszalmstra/rattler/pull/201))
- clippy warnings
- move libsolv to crate level for publishing ([#120](https://github.com/baszalmstra/rattler/pull/120))
- fix setting conda disttype to pool for proper constraint handling and… ([#113](https://github.com/baszalmstra/rattler/pull/113))
- include libsolv error message in unsolvable variant ([#100](https://github.com/baszalmstra/rattler/pull/100))
- robustly handle NUL in repodata.json ([#99](https://github.com/baszalmstra/rattler/pull/99))
- move match spec string generation to Display
- document unwrap usage
- ensure transaction is always created after solve
- more docs and sanity checks
- clarify docs
- use realistic package filenames in test data
- remove util in favor of rattler_conda_types
- lift remaining business logic from the libsolv wrapper
- some more libsolv cleanup
- remove todo
- simplify Solvable
- simplify Transaction
- simplify Solver
- make Repodata safer
- simplify libsolv wrapper
- extract add_repodata_records and add_virtual_packages from libsolv wrapper
- remove RepoRef
- remove RepoOwnedPtr
- simplify queue

### Other
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- resolvo 0.3.0 ([#500](https://github.com/baszalmstra/rattler/pull/500))
- fix warning for deref on a double reference ([#493](https://github.com/baszalmstra/rattler/pull/493))
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
- make the channel in the matchspec struct an actual channel ([#401](https://github.com/baszalmstra/rattler/pull/401))
- Release
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- Release
- Change solver implementation doc comment ([#352](https://github.com/baszalmstra/rattler/pull/352))
- remove rattler_libsolv_rs ([#350](https://github.com/baszalmstra/rattler/pull/350))
- hybrid incremental solver and performance improvements ([#349](https://github.com/baszalmstra/rattler/pull/349))
- Release
- get dependencies from provider ([#335](https://github.com/baszalmstra/rattler/pull/335))
- Merge branch 'main' into test/test-collapsing-error
- remove repo from pool ([#324](https://github.com/baszalmstra/rattler/pull/324))
- Merge branch 'main' into feat/nameless-matchspec
- Merge branch 'main' into feat/nameless-matchspec
- Merge remote-tracking branch 'upstream/main' into feat/generic-pool
- cache parsing of matchspec
- expose name
- Merge branch 'main' into feat/generic-pool
- removed Version::name()
- extract matchspec
- Merge branch 'main' into feat/generic-pool
- Merge branch 'main' into feat/generic-pool
- Release
- Release
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- rename libsolv crates ([#253](https://github.com/baszalmstra/rattler/pull/253))
- propose a repodata patch function to be able to add `pip` to the `pyt… ([#238](https://github.com/baszalmstra/rattler/pull/238))
- reduce sample count for solver benchmarks ([#250](https://github.com/baszalmstra/rattler/pull/250))
- make solver generic and add benchmarks ([#245](https://github.com/baszalmstra/rattler/pull/245))
- Add libsolv_rs ([#243](https://github.com/baszalmstra/rattler/pull/243))
- version parsing ([#240](https://github.com/baszalmstra/rattler/pull/240))
- Release
- Release
- Release
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- Release
- inherit workspace properties for crates
- update rstest requirement from 0.16.0 to 0.17.0 ([#121](https://github.com/baszalmstra/rattler/pull/121))
- rename SolverProblem to SolverTask ([#106](https://github.com/baszalmstra/rattler/pull/106))
- Solve problems ([#95](https://github.com/baszalmstra/rattler/pull/95))
- add features and track_features to index.json ([#104](https://github.com/baszalmstra/rattler/pull/104))
- matchspec & version docs ([#82](https://github.com/baszalmstra/rattler/pull/82))
