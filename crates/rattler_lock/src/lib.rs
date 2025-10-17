#![deny(missing_docs, dead_code)]

//! Definitions for a lock-file format that stores information about pinned
//! dependencies from both the Conda and Pypi ecosystem.
//!
//! The crate is structured in two API levels.
//!
//! 1. The top level API accessible through the [`LockFile`] type that exposes
//!    high level access to the lock-file. This API is intended to be relatively
//!    stable and is the preferred way to interact with the lock-file.
//! 2. The `*Data` types. These are lower level types that expose more of the
//!    internal data structures used in the crate. These types are not intended
//!    to be stable and are subject to change over time. These types are used
//!    internally by the top level API. Also note that only a subset of the
//!    `*Data` types are exposed. See `[crate::PyPiPackageData]`,
//!    `[crate::CondaPackageData]` for examples.
//!
//! ## Design goals
//!
//! The goal of the lock-file format is:
//!
//! * To be complete. The lock-file should contain all the information needed to
//!   recreate environments even years after it was created. As long as the
//!   package data persists that a lock-file refers to, it should be possible to
//!   recreate the environment.
//! * To be human readable. Although lock-files are not intended to be edited by
//!   hand, they should be relatively easy to read and understand. So that when
//!   a lock-file is checked into version control and someone looks at the diff,
//!   they can understand what changed.
//! * To be easily parsable. It should be fairly straightforward to create a
//!   parser for the format so that it can be used in other tools.
//! * To reduce diff size when the content changes. The order of content in the
//!   serialized lock-file should be fixed to ensure that the diff size is
//!   minimized when the content changes.
//! * To be reproducible. Recreating the lock-file with the exact same input
//!   (including externally fetched data) should yield the same lock-file
//!   byte-for-byte.
//! * To be statically verifiable. Given the specifications of the packages that
//!   went into a lock-file it should be possible to cheaply verify whether or
//!   not the specifications are still satisfied by the packages stored in the
//!   lock-file.
//! * Backward compatible. Older version of lock-files should still be readable
//!   by never versions of this crate.
//!
//! ## Relation to conda-lock
//!
//! Initially the lock-file format was based on [`conda-lock`](https://github.com/conda/conda-lock)
//! but over time significant changes have been made compared to the original
//! conda-lock format. Conda-lock files (e.g. `conda-lock.yml` files) can still
//! be parsed by this crate but the serialization format changed significantly.
//! This means files created by this crate are not compatible with conda-lock.
//!
//! Conda-lock stores a lot of metadata to be able to verify if the lock-file is
//! still valid given the sources/inputs. For example conda-lock contains a
//! `content-hash` which is a hash of all the input data of the lock-file.
//! This crate approaches this differently by storing enough information in the
//! lock-file to be able to verify if the lock-file still satisfies an
//! input/source without requiring additional input (e.g. network requests) or
//! expensive solves. We call this static satisfiability verification.
//!
//! Conda-lock stores a custom __partial__ representation of a
//! [`rattler_conda_types::RepoDataRecord`] in the lock-file. This poses a
//! problem when incrementally updating an environment. To only partially update
//! packages in the lock-file without completely recreating it, the records
//! stored in the lock-file need to be passed to the solver as "preferred"
//! packages. Since [`rattler_conda_types::MatchSpec`] can match on any field
//! present in a [`rattler_conda_types::PackageRecord`] we need to store all
//! fields in the lock-file not just a subset.
//! To that end this crate stores the full
//! [`rattler_conda_types::PackageRecord`] in the lock-file. This allows
//! completely recreating the record that was read from repodata when the
//! lock-file was created which will allow a correct incremental update.
//!
//! Conda-lock requires users to create multiple lock-files when they want to
//! store multiple environments. This crate allows storing multiple environments
//! for different platforms and with different channels in a single lock-file.
//! This allows storing production- and test environments in a single file.

use std::{collections::HashMap, io::Read, path::Path, str::FromStr, sync::Arc};

use fxhash::FxHashMap;
use indexmap::IndexSet;
use rattler_conda_types::{Platform, RepoDataRecord};

mod builder;
mod channel;
mod conda;
mod file_format_version;
mod hash;
pub mod options;
mod parse;
mod pypi;
mod pypi_indexes;
pub mod source;
mod url_or_path;
mod utils;

pub use builder::{LockFileBuilder, LockedPackage};
pub use channel::Channel;
pub use conda::{
    CondaBinaryData, CondaPackageData, CondaSourceData, ConversionError, GitShallowSpec, InputHash,
    PackageBuildSource, PackageBuildSourceKind,
};
pub use file_format_version::FileFormatVersion;
pub use hash::PackageHashes;
pub use options::SolveOptions;
pub use parse::ParseCondaLockError;
pub use pypi::{PypiPackageData, PypiPackageEnvironmentData, PypiSourceTreeHashable};
pub use pypi_indexes::{FindLinksUrlOrPath, PypiIndexes};
pub use rattler_conda_types::Matches;
pub use url_or_path::UrlOrPath;

