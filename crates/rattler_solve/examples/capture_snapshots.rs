//! TEMPORARY: captures resolvo `DependencySnapshot`s of the benchmark
//! environments so the solver core can be benchmarked and validated on
//! frozen inputs, independent of the conda ecosystem code. Not for merge.

use std::{collections::HashMap, fs::File, io::BufWriter, path::PathBuf};

use rattler_conda_types::ParseStrictness::Strict;
use rattler_conda_types::{Channel, ChannelConfig, MatchSpec};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};
use rattler_solve::{
    ChannelPriority,
    resolvo::{CondaDependencyProvider, NameType},
};
use resolvo::{DenseIndex, VersionSetId, snapshot::DependencySnapshot};

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
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .expect("usage: capture_snapshots <out-dir>")
        .into();
    std::fs::create_dir_all(&out_dir).unwrap();

    let base = format!(
        "{}/../../test-data/channels/conda-forge",
        env!("CARGO_MANIFEST_DIR")
    );
    let sparse_repo_data = vec![
        read_sparse_repodata(&format!("{base}/linux-64/repodata.json")),
        read_sparse_repodata(&format!("{base}/noarch/repodata.json")),
    ];

    let envs: Vec<(&str, Vec<&str>)> = vec![
        ("python", vec!["python=3.9"]),
        ("xtensor-xsimd", vec!["xtensor", "xsimd"]),
        ("tensorflow", vec!["tensorflow"]),
        ("quetz", vec!["quetz"]),
        ("tensorboard-grpc", vec!["tensorboard=2.1.1", "grpc-cpp=1.39.1"]),
    ];

    for (label, specs) in envs {
        let specs: Vec<MatchSpec> = specs
            .iter()
            .map(|s| MatchSpec::from_str(s, Strict).unwrap())
            .collect();
        let names = specs.iter().map(|s| s.name.as_exact().unwrap().clone());
        let available_packages = SparseRepoData::load_records_recursive(
            &sparse_repo_data,
            names,
            None,
            PackageFormatSelection::default(),
        )
        .unwrap();

        let provider = CondaDependencyProvider::new(
            available_packages
                .iter()
                .map(|records| records.iter().collect()),
            &[],
            &[],
            &[],
            &specs,
            None,
            None,
            ChannelPriority::default(),
            None,
            rattler_solve::SolveStrategy::Highest,
            Vec::new(),
            &HashMap::default(),
        )
        .expect("failed to create dependency provider");

        // Intern the root match specs exactly like the real solve does, and
        // remember their version set ids. Snapshot capture preserves ids, so
        // the same ids address the snapshot.
        let root_version_sets: Vec<VersionSetId> = specs
            .iter()
            .map(|spec| {
                let (name, nameless) = spec.clone().into_nameless();
                let name = name.as_exact().unwrap();
                let name_id = provider.pool.intern_package_name(NameType::from(name));
                provider.pool.intern_version_set(name_id, nameless.into())
            })
            .collect();

        let snapshot = DependencySnapshot::from_provider(
            provider,
            std::iter::empty::<resolvo::NameId>(),
            root_version_sets.clone(),
            std::iter::empty::<resolvo::SolvableId>(),
        )
        .expect("failed to capture snapshot");

        let snapshot_path = out_dir.join(format!("{label}.snapshot.json"));
        serde_json::to_writer(
            BufWriter::new(File::create(&snapshot_path).unwrap()),
            &snapshot,
        )
        .expect("failed to serialize snapshot");

        let roots: Vec<usize> = root_version_sets.iter().map(|v| v.to_index()).collect();
        std::fs::write(
            out_dir.join(format!("{label}.roots.json")),
            serde_json::to_string(&roots).unwrap(),
        )
        .unwrap();

        println!(
            "{label}: {} solvables, {} version sets -> {}",
            snapshot.solvables.len(),
            snapshot.version_sets.len(),
            snapshot_path.display()
        );
    }
}
