//! TEMPORARY: captures ONE resolvo `DependencySnapshot` of the complete
//! conda-forge channel (linux-64 + noarch) plus a deterministic sample of
//! root packages, so the solver core can be benchmarked over a large corpus
//! of real solves. Not for merge.

use std::{collections::HashMap, fs::File, io::BufWriter, path::PathBuf};

use rattler_conda_types::{Channel, ChannelConfig, NamelessMatchSpec, PackageName, RepoDataRecord};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};
use rattler_solve::{
    ChannelPriority,
    resolvo::{CondaDependencyProvider, NameType},
};
use resolvo::{DenseIndex, NameId, VersionSetId, snapshot::DependencySnapshot};

fn read_sparse_repodata(path: &str) -> SparseRepoData {
    SparseRepoData::from_file(
        Channel::from_str(
            "dummy",
            &ChannelConfig::default_with_root_dir(std::env::current_dir().unwrap()),
        )
        .unwrap(),
        "dummy".to_string(),
        path,
        None,
    )
    .unwrap()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out_dir: PathBuf = args
        .next()
        .expect("usage: capture_corpus <out-dir> [sample-count]")
        .into();
    let sample_count: usize = args.next().map_or(1000, |s| s.parse().unwrap());
    std::fs::create_dir_all(&out_dir).unwrap();

    let base = format!(
        "{}/../../test-data/channels/conda-forge",
        env!("CARGO_MANIFEST_DIR")
    );

    eprintln!("loading full channel...");
    let records: Vec<Vec<RepoDataRecord>> = [
        format!("{base}/linux-64/repodata.json"),
        format!("{base}/noarch/repodata.json"),
    ]
    .iter()
    .map(|path| {
        read_sparse_repodata(path)
            .load_all_records(PackageFormatSelection::default())
            .expect("failed to load records")
    })
    .collect();
    eprintln!(
        "loaded {} records",
        records.iter().map(Vec::len).sum::<usize>()
    );

    let provider = CondaDependencyProvider::new(
        records.iter().map(|r| r.iter().collect()),
        &[],
        &[],
        &[],
        &[],
        None,
        None,
        ChannelPriority::default(),
        None,
        rattler_solve::SolveStrategy::Highest,
        Vec::new(),
        &HashMap::default(),
    )
    .expect("failed to create dependency provider");

    // All unique package names, sorted for determinism.
    let mut names: Vec<&str> = records
        .iter()
        .flatten()
        .map(|r| r.package_record.name.as_normalized())
        .collect();
    names.sort_unstable();
    names.dedup();
    eprintln!("{} unique package names", names.len());

    let name_ids: Vec<NameId> = names
        .iter()
        .map(|name| {
            let package_name = PackageName::try_from(*name).unwrap();
            provider
                .pool
                .intern_package_name(NameType::from(&package_name))
        })
        .collect();

    // Deterministic, evenly spaced sample of root packages. Each root is a
    // bare "any version" requirement on the package.
    let mut roots: Vec<(String, usize)> = Vec::with_capacity(sample_count);
    let mut root_version_sets: Vec<VersionSetId> = Vec::with_capacity(sample_count);
    for i in 0..sample_count.min(names.len()) {
        let idx = i * names.len() / sample_count.min(names.len());
        let name_id = name_ids[idx];
        let version_set = provider
            .pool
            .intern_version_set(name_id, NamelessMatchSpec::default().into());
        roots.push((names[idx].to_string(), version_set.to_index()));
        root_version_sets.push(version_set);
    }

    eprintln!("capturing snapshot of the full channel...");
    let capture_start = std::time::Instant::now();
    let snapshot = DependencySnapshot::from_provider(
        provider,
        name_ids,
        root_version_sets,
        std::iter::empty::<resolvo::SolvableId>(),
    )
    .expect("failed to capture snapshot");
    eprintln!(
        "captured {} solvables, {} version sets in {:.1}s",
        snapshot.solvables.len(),
        snapshot.version_sets.len(),
        capture_start.elapsed().as_secs_f64()
    );

    let snapshot_path = out_dir.join("conda-forge.snapshot.json");
    serde_json::to_writer(
        BufWriter::new(File::create(&snapshot_path).unwrap()),
        &snapshot,
    )
    .expect("failed to serialize snapshot");
    std::fs::write(
        out_dir.join("conda-forge.roots.json"),
        serde_json::to_string(&roots).unwrap(),
    )
    .unwrap();
    eprintln!("wrote {}", snapshot_path.display());
}
