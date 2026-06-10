use std::{
    collections::{HashMap, HashSet},
    io::BufWriter,
    path::Path,
    str::FromStr,
};

use clap::Parser;
use itertools::Itertools;
use rattler_conda_types::{
    Channel, ChannelConfig, GenericVirtualPackage, MatchSpec, PackageName, ParseStrictness,
    Platform, Version,
};
use rattler_networking::LazyClient;
use rattler_repodata_gateway::fetch::FetchRepoDataOptions;
use rattler_solve::{
    ChannelPriority, SolveStrategy,
    resolvo::{CondaDependencyProvider, NameType},
};
use resolvo::{
    Condition, Dependencies, DependencyProvider, DenseIndex, EnvironmentPackage, Interner, NameId,
    PackageCandidates, Requirement, VersionSetId, VersionSetRelation,
};

#[derive(Parser)]
#[clap(about)]
struct Args {
    /// The channel to make the snapshot for.
    channel: String,

    /// The subdirs to query.
    #[clap(short, long, num_args=1..)]
    subdir: Vec<Platform>,

    /// The output path
    #[clap(short)]
    output: Option<String>,

    /// Inject a concrete virtual package describing the simulated machine,
    /// e.g. `--machine "__glibc=2.35"` or `--machine "__archspec=1=x86_64_v3"`
    /// (name=version or name=version=build). These become the candidates the
    /// plain solver sees in concrete benchmark mode.
    #[clap(long)]
    machine: Vec<String>,

    /// Mark a package as an environment package for universal solving, e.g.
    /// `--symbolic __glibc` or `--symbolic __cuda:absent` (the `:absent`
    /// suffix marks the package as absentable). The package keeps its
    /// concrete machine candidates in the snapshot; providers in universal
    /// mode present it as an environment package instead.
    #[clap(long)]
    symbolic: Vec<String>,

    /// Intern an extra version set targeting an environment package into the
    /// snapshot and its relation table, e.g.
    /// `--env-spec "__glibc >=2.17,<3.0a0"`. Use this for environment-model
    /// literals that do not appear verbatim among repodata dependencies.
    #[clap(long)]
    env_spec: Vec<String>,
}

/// Parses a `--machine` argument of the form `name=version` or
/// `name=version=build`.
fn parse_machine(arg: &str) -> GenericVirtualPackage {
    let mut parts = arg.splitn(3, '=');
    let name = parts.next().expect("a virtual package name");
    let version = parts
        .next()
        .unwrap_or_else(|| panic!("--machine '{arg}' must have the form name=version[=build]"));
    let build_string = parts.next().unwrap_or("0").to_string();
    GenericVirtualPackage {
        name: PackageName::from_str(name).expect("a valid virtual package name"),
        version: Version::from_str(version).expect("a valid version"),
        build_string,
    }
}

/// Parses a `--symbolic` argument of the form `name` or `name:absent`.
fn parse_symbolic(arg: &str) -> (PackageName, bool) {
    let (name, can_be_absent) = match arg.strip_suffix(":absent") {
        Some(name) => (name, true),
        None => (arg, false),
    };
    (
        PackageName::from_str(name).expect("a valid package name"),
        can_be_absent,
    )
}

