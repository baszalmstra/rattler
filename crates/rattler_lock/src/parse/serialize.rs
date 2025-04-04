use crate::{
    file_format_version::FileFormatVersion,
    parse::{models::v6, V6},
    Channel, CondaPackageData, EnvironmentData, EnvironmentPackageData, FindLinksUrlOrPath,
    LockFile, LockFileInner, PypiIndexes, PypiPackageData, PypiPackageEnvironmentData, UrlOrPath,
};
use itertools::Itertools;
use pep508_rs::ExtraName;
use rattler_conda_types::{PackageName, Platform, RawNoArchType, VersionWithSource};
use serde::{Serialize, Serializer};
use serde_with::{serde_as, SerializeAs};
use simple_yaml_writer::{YamlSequence, YamlTable, YamlWriter};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
    marker::PhantomData,
};
use url::Url;

impl LockFile {
    /// Writes the conda lock to a file
    pub fn to_path(&self, path: &Path) -> Result<(), std::io::Error> {
        let file = std::fs::File::create(path)?;
        SerializableLockFile::from(self).to_writer(BufWriter::new(file))
    }

    /// Writes the conda lock to a string
    pub fn render_to_string(&self) -> Result<String, std::io::Error> {
        let mut buffer = Vec::new();
        SerializableLockFile::from(self).to_writer(&mut buffer)?;
        Ok(String::from_utf8(buffer).expect("valid utf-8"))
    }
}

#[serde_as]
#[derive(Serialize)]
#[serde(bound(serialize = "V: SerializeAs<PackageData<'a>>"))]
struct SerializableLockFile<'a, V> {
    version: FileFormatVersion,
    environments: BTreeMap<&'a String, SerializableEnvironment<'a>>,
    #[serde_as(as = "Vec<V>")]
    packages: Vec<PackageData<'a>>,
    #[serde(skip)]
    _version: PhantomData<V>,
}

#[derive(Serialize)]
struct SerializableEnvironment<'a> {
    channels: &'a [Channel],
    #[serde(flatten)]
    indexes: Option<&'a PypiIndexes>,
    packages: BTreeMap<Platform, Vec<SerializablePackageSelector<'a>>>,
}

impl<'a> SerializableEnvironment<'a> {
    fn from_environment(
        inner: &'a LockFileInner,
        env_data: &'a EnvironmentData,
        used_conda_packages: &HashSet<usize>,
        used_pypi_packages: &HashSet<usize>,
    ) -> Self {
        SerializableEnvironment {
            channels: &env_data.channels,
            indexes: env_data.indexes.as_ref(),
            packages: env_data
                .packages
                .iter()
                .map(|(platform, packages)| {
                    (
                        *platform,
                        packages
                            .iter()
                            .map(|&package_data| {
                                SerializablePackageSelector::from_lock_file(
                                    inner,
                                    package_data,
                                    used_conda_packages,
                                    used_pypi_packages,
                                )
                            })
                            .sorted()
                            .collect(),
                    )
                })
                .collect(),
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Eq, PartialEq)]
#[serde(untagged)]
enum SerializablePackageDataV6<'a> {
    Conda(v6::CondaPackageDataModel<'a>),
    Pypi(v6::PypiPackageDataModel<'a>),
}

impl<'a> From<PackageData<'a>> for SerializablePackageDataV6<'a> {
    fn from(package: PackageData<'a>) -> Self {
        match package {
            PackageData::Conda(p) => Self::Conda(p.into()),
            PackageData::Pypi(p) => Self::Pypi(p.into()),
        }
    }
}

#[derive(Serialize, Eq, PartialEq)]
#[serde(untagged, rename_all = "snake_case")]
enum SerializablePackageSelector<'a> {
    Conda {
        conda: &'a UrlOrPath,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a PackageName>,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<&'a VersionWithSource>,
        #[serde(skip_serializing_if = "Option::is_none")]
        build: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subdir: Option<&'a str>,
    },
    Pypi {
        pypi: &'a UrlOrPath,
        #[serde(skip_serializing_if = "BTreeSet::is_empty")]
        extras: &'a BTreeSet<ExtraName>,
    },
}