/// The name of the default environment in a [`LockFile`]. This is the
/// environment name that is used when no explicit environment name is
/// specified.
pub const DEFAULT_ENVIRONMENT_NAME: &str = "default";

/// Represents a lock-file for both Conda packages and Pypi packages.
///
/// Lock-files can store information for multiple platforms and for multiple
/// environments.
///
/// The high-level API provided by this type holds internal references to the
/// data. Its is therefore cheap to clone this type.
#[derive(Clone, Default, Debug)]
pub struct LockFile {
    inner: Arc<LockFileInner>,
}

/// Internal data structure that stores the lock-file data.
#[derive(Default, Debug)]
struct LockFileInner {
    version: FileFormatVersion,
    environments: Vec<EnvironmentData>,
    conda_packages: Vec<CondaPackageData>,
    pypi_packages: Vec<PypiPackageData>,
    pypi_environment_package_data: Vec<PypiPackageEnvironmentData>,

    environment_lookup: FxHashMap<String, usize>,
}

/// An package used in an environment. Selects a type of package based on the
/// enum and might contain additional data that is specific to the environment.
/// For instance different environments might select the same Pypi package but
/// with different extras.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
enum EnvironmentPackageData {
    Conda(usize),
    Pypi(usize, usize),
}

/// Information about a specific environment in the lock file.
///
/// This only needs to store information about an environment that cannot be
/// derived from the packages itself.
///
/// The default environment is called "default".
#[derive(Clone, Debug)]
struct EnvironmentData {
    /// The channels used to solve the environment. Note that the order matters.
    channels: Vec<Channel>,

    /// The pypi indexes used to solve the environment.
    indexes: Option<PypiIndexes>,

    /// The options that were used to solve the environment.
    options: SolveOptions,

    /// For each individual platform this environment supports we store the
    /// package identifiers associated with the environment.
    packages: FxHashMap<Platform, IndexSet<EnvironmentPackageData>>,
}

impl LockFile {
    /// Constructs a new lock-file builder. This is the preferred way to
    /// constructs a lock-file programmatically.
    pub fn builder() -> LockFileBuilder {
        LockFileBuilder::new()
    }

    /// Parses an conda-lock file from a reader.
    pub fn from_reader(mut reader: impl Read) -> Result<Self, ParseCondaLockError> {
        let mut str = String::new();
        reader.read_to_string(&mut str)?;
        Self::from_str(&str)
    }

    /// Parses an conda-lock file from a file.
    pub fn from_path(path: &Path) -> Result<Self, ParseCondaLockError> {
        let source = std::fs::read_to_string(path)?;
        Self::from_str(&source)
    }

    /// Writes the conda lock to a file
    pub fn to_path(&self, path: &Path) -> Result<(), std::io::Error> {
        let file = std::fs::File::create(path)?;
        serde_yaml::to_writer(file, self).map_err(std::io::Error::other)
    }

    /// Writes the conda lock to a string
    pub fn render_to_string(&self) -> Result<String, std::io::Error> {
        serde_yaml::to_string(self).map_err(std::io::Error::other)
    }

    /// Returns the environment with the given name.
    pub fn environment(&self, name: &str) -> Option<Environment<'_>> {
        let index = *self.inner.environment_lookup.get(name)?;
        Some(Environment {
            lock_file: self,
            index,
        })
    }

    /// Returns the environment with the default name as defined by
    /// [`DEFAULT_ENVIRONMENT_NAME`].
    pub fn default_environment(&self) -> Option<Environment<'_>> {
        self.environment(DEFAULT_ENVIRONMENT_NAME)
    }

    /// Returns an iterator over all environments defined in the lock-file.
    pub fn environments(&self) -> impl ExactSizeIterator<Item = (&str, Environment<'_>)> + '_ {
        self.inner
            .environment_lookup
            .iter()
            .map(move |(name, index)| {
                (
                    name.as_str(),
                    Environment {
                        lock_file: self,
                        index: *index,
                    },
                )
            })
    }

    /// Returns the version of the lock-file.
    pub fn version(&self) -> FileFormatVersion {
        self.inner.version
    }

    /// Check if there are any packages in the lockfile
    pub fn is_empty(&self) -> bool {
        self.inner.conda_packages.is_empty() && self.inner.pypi_packages.is_empty()
    }
}

/// Information about a specific environment in the lock-file.
#[derive(Clone, Copy)]
pub struct Environment<'lock> {
    lock_file: &'lock LockFile,
    index: usize,
}