/// Recursively collects the version sets referenced by a condition.
fn condition_version_sets(
    provider: &CondaDependencyProvider<'_>,
    condition: resolvo::ConditionId,
    out: &mut Vec<VersionSetId>,
) {
    match provider.resolve_condition(condition) {
        Condition::Requirement(version_set) => out.push(version_set),
        Condition::Binary(_, lhs, rhs) => {
            condition_version_sets(provider, lhs, out);
            condition_version_sets(provider, rhs, out);
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Args = Args::parse();

    // Determine the channel
    let channel = Channel::from_str(
        &args.channel,
        &ChannelConfig::default_with_root_dir(std::env::current_dir().unwrap()),
    )
    .unwrap();

    let virtual_packages: Vec<GenericVirtualPackage> =
        args.machine.iter().map(|arg| parse_machine(arg)).collect();
    let symbolic_packages: Vec<(PackageName, bool)> =
        args.symbolic.iter().map(|arg| parse_symbolic(arg)).collect();

    // Fetch the repodata for all the subdirs.
    let mut subdirs: HashSet<Platform> = HashSet::from_iter(args.subdir);
    if subdirs.is_empty() {
        subdirs.insert(Platform::current());
    }
    subdirs.insert(Platform::NoArch);

    let client = LazyClient::default();
    let mut records = Vec::new();
    for &subdir in &subdirs {
        eprintln!("fetching repodata for {subdir:?}..");
        let repodata = rattler_repodata_gateway::fetch::fetch_repo_data(
            channel.platform_url(subdir),
            client.clone(),
            rattler_cache::default_cache_dir()
                .unwrap()
                .join(rattler_cache::REPODATA_CACHE_DIR),
            FetchRepoDataOptions::default(),
            None,
        )
        .await
        .unwrap();

        eprintln!("parsing repodata..");
        let repodata = rattler_conda_types::RepoData::from_path(repodata.repo_data_json_path)
            .unwrap()
            .into_repo_data_records(&channel);

        records.push(repodata);
    }

    // Create the dependency provider. The provider stays fully concrete (the
    // machine candidates are injected as records); the environment markers
    // are added to the snapshot afterwards so that one snapshot serves both
    // the concrete and the universal benchmark modes.
    let provider = CondaDependencyProvider::new(
        records
            .iter()
            .map(rattler_solve::resolvo::RepoData::from_iter),
        &[],
        &[],
        &virtual_packages,
        &[],
        None,
        None,
        ChannelPriority::default(),
        None,
        SolveStrategy::default(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();

    // Resolve the environment package names.
    let environment_packages: Vec<(NameId, PackageName, bool)> = symbolic_packages
        .iter()
        .map(|(name, can_be_absent)| {
            let name_id = provider.pool.intern_package_name(NameType::from(name));
            (name_id, name.clone(), *can_be_absent)
        })
        .collect();
    let environment_name_ids: HashSet<NameId> = environment_packages
        .iter()
        .map(|&(name_id, _, _)| name_id)
        .collect();

    // Pre-walk all dependencies through the public provider interface. This
    // forces every dependency version set into the pool (so ids are final)
    // and collects the version sets that target environment packages: the
    // domain of the relation table.
    eprintln!("collecting environment version sets..");
    let mut environment_version_sets: Vec<VersionSetId> = Vec::new();
    let mut seen_version_sets: HashSet<VersionSetId> = HashSet::new();
    let mut collect = |provider: &CondaDependencyProvider<'_>, version_set: VersionSetId| {
        if environment_name_ids.contains(&provider.version_set_name(version_set))
            && seen_version_sets.insert(version_set)
        {
            environment_version_sets.push(version_set);
        }
    };
    let package_names = provider.package_names().collect::<Vec<_>>();
    for &name in &package_names {
        let Some(PackageCandidates::Candidates(candidates)) = provider.get_candidates(name).await
        else {
            continue;
        };
        let excluded = candidates.excluded.iter().map(|&(solvable, _)| solvable);
        for solvable in candidates.candidates.iter().copied().chain(excluded) {
            let Dependencies::Known(dependencies) = provider.get_dependencies(solvable).await
            else {
                continue;
            };
            let mut referenced = Vec::new();
            for requirement in &dependencies.requirements {
                if let Some(condition) = requirement.condition {
                    condition_version_sets(&provider, condition, &mut referenced);
                }
                match requirement.requirement {
                    Requirement::Single(version_set) => referenced.push(version_set),
                    Requirement::Union(union) => {
                        referenced.extend(provider.version_sets_in_union(union));
                    }
                }
            }
            referenced.extend(dependencies.constrains.iter().copied());
            for version_set in referenced {
                collect(&provider, version_set);
            }
        }
    }

    // Intern the extra version sets requested on the command line.
    for spec_str in &args.env_spec {
        let spec = MatchSpec::from_str(spec_str, ParseStrictness::Lenient)
            .unwrap_or_else(|err| panic!("invalid --env-spec '{spec_str}': {err}"));
        let (matcher, nameless) = spec.into_nameless();
        let name = matcher
            .as_exact()
            .unwrap_or_else(|| panic!("--env-spec '{spec_str}' must name an exact package"))
            .clone();
        let name_id = provider.pool.intern_package_name(NameType::from(&name));
        if !environment_name_ids.contains(&name_id) {
            panic!("--env-spec '{spec_str}' targets '{}' which is not --symbolic", name.as_normalized());
        }
        let version_set = provider.pool.intern_version_set(name_id, nameless.into());
        eprintln!(
            "env-spec '{spec_str}' -> version set {} with display '{}'",
            version_set.to_index(),
            provider.display_version_set(version_set),
        );
        collect(&provider, version_set);
    }

    // Compute the pairwise relation table per environment package. Only
    // definite answers are stored; missing entries mean Unknown.
    eprintln!(
        "computing relation table over {} environment version sets..",
        environment_version_sets.len()
    );
    let mut by_package: HashMap<NameId, Vec<VersionSetId>> = HashMap::new();
    for &version_set in &environment_version_sets {
        by_package
            .entry(provider.version_set_name(version_set))
            .or_default()
            .push(version_set);
    }
    let mut relations: Vec<(VersionSetId, VersionSetId, VersionSetRelation)> = Vec::new();
    let mut relation_counts: HashMap<&'static str, usize> = HashMap::new();
    for (&name_id, version_sets) in by_package.iter().sorted_by_key(|&(&name_id, _)| name_id) {
        let mut unknown = 0usize;
        for (index, &a) in version_sets.iter().enumerate() {
            for &b in &version_sets[index + 1..] {
                let relation = provider.environment_version_set_relation(a, b);
                let label = match relation {
                    VersionSetRelation::Disjoint => "disjoint",
                    VersionSetRelation::Subset => "subset",
                    VersionSetRelation::Superset => "superset",
                    VersionSetRelation::Equal => "equal",
                    VersionSetRelation::Unknown => {
                        unknown += 1;
                        "unknown"
                    }
                };
                *relation_counts.entry(label).or_default() += 1;
                if relation != VersionSetRelation::Unknown {
                    relations.push((a, b, relation));
                }
            }
        }
        eprintln!(
            "  {}: {} version sets, {} unknown pairs",
            provider.display_name(name_id),
            version_sets.len(),
            unknown,
        );
    }
    eprintln!("relation table: {relation_counts:?}");

    eprintln!("creating snapshot..");
    let mut snapshot = resolvo::snapshot::DependencySnapshot::from_provider(
        provider,
        package_names,
        environment_version_sets,
        [],
    )
    .unwrap();

    // Mark the environment packages and attach the relation table.
    for (name_id, name, can_be_absent) in environment_packages {
        let package = snapshot.packages.get_mut(name_id).unwrap_or_else(|| {
            panic!(
                "environment package '{}' is not part of the snapshot; \
                 inject a --machine candidate for it",
                name.as_normalized()
            )
        });
        package.environment = Some(EnvironmentPackage { can_be_absent });
    }
    snapshot.environment_version_set_relations = relations;

    let output_file = args.output.unwrap_or_else(|| {
        format!(
            "snapshot-{}-{}.json",
            channel.name(),
            subdirs
                .iter()
                .copied()
                .map(Platform::as_str)
                .sorted()
                .join("-")
        )
    });
    eprintln!("serializing snapshot to {}", &output_file);
    let snapshot_path = Path::new(&output_file);
    if let Some(dir) = snapshot_path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let snapshot_file = BufWriter::new(std::fs::File::create(snapshot_path).unwrap());
    serde_json::to_writer(snapshot_file, &snapshot).unwrap();
}