#[derive(Copy, Clone)]
enum CondaDisambiguityFilter {
    Name,
    Version,
    Build,
    Subdir,
}

impl CondaDisambiguityFilter {
    fn all() -> [CondaDisambiguityFilter; 4] {
        [Self::Name, Self::Version, Self::Build, Self::Subdir]
    }

    fn filter(&self, package: &CondaPackageData, other: &CondaPackageData) -> bool {
        match self {
            Self::Name => package.record().name == other.record().name,
            Self::Version => package.record().version == other.record().version,
            Self::Build => package.record().build == other.record().build,
            Self::Subdir => package.record().subdir == other.record().subdir,
        }
    }
}

impl<'a> SerializablePackageSelector<'a> {
    fn from_lock_file(
        inner: &'a LockFileInner,
        package: EnvironmentPackageData,
        used_conda_packages: &HashSet<usize>,
        used_pypi_packages: &HashSet<usize>,
    ) -> Self {
        match package {
            EnvironmentPackageData::Conda(idx) => {
                Self::from_conda(inner, &inner.conda_packages[idx], used_conda_packages)
            }
            EnvironmentPackageData::Pypi(pkg_data_idx, env_data_idx) => Self::from_pypi(
                inner,
                &inner.pypi_packages[pkg_data_idx],
                &inner.pypi_environment_package_data[env_data_idx],
                used_pypi_packages,
            ),
        }
    }

    fn from_conda(
        inner: &'a LockFileInner,
        package: &'a CondaPackageData,
        used_conda_packages: &HashSet<usize>,
    ) -> Self {
        // Find all packages that share the same location
        let mut similar_packages = inner
            .conda_packages
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| used_conda_packages.contains(&idx).then_some(p))
            .filter(|p| p.location() == package.location())
            .collect::<Vec<_>>();

        // Iterate over other distinguising factors and reduce the set of possible
        // packages to a minimum with the least number of keys added.
        let mut name = None;
        let mut version = None;
        let mut build = None;
        let mut subdir = None;
        while similar_packages.len() > 1 {
            let (filter, similar) = CondaDisambiguityFilter::all()
                .into_iter()
                .map(|filter| {
                    (
                        filter,
                        similar_packages
                            .iter()
                            .copied()
                            .filter(|p| filter.filter(package, p))
                            .collect_vec(),
                    )
                })
                .min_by_key(|(_filter, set)| set.len())
                .expect("cannot be empty because the set should always contain `package`");

            if similar.len() == similar_packages.len() {
                // No further disambiguation possible. Assume that the package is a duplicate.
                break;
            }

            similar_packages = similar;
            match filter {
                CondaDisambiguityFilter::Name => {
                    name = Some(&package.record().name);
                }
                CondaDisambiguityFilter::Version => {
                    version = Some(&package.record().version);
                }
                CondaDisambiguityFilter::Build => {
                    build = Some(package.record().build.as_str());
                }
                CondaDisambiguityFilter::Subdir => {
                    subdir = Some(package.record().subdir.as_str());
                }
            }
        }

        Self::Conda {
            conda: package.location(),
            name,
            version,
            build,
            subdir,
        }
    }

    fn from_pypi(
        _inner: &'a LockFileInner,
        package: &'a PypiPackageData,
        env: &'a PypiPackageEnvironmentData,
        _used_pypi_packages: &HashSet<usize>,
    ) -> Self {
        Self::Pypi {
            pypi: &package.location,
            extras: &env.extras,
        }
    }
}