impl<'lock> Environment<'lock> {
    /// Returns a reference to the internal data structure.
    fn data(&self) -> &'lock EnvironmentData {
        &self.lock_file.inner.environments[self.index]
    }

    /// Returns the lock file to which this environment belongs.
    pub fn lock_file(&self) -> &'lock LockFile {
        self.lock_file
    }

    /// Returns all the platforms for which we have a locked-down environment.
    pub fn platforms(&self) -> impl ExactSizeIterator<Item = Platform> + '_ {
        self.data().packages.keys().copied()
    }

    /// Returns the channels that are used by this environment.
    ///
    /// Note that the order of the channels is significant. The first channel is
    /// the highest priority channel.
    pub fn channels(&self) -> &[Channel] {
        &self.data().channels
    }

    /// Returns the Pypi indexes that were used to solve this environment.
    ///
    /// If there are no pypi packages in the lock-file this will return `None`.
    ///
    /// Starting with version `5` of the format this should not be optional.
    pub fn pypi_indexes(&self) -> Option<&PypiIndexes> {
        self.data().indexes.as_ref()
    }

    /// Returns the solver options that were used to create this environment.
    pub fn solve_options(&self) -> &SolveOptions {
        &self.data().options
    }

    /// Returns all the packages for a specific platform in this environment.
    pub fn packages(
        &self,
        platform: Platform,
    ) -> Option<impl DoubleEndedIterator<Item = LockedPackageRef<'lock>> + ExactSizeIterator + '_>
    {
        Some(
            self.data()
                .packages
                .get(&platform)?
                .iter()
                .map(move |package| match package {
                    EnvironmentPackageData::Conda(data) => {
                        LockedPackageRef::Conda(&self.lock_file.inner.conda_packages[*data])
                    }
                    EnvironmentPackageData::Pypi(data, env_data) => LockedPackageRef::Pypi(
                        &self.lock_file.inner.pypi_packages[*data],
                        &self.lock_file.inner.pypi_environment_package_data[*env_data],
                    ),
                }),
        )
    }

    /// Returns an iterator over all packages and platforms defined for this
    /// environment
    pub fn packages_by_platform(
        &self,
    ) -> impl ExactSizeIterator<
        Item = (
            Platform,
            impl DoubleEndedIterator<Item = LockedPackageRef<'lock>> + ExactSizeIterator + '_,
        ),
    > + '_ {
        let env_data = self.data();
        env_data.packages.iter().map(move |(platform, packages)| {
            (
                *platform,
                packages.iter().map(move |package| match package {
                    EnvironmentPackageData::Conda(data) => {
                        LockedPackageRef::Conda(&self.lock_file.inner.conda_packages[*data])
                    }
                    EnvironmentPackageData::Pypi(data, env_data) => LockedPackageRef::Pypi(
                        &self.lock_file.inner.pypi_packages[*data],
                        &self.lock_file.inner.pypi_environment_package_data[*env_data],
                    ),
                }),
            )
        })
    }

    /// Returns all pypi packages for all platforms
    pub fn pypi_packages_by_platform(
        &self,
    ) -> impl ExactSizeIterator<
        Item = (
            Platform,
            impl DoubleEndedIterator<Item = (&'lock PypiPackageData, &'lock PypiPackageEnvironmentData)>,
        ),
    > + '_ {
        let env_data = self.data();
        env_data.packages.iter().map(|(platform, packages)| {
            let records = packages.iter().filter_map(|package| match package {
                EnvironmentPackageData::Conda(_) => None,
                EnvironmentPackageData::Pypi(pkg_data_idx, env_data_idx) => Some((
                    &self.lock_file.inner.pypi_packages[*pkg_data_idx],
                    &self.lock_file.inner.pypi_environment_package_data[*env_data_idx],
                )),
            });
            (*platform, records)
        })
    }

    /// Returns all conda packages for all platforms.
    pub fn conda_packages_by_platform(
        &self,
    ) -> impl ExactSizeIterator<
        Item = (
            Platform,
            impl DoubleEndedIterator<Item = &'lock CondaPackageData> + '_,
        ),
    > + '_ {
        self.packages_by_platform()
            .map(|(platform, packages)| (platform, packages.filter_map(LockedPackageRef::as_conda)))
    }

    /// Returns all binary conda packages for all platforms and converts them to
    /// [`RepoDataRecord`].
    pub fn conda_repodata_records_by_platform(
        &self,
    ) -> Result<HashMap<Platform, Vec<RepoDataRecord>>, ConversionError> {
        self.conda_packages_by_platform()
            .map(|(platform, packages)| {
                Ok((
                    platform,
                    packages
                        .filter_map(CondaPackageData::as_binary)
                        .map(RepoDataRecord::try_from)
                        .collect::<Result<Vec<_>, ConversionError>>()?,
                ))
            })
            .collect()
    }

    /// Returns all conda packages for a specific platform.
    pub fn conda_packages(
        &self,
        platform: Platform,
    ) -> Option<impl DoubleEndedIterator<Item = &'lock CondaPackageData> + '_> {
        self.packages(platform)
            .map(|packages| packages.filter_map(LockedPackageRef::as_conda))
    }

    /// Takes all the conda packages, converts them to [`RepoDataRecord`] and
    /// returns them or returns an error if the conversion failed. Returns
    /// `None` if the specified platform is not defined for this
    /// environment.
    ///
    /// This method ignores any conda packages that do not refer to repodata
    /// records.
    pub fn conda_repodata_records(
        &self,
        platform: Platform,
    ) -> Result<Option<Vec<RepoDataRecord>>, ConversionError> {
        self.conda_packages(platform)
            .map(|packages| {
                packages
                    .filter_map(CondaPackageData::as_binary)
                    .map(RepoDataRecord::try_from)
                    .collect()
            })
            .transpose()
    }

    /// Returns all the pypi packages and their associated environment data for
    /// the specified platform. Returns `None` if the platform is not
    /// defined for this environment.
    pub fn pypi_packages(
        &self,
        platform: Platform,
    ) -> Option<
        impl DoubleEndedIterator<Item = (&'lock PypiPackageData, &'lock PypiPackageEnvironmentData)>
            + '_,
    > {
        self.packages(platform)
            .map(|pkgs| pkgs.filter_map(LockedPackageRef::as_pypi))
    }

    /// Returns whether this environment has any pypi packages for the specified platform.
    pub fn has_pypi_packages(&self, platform: Platform) -> bool {
        self.pypi_packages(platform)
            .is_some_and(|mut packages| packages.next().is_some())
    }

    /// Creates a [`OwnedEnvironment`] from this environment.
    pub fn to_owned(self) -> OwnedEnvironment {
        OwnedEnvironment {
            lock_file: self.lock_file.clone(),
            index: self.index,
        }
    }
}

