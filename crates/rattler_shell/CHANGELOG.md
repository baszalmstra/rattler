# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.19.0](https://github.com/baszalmstra/rattler/compare/rattler_shell-v0.18.0...rattler_shell-v0.19.0) - 2024-02-26

### Added
- add from_str to the shellenum ([#258](https://github.com/baszalmstra/rattler/pull/258))
- run activation and capture result ([#239](https://github.com/baszalmstra/rattler/pull/239))
- Add path variable modification for the shells with env var expansion ([#232](https://github.com/baszalmstra/rattler/pull/232))
- add executable name to the shell trait and implement it ([#224](https://github.com/baszalmstra/rattler/pull/224))
- added experimental fields to conda lock ([#221](https://github.com/baszalmstra/rattler/pull/221))
- add a dynamic shell type and simple discovery ([#187](https://github.com/baszalmstra/rattler/pull/187))

### Fixed
- clippy lints ([#470](https://github.com/baszalmstra/rattler/pull/470))
- recursive look for parent process name ([#424](https://github.com/baszalmstra/rattler/pull/424))
- environment activation for windows ([#398](https://github.com/baszalmstra/rattler/pull/398))
- format env variable for `fish` shell ([#264](https://github.com/baszalmstra/rattler/pull/264))
- powershell on unix ([#234](https://github.com/baszalmstra/rattler/pull/234))
- zsh also uses `sh` as shell extension ([#223](https://github.com/baszalmstra/rattler/pull/223))

### Other
- try to upgrade most dependencies ([#492](https://github.com/baszalmstra/rattler/pull/492))
- move all dependencies to workspace ([#501](https://github.com/baszalmstra/rattler/pull/501))
- lock-file v4 ([#484](https://github.com/baszalmstra/rattler/pull/484))
- fix clippy and deprecation warnings ([#490](https://github.com/baszalmstra/rattler/pull/490))
- Release
- Release
- Release
- more clippy lint ([#462](https://github.com/baszalmstra/rattler/pull/462))
- Release
- rename all `behaviour` to `behavior` ([#428](https://github.com/baszalmstra/rattler/pull/428))
- Release
- Release
- Release
- Release
- Release
- Release
- fmt ([#376](https://github.com/baszalmstra/rattler/pull/376))
- Fix/xonsh extension ([#375](https://github.com/baszalmstra/rattler/pull/375))
- all dependencies ([#366](https://github.com/baszalmstra/rattler/pull/366))
- Nushell fixes ([#360](https://github.com/baszalmstra/rattler/pull/360))
- Release
- add initial nushell support ([#271](https://github.com/baszalmstra/rattler/pull/271))
- Release
- use `any` instead of `find`
- filter empty keys from environment
- improve xonsh detection
- escape environment variables in powershell for Programs(x86)
- Update all dependencies and fix chrono deprecation ([#302](https://github.com/baszalmstra/rattler/pull/302))
- Release
- Release
- run_command ends with a newline ([#262](https://github.com/baszalmstra/rattler/pull/262))
- call codesign binary instead of using codesign crate ([#259](https://github.com/baszalmstra/rattler/pull/259))
- version parsing ([#240](https://github.com/baszalmstra/rattler/pull/240))
- Release
- Release
- detect current shell from $SHELL or parent process ID ([#219](https://github.com/baszalmstra/rattler/pull/219))
- Release
- update all dependencies ([#208](https://github.com/baszalmstra/rattler/pull/208))
- return path and script from activation command ([#151](https://github.com/baszalmstra/rattler/pull/151))
- Refactor activation to use rattler_conda_types::Platform instead of new enum ([#144](https://github.com/baszalmstra/rattler/pull/144))
- inherit workspace properties for crates
- add features and track_features to index.json ([#104](https://github.com/baszalmstra/rattler/pull/104))
- add activation script writing ([#77](https://github.com/baszalmstra/rattler/pull/77))
- Shell activation helpers ([#68](https://github.com/baszalmstra/rattler/pull/68))