impl<'a> PartialOrd for SerializablePackageSelector<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for SerializablePackageSelector<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (
                SerializablePackageSelector::Conda { .. },
                SerializablePackageSelector::Pypi { .. },
            ) => {
                // Sort conda packages before pypi packages
                Ordering::Less
            }
            (
                SerializablePackageSelector::Pypi { .. },
                SerializablePackageSelector::Conda { .. },
            ) => {
                // Sort Pypi packages after conda packages
                Ordering::Greater
            }
            (
                SerializablePackageSelector::Conda {
                    conda: a,
                    name: name_a,
                    build: build_a,
                    version: version_a,
                    subdir: subdir_a,
                },
                SerializablePackageSelector::Conda {
                    conda: b,
                    name: name_b,
                    build: build_b,
                    version: version_b,
                    subdir: subdir_b,
                },
            ) => compare_url_by_location(a, b)
                .then_with(|| name_a.cmp(name_b))
                .then_with(|| version_a.cmp(version_b))
                .then_with(|| build_a.cmp(build_b))
                .then_with(|| subdir_a.cmp(subdir_b)),
            (
                SerializablePackageSelector::Pypi { pypi: a, .. },
                SerializablePackageSelector::Pypi { pypi: b, .. },
            ) => compare_url_by_location(a, b),
        }
    }
}

/// First sort packages just by their filename. Since most of the time the urls
/// end in the packages filename this causes the urls to be sorted by package
/// name.
fn compare_url_by_filename(a: &Url, b: &Url) -> Ordering {
    if let (Some(a), Some(b)) = (
        a.path_segments()
            .and_then(Iterator::last)
            .map(str::to_lowercase),
        b.path_segments()
            .and_then(Iterator::last)
            .map(str::to_lowercase),
    ) {
        match a.cmp(&b) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }

    // Otherwise just sort by their full URL
    a.cmp(b)
}

fn compare_url_by_location(a: &UrlOrPath, b: &UrlOrPath) -> Ordering {
    match (a, b) {
        (UrlOrPath::Url(a), UrlOrPath::Url(b)) => compare_url_by_filename(a, b),
        (UrlOrPath::Url(_), UrlOrPath::Path(_)) => Ordering::Less,
        (UrlOrPath::Path(_), UrlOrPath::Url(_)) => Ordering::Greater,
        (UrlOrPath::Path(a), UrlOrPath::Path(b)) => a.as_str().cmp(b.as_str()),
    }
}

impl<'a> SerializeAs<PackageData<'a>> for V6 {
    fn serialize_as<S>(source: &PackageData<'a>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SerializablePackageDataV6::from(*source).serialize(serializer)
    }
}

impl Serialize for LockFile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SerializableLockFile::from(self).serialize(serializer)
    }
}