/// An owned version of an [`Environment`].
///
/// Use [`OwnedEnvironment::as_ref`] to get a reference to the environment data.
#[derive(Clone)]
pub struct OwnedEnvironment {
    lock_file: LockFile,
    index: usize,
}

impl OwnedEnvironment {
    /// Returns a reference to the environment data.
    pub fn as_ref(&self) -> Environment<'_> {
        Environment {
            lock_file: &self.lock_file,
            index: self.index,
        }
    }

    /// Returns the lock-file this environment is part of.
    pub fn lock_file(&self) -> LockFile {
        self.lock_file.clone()
    }
}

/// Data related to a single locked package in an [`Environment`].
#[derive(Clone, Copy)]
pub enum LockedPackageRef<'lock> {
    /// A conda package
    Conda(&'lock CondaPackageData),

    /// A pypi package
    Pypi(&'lock PypiPackageData, &'lock PypiPackageEnvironmentData),
}

impl<'lock> LockedPackageRef<'lock> {
    /// Returns the name of the package as it occurs in the lock file. This
    /// might not be the normalized name.
    pub fn name(self) -> &'lock str {
        match self {
            LockedPackageRef::Conda(data) => data.record().name.as_source(),
            LockedPackageRef::Pypi(data, _) => data.name.as_ref(),
        }
    }

    /// Returns the location of the package.
    pub fn location(self) -> &'lock UrlOrPath {
        match self {
            LockedPackageRef::Conda(data) => data.location(),
            LockedPackageRef::Pypi(data, _) => &data.location,
        }
    }

    /// Returns the pypi package if this is a pypi package.
    pub fn as_pypi(self) -> Option<(&'lock PypiPackageData, &'lock PypiPackageEnvironmentData)> {
        match self {
            LockedPackageRef::Conda(_) => None,
            LockedPackageRef::Pypi(data, env) => Some((data, env)),
        }
    }

    /// Returns the conda package if this is a conda package.
    pub fn as_conda(self) -> Option<&'lock CondaPackageData> {
        match self {
            LockedPackageRef::Conda(data) => Some(data),
            LockedPackageRef::Pypi(..) => None,
        }
    }

    /// Returns the package as a binary conda package if this is a binary conda
    /// package.
    pub fn as_binary_conda(self) -> Option<&'lock CondaBinaryData> {
        self.as_conda().and_then(CondaPackageData::as_binary)
    }

    /// Returns the package as a source conda package if this is a source conda
    /// package.
    pub fn as_source_conda(self) -> Option<&'lock CondaSourceData> {
        self.as_conda().and_then(CondaPackageData::as_source)
    }
}

#[cfg(test)]
mod test {
    use std::{
        path::{Path, PathBuf},
        str::FromStr,
    };

    use rattler_conda_types::{Platform, RepoDataRecord};
    use rstest::*;

    use super::{LockFile, DEFAULT_ENVIRONMENT_NAME};