impl<'a> From<&'a LockFile> for SerializableLockFile<'a, V6> {
    fn from(value: &'a LockFile) -> Self {
        let inner = value.inner.as_ref();

        // Determine the package indexes that are used in the lock-file.
        let mut used_conda_packages = HashSet::new();
        let mut used_pypi_packages = HashSet::new();
        for env in inner.environments.iter() {
            for packages in env.packages.values() {
                for package in packages {
                    match package {
                        EnvironmentPackageData::Conda(idx) => {
                            used_conda_packages.insert(*idx);
                        }
                        EnvironmentPackageData::Pypi(pkg_idx, _env_idx) => {
                            used_pypi_packages.insert(*pkg_idx);
                        }
                    }
                }
            }
        }

        // Collect all environments
        let environments = inner
            .environment_lookup
            .iter()
            .map(|(name, env_idx)| {
                (
                    name,
                    SerializableEnvironment::from_environment(
                        inner,
                        &inner.environments[*env_idx],
                        &used_conda_packages,
                        &used_pypi_packages,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();

        // Get all packages.
        let conda_packages = inner
            .conda_packages
            .iter()
            .enumerate()
            .filter(|(idx, _)| used_conda_packages.contains(idx))
            .map(|(_, p)| PackageData::Conda(p));

        let pypi_packages = inner
            .pypi_packages
            .iter()
            .enumerate()
            .filter(|(idx, _)| used_pypi_packages.contains(idx))
            .map(|(_, p)| PackageData::Pypi(p));

        // Sort the packages in a deterministic order. See [`SerializablePackageData`]
        // for more information.
        let packages = itertools::chain!(conda_packages, pypi_packages).sorted();

        SerializableLockFile {
            version: FileFormatVersion::LATEST,
            environments,
            packages: packages.collect(),
            _version: PhantomData::<V6>,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum PackageData<'a> {
    Conda(&'a CondaPackageData),
    Pypi(&'a PypiPackageData),
}

impl<'a> PackageData<'a> {
    fn source_name(&self) -> &str {
        match self {
            PackageData::Conda(p) => p.record().name.as_source(),
            PackageData::Pypi(p) => p.name.as_ref(),
        }
    }
}

impl<'a> PartialOrd<Self> for PackageData<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for PackageData<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        use PackageData::{Conda, Pypi};
        self.source_name()
            .cmp(other.source_name())
            .then_with(|| match (self, other) {
                (Conda(a), Conda(b)) => a.cmp(b),
                (Pypi(a), Pypi(b)) => a.cmp(b),
                (Pypi(_), _) => Ordering::Less,
                (_, Pypi(_)) => Ordering::Greater,
            })
    }
}

impl Serialize for CondaPackageData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SerializablePackageDataV6::Conda(v6::CondaPackageDataModel::from(self))
            .serialize(serializer)
    }
}

impl Serialize for PypiPackageData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SerializablePackageDataV6::Pypi(v6::PypiPackageDataModel::from(self)).serialize(serializer)
    }
}

impl<'a> SerializableLockFile<'a, V6> {
    fn to_writer(&self, mut writer: impl std::io::Write) -> std::io::Result<()> {
        let mut yaml = YamlWriter::new(&mut writer);
        let mut root = yaml.root();

        // Write the version to the document.
        root.number("version", f64::from(self.version as u16))?;

        // Write the individual environments
        root.table("environments", |tbl| {
            for (name, env) in &self.environments {
                tbl.table(name, |tbl| env.write_to_yaml(tbl))?;
            }
            Ok(())
        })?;

        // Write all the packages to the document.
        root.sequence("packages", |seq| {
            for package in self.packages.iter() {
                let package = SerializablePackageDataV6::from(*package);
                seq.table(|tbl| {
                    match package {
                        SerializablePackageDataV6::Conda(p) => p.write_to_yaml(tbl)?,
                        SerializablePackageDataV6::Pypi(p) => p.write_to_yaml(tbl)?,
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;

        Ok(())
    }
}

impl<'a> SerializableEnvironment<'a> {
    fn write_to_yaml<W: Write>(&self, tbl: &mut YamlTable<'_, W>) -> std::io::Result<()> {
        // Write the channels to the document.
        if self.channels.is_empty() {
            tbl.inline_sequence("channels", |_| Ok(()))?;
        } else {
            tbl.sequence("channels", |seq| {
                for channel in self.channels.iter() {
                    seq.table(|tbl| {
                        tbl.string("url", channel.url.as_str())?;
                        if !channel.used_env_vars.is_empty() {
                            tbl.inline_sequence("used_env_vars", |seq| {
                                for var in channel.used_env_vars.iter() {
                                    seq.string(var.as_str())?;
                                }
                                Ok(())
                            })?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
        }

        // Write the indexes to the document if specified.
        if let Some(indexes) = self.indexes {
            if indexes.indexes.is_empty() {
                tbl.inline_sequence("indexes", |_| Ok(()))?;
            } else {
                tbl.sequence("indexes", |seq| {
                    for index in indexes.indexes.iter() {
                        seq.string(index.as_str())?;
                    }
                    Ok(())
                })?;
            }
            if !indexes.find_links.is_empty() {
                tbl.sequence("find-links", |seq| {
                    for find_link in indexes.find_links.iter() {
                        seq.table(|tbl| match find_link {
                            FindLinksUrlOrPath::Path(path) => {
                                tbl.string("path", &path.to_string_lossy())
                            }
                            FindLinksUrlOrPath::Url(url) => tbl.string("url", url.as_str()),
                        })?;
                    }
                    Ok(())
                })?;
            }
        }

        // Write the packages to the document.
        tbl.table("packages", |platforms| {
            for (platform, pkgs) in self.packages.iter() {
                platforms.sequence(platform.as_str(), |packages| {
                    for pkg in pkgs {
                        pkg.write_to_yaml(packages)?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;

        Ok(())
    }
}

impl<'a> SerializablePackageSelector<'a> {
    fn write_to_yaml<W: Write>(
        &self,
        packages: &mut YamlSequence<'_, W>,
    ) -> std::io::Result<()> {
        match self {
            SerializablePackageSelector::Conda {
                conda,
                name,
                version,
                build,
                subdir,
            } => {
                let version = version.map(|v| v.as_str());
                match [
                    ("conda", Some(conda.as_str())),
                    ("name", name.map(rattler_conda_types::PackageName::as_normalized)),
                    ("version", version.as_deref()),
                    ("build", *build),
                    ("subdir", *subdir),
                ]
                .into_iter()
                .filter_map(|(k, v)| v.map(|v| (k, v)))
                .exactly_one()
                {
                    Ok((k, v)) => {
                        packages.table(|tbl| {
                            tbl.string(k, v)?;
                            Ok(())
                        })?;
                    }
                    Err(elems) => packages.inline_table(|tbl| {
                        for (k, v) in elems {
                            tbl.string(k, v)?;
                        }
                        Ok(())
                    })?,
                };
            }
            SerializablePackageSelector::Pypi { pypi, extras } => {
                if extras.is_empty() {
                    packages.table(|tbl| {
                        tbl.string("pypi", pypi.as_str())?;
                        Ok(())
                    })?;
                } else {
                    packages.inline_table(|tbl| {
                        tbl.string("pypi", pypi.as_str())?;
                        tbl.inline_sequence("extras", |seq| {
                            for extra in extras.iter() {
                                seq.string(extra.as_ref())?;
                            }
                            Ok(())
                        })?;
                        Ok(())
                    })?;
                }
            }
        }
        Ok(())
    }
}

impl<'a> v6::CondaPackageDataModel<'a> {
    fn write_to_yaml<W: Write>(&self, packages: &mut YamlTable<'_, W>) -> std::io::Result<()> {
        packages.string("conda", self.location.as_str())?;
        if let Some(name) = &self.name {
            packages.string("name", name.as_source())?;
        }
        if let Some(version) = &self.version {
            packages.string("version", &version.as_str())?;
        }
        if let Some(build) = &self.build {
            packages.string("build", build)?;
        }
        if let Some(build_number) = &self.build_number {
            packages.number("build_number", *build_number as f64)?;
        }
        if let Some(subdir) = &self.subdir {
            packages.string("subdir", subdir.as_str())?;
        }
        if let Some(noarch) = &self.noarch {
            match &noarch.0 {
                None => packages.boolean("noarch", false)?,
                Some(RawNoArchType::GenericV1) => packages.boolean("noarch", true)?,
                Some(RawNoArchType::GenericV2) => packages.string("noarch", "generic")?,
                Some(RawNoArchType::Python) => packages.string("noarch", "python")?,
            }
        }
        if let Some(sha256) = &self.sha256 {
            packages.string("sha256", &format!("{sha256:x}"))?;
        }
        if let Some(md5) = &self.md5 {
            packages.string("md5", &format!("{md5:x}"))?;
        }
        if let Some(legacy_bz2_md5) = &self.legacy_bz2_md5 {
            packages.string("legacy_bz2_md5", &format!("{legacy_bz2_md5:x}"))?;
        }
        if !self.depends.is_empty() {
            packages.inline_sequence("depends", |seq| {
                for dep in self.depends.iter() {
                    seq.string(dep)?;
                }
                Ok(())
            })?;
        }
        if !self.constrains.is_empty() {
            packages.inline_sequence("constrains", |seq| {
                for constr in self.constrains.iter() {
                    seq.string(constr)?;
                }
                Ok(())
            })?;
        }
        if let Some(arch) = self.arch.as_deref() {
            match arch {
                None => packages.null("arch")?,
                Some(arch) => packages.string("arch", arch.as_str())?,
            }
        }
        if let Some(platform) = self.platform.as_deref() {
            match platform {
                None => packages.null("platform")?,
                Some(platform) => packages.string("platform", platform.as_str())?,
            }
        }
        if let Some(channel) = self.channel.as_deref() {
            match channel {
                None => packages.null("channel")?,
                Some(channel) => packages.string("channel", channel.as_str())?,
            }
        }
        if let Some(features) = AsRef::as_ref(&self.features) {
            packages.string("features", features.as_str())?;
        }
        if !self.track_features.is_empty() {
            packages.inline_sequence("track_features", |seq| {
                for feature in self.track_features.iter() {
                    seq.string(feature.as_str())?;
                }
                Ok(())
            })?;
        }
        if let Some(file_name) = &self.file_name {
            match AsRef::as_ref(file_name) {
                Some(file_name) => {
                    packages.string("file_name", file_name.as_str())?;
                }
                None => {
                    packages.null("file_name")?;
                }
            }
        }
        if let Some(license) = AsRef::as_ref(&self.license) {
            packages.string("license", license.as_str())?;
        }
        if let Some(license_family) = AsRef::as_ref(&self.license_family) {
            packages.string("license_family", license_family.as_str())?;
        }
        if let Some(purls) = AsRef::as_ref(&self.purls) {
            if purls.is_empty() {
                packages.inline_sequence("purls", |_| Ok(()))?;
            } else {
                packages.sequence("purls", |seq| {
                    for purl in purls.iter() {
                        seq.string(&purl.to_string())?;
                    }
                    Ok(())
                })?;
            }
        }
        if let Some(size) = AsRef::as_ref(&self.size) {
            packages.number("size", *size as f64)?;
        }
        if let Some(legacy_bz2_size) = AsRef::as_ref(&self.legacy_bz2_size) {
            packages.number("legacy_bz2_size", *legacy_bz2_size as f64)?;
        }
        if let Some(timestamp) = &self.timestamp {
            packages.number("timestamp", timestamp.timestamp_millis() as f64)?;
        }
        if let Some(input) = &self.input {
            packages.table("input", |tbl| {
                tbl.string("hash", &format!("{:x}", input.hash))?;
                tbl.inline_sequence("globs", |seq| {
                    for value in input.globs.iter() {
                        seq.string(value.as_str())?;
                    }
                    Ok(())
                })?;
                Ok(())
            })?;
        }
        if let Some(python_site_packages_path) = AsRef::as_ref(&self.python_site_packages_path) {
            packages.string(
                "python_site_packages_path",
                python_site_packages_path.as_str(),
            )?;
        }
        Ok(())
    }
}

impl<'a> v6::PypiPackageDataModel<'a> {
    fn write_to_yaml<W: Write>(&self, tbl: &mut YamlTable<'_, W>) -> std::io::Result<()> {
        tbl.string("pypi", self.location.as_str())?;
        tbl.string("name", &self.name.to_string())?;
        tbl.string("version", &self.version.to_string())?;
        if let Some(md5) = AsRef::as_ref(&self.hash)
            .as_ref()
            .and_then(|hash| hash.md5())
        {
            tbl.string("md5", &format!("{md5:x}"))?;
        }
        if let Some(sha256) = AsRef::as_ref(&self.hash)
            .as_ref()
            .and_then(|hash| hash.sha256())
        {
            tbl.string("sha256", &format!("{sha256:x}"))?;
        }
        if !self.requires_dist.is_empty() {
            tbl.sequence("requires_dist", |seq| {
                for req in self.requires_dist.iter() {
                    seq.string(&req.to_string())?;
                }
                Ok(())
            })?;
        }
        if let Some(requires_python) = AsRef::as_ref(&self.requires_python) {
            tbl.string("requires_python", &requires_python.to_string())?;
        }
        if self.editable {
            tbl.boolean("editable", self.editable)?;
        }
        Ok(())
    }
}