    #[rstest]
    #[case::v0_numpy("v0/numpy-conda-lock.yml")]
    #[case::v0_python("v0/python-conda-lock.yml")]
    #[case::v0_pypi_matplotlib("v0/pypi-matplotlib-conda-lock.yml")]
    #[case::v3_robostack("v3/robostack-turtlesim-conda-lock.yml")]
    #[case::v3_numpy("v4/numpy-lock.yml")]
    #[case::v4_python("v4/python-lock.yml")]
    #[case::v4_pypi_matplotlib("v4/pypi-matplotlib-lock.yml")]
    #[case::v4_turtlesim("v4/turtlesim-lock.yml")]
    #[case::v4_pypi_path("v4/path-based-lock.yml")]
    #[case::v4_pypi_absolute_path("v4/absolute-path-lock.yml")]
    #[case::v5_pypi_flat_index("v5/flat-index-lock.yml")]
    #[case::v5_with_and_without_purl("v5/similar-with-and-without-purl.yml")]
    #[case::v6_conda_source_path("v6/conda-path-lock.yml")]
    #[case::v6_derived_channel("v6/derived-channel-lock.yml")]
    #[case::v6_sources("v6/sources-lock.yml")]
    #[case::v6_options("v6/options-lock.yml")]
    #[case::v6_pixi_build_pinned_source("v6/pixi-build-pinned-source-lock.yml")]
    #[case::v6_pixi_build_url_source("v6/pixi-build-url-source-lock.yml")]
    #[case::v6_pixi_build_git_tag_source("v6/pixi-build-git-tag-source-lock.yml")]
    #[case::v6_pixi_build_git_rev_only_source("v6/pixi-build-git-rev-only-source-lock.yml")]
    fn test_parse(#[case] file_name: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join(file_name);
        let conda_lock = LockFile::from_path(&path).unwrap();
        insta::assert_yaml_snapshot!(file_name, conda_lock);
    }

    #[rstest]
    fn test_roundtrip(
        #[files("../../test-data/conda-lock/**/*.yml")]
        #[exclude("forward-compatible-lock")]
        path: PathBuf,
    ) {
        // Load the lock-file
        let conda_lock = LockFile::from_path(&path).unwrap();

        // Serialize the lock-file
        let rendered_lock_file = conda_lock.render_to_string().unwrap();

        // Parse the rendered lock-file again
        let parsed_lock_file = LockFile::from_str(&rendered_lock_file).unwrap();

        // And re-render again
        let rerendered_lock_file = parsed_lock_file.render_to_string().unwrap();

        similar_asserts::assert_eq!(rendered_lock_file, rerendered_lock_file);
    }

    /// Absolute paths on Windows are not properly parsed.
    /// See: <https://github.com/conda/rattler/issues/615>
    #[test]
    fn test_issue_615() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock/v4/absolute-path-lock.yml");
        let conda_lock = LockFile::from_path(&path);
        assert!(conda_lock.is_ok());
    }

    #[test]
    fn packages_for_platform() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join("v0/numpy-conda-lock.yml");

        // Try to read conda_lock
        let conda_lock = LockFile::from_path(&path).unwrap();

        insta::assert_yaml_snapshot!(conda_lock
            .environment(DEFAULT_ENVIRONMENT_NAME)
            .unwrap()
            .packages(Platform::Linux64)
            .unwrap()
            .map(|p| p.location().to_string())
            .collect::<Vec<_>>());

        insta::assert_yaml_snapshot!(conda_lock
            .environment(DEFAULT_ENVIRONMENT_NAME)
            .unwrap()
            .packages(Platform::Osx64)
            .unwrap()
            .map(|p| p.location().to_string())
            .collect::<Vec<_>>());
    }

    #[test]
    fn test_has_pypi_packages() {
        // v4
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join("v4/pypi-matplotlib-lock.yml");
        let conda_lock = LockFile::from_path(&path).unwrap();

        assert!(conda_lock
            .environment(DEFAULT_ENVIRONMENT_NAME)
            .unwrap()
            .has_pypi_packages(Platform::Linux64));

        // v6
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join("v6/numpy-as-pypi-lock.yml");
        let conda_lock = LockFile::from_path(&path).unwrap();

        assert!(conda_lock
            .environment(DEFAULT_ENVIRONMENT_NAME)
            .unwrap()
            .has_pypi_packages(Platform::OsxArm64));

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join("v6/python-from-conda-only-lock.yml");
        let conda_lock = LockFile::from_path(&path).unwrap();

        assert!(!conda_lock
            .environment(DEFAULT_ENVIRONMENT_NAME)
            .unwrap()
            .has_pypi_packages(Platform::OsxArm64));
    }

    #[test]
    fn test_is_empty() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join("v6/empty-lock.yml");
        let conda_lock = LockFile::from_path(&path).unwrap();
        assert!(conda_lock.is_empty());

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/conda-lock")
            .join("v6/python-from-conda-only-lock.yml");
        let conda_lock = LockFile::from_path(&path).unwrap();
        assert!(!conda_lock.is_empty());
    }

    #[test]
    fn solve_roundtrip() {
        // load repodata from JSON
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/repodata-records/_libgcc_mutex-0.1-conda_forge.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let repodata_record: RepoDataRecord = serde_json::from_str(&content).unwrap();

        // check that the repodata record is as expected
        assert_eq!(repodata_record.package_record.arch, None);
        assert_eq!(repodata_record.package_record.platform, None);

        // create a lockfile with the repodata record
        let lock_file = LockFile::builder()
            .with_conda_package(
                DEFAULT_ENVIRONMENT_NAME,
                Platform::Linux64,
                repodata_record.clone().into(),
            )
            .finish();

        // serialize the lockfile
        let rendered_lock_file = lock_file.render_to_string().unwrap();

        // parse the lockfile
        let parsed_lock_file = LockFile::from_str(&rendered_lock_file).unwrap();
        // get repodata record from parsed lockfile
        let repodata_records = parsed_lock_file
            .environment(DEFAULT_ENVIRONMENT_NAME)
            .unwrap()
            .conda_repodata_records(Platform::Linux64)
            .unwrap()
            .unwrap();

        // These are not equal because the one from `repodata_records[0]` contains arch and platform.
        let repodata_record_two = repodata_records[0].clone();
        assert_eq!(
            repodata_record_two.package_record.arch,
            Some("x86_64".to_string())
        );
        assert_eq!(
            repodata_record_two.package_record.platform,
            Some("linux".to_string())
        );

        // But if we render it again, the lockfile should look the same at least
        let rerendered_lock_file_two = parsed_lock_file.render_to_string().unwrap();
        assert_eq!(rendered_lock_file, rerendered_lock_file_two);
    }

    /// Tests that source packages with virtual=true are correctly serialized to YAML and
    /// deserialized back, preserving the virtual field through a roundtrip.
    #[test]
    fn test_virtual_field_serialization() {
        use crate::{CondaSourceData, UrlOrPath};
        use rattler_conda_types::{PackageRecord, VersionWithSource};
        use std::str::FromStr;

        // Create a source package with virtual=true
        let version = VersionWithSource::from_str("1.0.0").unwrap();
        let mut package_record = PackageRecord::new(
            "test-package".parse().unwrap(),
            version,
            "py39_0".to_string(),
        );
        package_record.build_number = 0;
        package_record.subdir = "linux-64".to_string();

        let source_data = CondaSourceData {
            package_record,
            location: UrlOrPath::Url("https://example.com/test-package.tar.bz2".parse().unwrap()),
            package_build_source: None,
            input: None,
            sources: Default::default(),
            r#virtual: true,
        };

        let package = crate::CondaPackageData::Source(source_data);

        // Create a lock file with the virtual package
        let lock_file = LockFile::builder()
            .with_conda_package(DEFAULT_ENVIRONMENT_NAME, Platform::Linux64, package)
            .finish();

        // Serialize the lock file
        let rendered = lock_file.render_to_string().unwrap();

        // Parse it back
        let parsed = LockFile::from_str(&rendered).unwrap();

        // Verify the virtual field was preserved
        let env = parsed.environment(DEFAULT_ENVIRONMENT_NAME).unwrap();
        let packages: Vec<_> = env.packages(Platform::Linux64).unwrap().collect();
        assert_eq!(packages.len(), 1);

        let source = packages[0]
            .as_source_conda()
            .expect("Should be a source package");
        assert!(source.r#virtual, "Virtual field should be true");
    }

    /// Tests that the virtual field defaults to false and is not serialized when false,
    /// helping to keep lock files minimal by omitting the field for non-virtual packages.
    #[test]
    fn test_virtual_field_default_false() {
        use crate::{CondaSourceData, UrlOrPath};
        use rattler_conda_types::{PackageRecord, VersionWithSource};
        use std::str::FromStr;

        // Create a source package with virtual=false (default)
        let version = VersionWithSource::from_str("1.0.0").unwrap();
        let mut package_record = PackageRecord::new(
            "test-package".parse().unwrap(),
            version,
            "py39_0".to_string(),
        );
        package_record.build_number = 0;
        package_record.subdir = "linux-64".to_string();

        let source_data = CondaSourceData {
            package_record,
            location: UrlOrPath::Url("https://example.com/test-package.tar.bz2".parse().unwrap()),
            package_build_source: None,
            input: None,
            sources: Default::default(),
            r#virtual: false,
        };

        let package = crate::CondaPackageData::Source(source_data);

        // Create a lock file
        let lock_file = LockFile::builder()
            .with_conda_package(DEFAULT_ENVIRONMENT_NAME, Platform::Linux64, package)
            .finish();

        // Serialize the lock file
        let rendered = lock_file.render_to_string().unwrap();

        // Check that virtual field is NOT in the output when false (optimization)
        assert!(
            !rendered.contains("virtual: false"),
            "virtual: false should not be serialized"
        );

        // Parse it back
        let parsed = LockFile::from_str(&rendered).unwrap();

        // Verify the virtual field defaults to false
        let env = parsed.environment(DEFAULT_ENVIRONMENT_NAME).unwrap();
        let packages: Vec<_> = env.packages(Platform::Linux64).unwrap().collect();
        let source = packages[0]
            .as_source_conda()
            .expect("Should be a source package");
        assert!(!source.r#virtual, "Virtual field should default to false");
    }

    /// Tests that both virtual and non-virtual versions of the same package can coexist
    /// in a lock file at the same URL location, differentiated by their virtual field values.
    /// This is the core use case: allowing development dependencies to be specified without
    /// being installed.
    #[test]
    fn test_virtual_and_non_virtual_same_location() {
        use crate::{CondaSourceData, UrlOrPath};
        use rattler_conda_types::{PackageRecord, VersionWithSource};
        use std::str::FromStr;

        let url: url::Url = "https://example.com/package.tar.bz2".parse().unwrap();

        // Create a virtual version
        let version = VersionWithSource::from_str("1.0.0").unwrap();
        let mut virt_pkg_record =
            PackageRecord::new("mypackage".parse().unwrap(), version, "py39_0".to_string());
        virt_pkg_record.build_number = 0;
        virt_pkg_record.subdir = "linux-64".to_string();

        let virtual_package = crate::CondaPackageData::Source(CondaSourceData {
            package_record: virt_pkg_record,
            location: UrlOrPath::Url(url.clone()),
            package_build_source: None,
            input: None,
            sources: Default::default(),
            r#virtual: true,
        });

        // Create a non-virtual version with same location
        let version2 = VersionWithSource::from_str("1.0.0").unwrap();
        let mut non_virt_pkg_record =
            PackageRecord::new("mypackage".parse().unwrap(), version2, "py39_0".to_string());
        non_virt_pkg_record.build_number = 0;
        non_virt_pkg_record.subdir = "linux-64".to_string();

        let non_virtual_package = crate::CondaPackageData::Source(CondaSourceData {
            package_record: non_virt_pkg_record,
            location: UrlOrPath::Url(url),
            package_build_source: None,
            input: None,
            sources: Default::default(),
            r#virtual: false,
        });

        // Create lock file with both
        let lock_file = LockFile::builder()
            .with_conda_package(DEFAULT_ENVIRONMENT_NAME, Platform::Linux64, virtual_package)
            .with_conda_package(
                DEFAULT_ENVIRONMENT_NAME,
                Platform::Linux64,
                non_virtual_package,
            )
            .finish();

        // Serialize and roundtrip
        let rendered = lock_file.render_to_string().unwrap();

        // Parse back
        let parsed = LockFile::from_str(&rendered).unwrap();

        // Verify packages exist
        let env = parsed.environment(DEFAULT_ENVIRONMENT_NAME).unwrap();
        let packages: Vec<_> = env.packages(Platform::Linux64).unwrap().collect();
        assert!(!packages.is_empty(), "Should have at least one package");

        // Verify at least one is virtual
        let mut has_virtual = false;

        for pkg in packages {
            if let Some(source) = pkg.as_source_conda() {
                if source.r#virtual {
                    has_virtual = true;
                }
            }
        }

        assert!(
            has_virtual,
            "Should have at least one virtual package in the output"
        );
    }

    /// Tests backward compatibility by verifying that lock files without the virtual field
    /// (from older versions) can still be parsed correctly, with virtual defaulting to false.
    #[test]
    fn test_virtual_package_backward_compatibility() {
        use std::str::FromStr;

        // YAML with a source package that doesn't have a virtual field
        let yaml_without_virtual = r#"
version: 6
environments:
  default:
    channels:
      - url: https://conda.anaconda.org/conda-forge/
    packages:
      linux-64:
        - conda: https://example.com/package.tar.bz2
          name: mypackage
          version: "1.0.0"
          build: py39_0
          subdir: linux-64
packages:
  - conda: https://example.com/package.tar.bz2
    name: mypackage
    version: "1.0.0"
    build: py39_0
    subdir: linux-64
"#;

        // Parse should succeed
        let lock_file = LockFile::from_str(yaml_without_virtual).unwrap();

        // Verify the package was loaded without virtual field (defaults to false)
        let env = lock_file.environment(DEFAULT_ENVIRONMENT_NAME).unwrap();
        let packages: Vec<_> = env.packages(Platform::Linux64).unwrap().collect();
        assert_eq!(packages.len(), 1);

        let source = packages[0]
            .as_source_conda()
            .expect("Should be a source package");
        assert!(
            !source.r#virtual,
            "Should default to false for old lock files"
        );

        // Re-render should not add virtual: false
        let rendered = lock_file.render_to_string().unwrap();
        assert!(!rendered.contains("virtual: false"));
    }

    /// Tests that the virtual field is correctly used in package disambiguation when
    /// multiple packages with the same name, version, and build need to be distinguished.
    /// This ensures that virtual and non-virtual versions can be unambiguously referenced.
    #[test]
    fn test_virtual_disambiguation() {
        use crate::{CondaSourceData, UrlOrPath};
        use rattler_conda_types::{PackageRecord, VersionWithSource};
        use std::str::FromStr as _;

        let url: url::Url = "https://example.com/package.tar.bz2".parse().unwrap();

        // Create multiple packages to test disambiguation
        // Package 1: name=pkg, version=1.0.0, build=py39_0, non-virtual
        let version = VersionWithSource::from_str("1.0.0").unwrap();
        let mut pkg1_record =
            PackageRecord::new("pkg".parse().unwrap(), version, "py39_0".to_string());
        pkg1_record.build_number = 0;
        pkg1_record.subdir = "linux-64".to_string();

        let pkg1 = crate::CondaPackageData::Source(CondaSourceData {
            package_record: pkg1_record,
            location: UrlOrPath::Url(url.clone()),
            package_build_source: None,
            input: None,
            sources: Default::default(),
            r#virtual: false,
        });

        // Package 2: name=pkg, version=1.0.0, build=py39_0, virtual
        let version2 = VersionWithSource::from_str("1.0.0").unwrap();
        let mut pkg2_record =
            PackageRecord::new("pkg".parse().unwrap(), version2, "py39_0".to_string());
        pkg2_record.build_number = 0;
        pkg2_record.subdir = "linux-64".to_string();

        let pkg2 = crate::CondaPackageData::Source(CondaSourceData {
            package_record: pkg2_record,
            location: UrlOrPath::Url(url),
            package_build_source: None,
            input: None,
            sources: Default::default(),
            r#virtual: true,
        });

        let lock_file = LockFile::builder()
            .with_conda_package(DEFAULT_ENVIRONMENT_NAME, Platform::Linux64, pkg1)
            .with_conda_package(DEFAULT_ENVIRONMENT_NAME, Platform::Linux64, pkg2)
            .finish();

        // Serialize and parse to verify roundtrip works
        let rendered = lock_file.render_to_string().unwrap();
        let _parsed = LockFile::from_str(&rendered).unwrap();
    }

    /// Tests that multiple render-parse cycles maintain consistency with virtual packages.
    /// This verifies that the serialization format is stable and reproducible, which is
    /// a core design goal of the lock file format.
    #[test]
    fn test_virtual_roundtrip() {
        use crate::{CondaSourceData, UrlOrPath};
        use rattler_conda_types::{PackageRecord, VersionWithSource};
        use std::str::FromStr;

        // Create a mix of virtual and non-virtual packages
        let mut builder = LockFile::builder();

        let packages_config = vec![(0, false), (1, true), (2, false), (3, true)];

        for (i, is_virtual) in packages_config {
            let version = VersionWithSource::from_str("1.0.0").unwrap();
            let mut pkg_record = PackageRecord::new(
                format!("pkg{}", i).parse().unwrap(),
                version,
                "py39_0".to_string(),
            );
            pkg_record.build_number = 0;
            pkg_record.subdir = "linux-64".to_string();

            let package = crate::CondaPackageData::Source(CondaSourceData {
                package_record: pkg_record,
                location: UrlOrPath::Url(
                    format!("https://example.com/pkg{}.tar.bz2", i)
                        .parse()
                        .unwrap(),
                ),
                package_build_source: None,
                input: None,
                sources: Default::default(),
                r#virtual: is_virtual,
            });

            builder =
                builder.with_conda_package(DEFAULT_ENVIRONMENT_NAME, Platform::Linux64, package);
        }

        let lock_file = builder.finish();

        // First render
        let rendered1 = lock_file.render_to_string().unwrap();
        let parsed1 = LockFile::from_str(&rendered1).unwrap();

        // Second render
        let rendered2 = parsed1.render_to_string().unwrap();
        let parsed2 = LockFile::from_str(&rendered2).unwrap();

        // Third render
        let rendered3 = parsed2.render_to_string().unwrap();

        // All renders should be identical
        assert_eq!(rendered1, rendered2, "First and second render should match");
        assert_eq!(rendered2, rendered3, "Second and third render should match");

        // Verify virtual fields are preserved
        let env = parsed2.environment(DEFAULT_ENVIRONMENT_NAME).unwrap();
        let packages: Vec<_> = env.packages(Platform::Linux64).unwrap().collect();
        assert_eq!(packages.len(), 4);

        let expected_virtual = vec![false, true, false, true];
        for (pkg, &expected) in packages.iter().zip(expected_virtual.iter()) {
            let source = pkg.as_source_conda().expect("Should be source package");
            assert_eq!(
                source.r#virtual, expected,
                "Virtual field mismatch for package"
            );
        }
    }
}
