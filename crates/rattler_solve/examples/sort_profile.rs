//! TEMPORARY: profiles the candidate sorting stages. Not for merge.

use std::{collections::HashMap, hint::black_box, path::Path, time::Instant};

use futures::FutureExt;
use rattler_conda_types::{Channel, MatchSpec};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};
use rattler_solve::{
    ChannelPriority,
    resolvo::{CondaDependencyProvider, NameType, sort_stats},
};
use resolvo::SolverCache;

fn profile_sort(sparse_repo_data: &SparseRepoData, spec: &str) {
    let match_spec =
        MatchSpec::from_str(spec, rattler_conda_types::ParseStrictness::Lenient).unwrap();
    let package_name = match_spec.name.as_exact().unwrap().clone();

    let repodata = SparseRepoData::load_records_recursive(
        [sparse_repo_data],
        [package_name.clone()],
        None,
        PackageFormatSelection::default(),
    )
    .expect("failed to load records");

    let n_records: usize = repodata.iter().map(Vec::len).sum();

    // Warm up + measure over several iterations
    const ITERS: usize = 20;
    let mut construct_total = 0f64;
    let mut sort_total = 0f64;
    sort_stats::reset();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let dependency_provider = CondaDependencyProvider::new(
            repodata.iter().map(|r| r.iter().collect()),
            &[],
            &[],
            &[],
            std::slice::from_ref(&match_spec),
            None,
            None,
            ChannelPriority::default(),
            None,
            rattler_solve::SolveStrategy::Highest,
            Vec::new(),
            &HashMap::default(),
        )
        .expect("failed to create dependency provider");
        let t1 = t0.elapsed();

        let name = dependency_provider
            .pool
            .intern_package_name(NameType::from(&package_name));
        let version_set = dependency_provider
            .pool
            .intern_version_set(name, match_spec.clone().into_nameless().1.into());

        let cache = SolverCache::new(dependency_provider);

        let t2 = Instant::now();
        let deps = cache
            .get_or_cache_sorted_candidates(version_set.into())
            .now_or_never()
            .expect("failed to get candidates")
            .expect("solver requested cancellation");
        let t3 = t2.elapsed();
        black_box(deps);

        construct_total += t1.as_secs_f64() * 1e3;
        sort_total += t3.as_secs_f64() * 1e3;
    }

    println!("=== {spec} ({n_records} records loaded) ===");
    println!(
        "provider construction: {:.3} ms/iter\nget_or_cache_sorted_candidates: {:.3} ms/iter",
        construct_total / ITERS as f64,
        sort_total / ITERS as f64
    );
    println!("--- accumulated over {ITERS} iters (divide by {ITERS}) ---");
    println!("{}", sort_stats::report());
    println!();
}

fn main() {
    let channel_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("test-data")
        .join("channels")
        .join("conda-forge");
    let repodata_json_path = channel_path.join("linux-64").join("repodata.json");
    let channel = Channel::try_from_directory(&channel_path).unwrap();

    let sparse_repo_data = SparseRepoData::from_file(channel, "linux-64", repodata_json_path, None)
        .expect("failed to load sparse repodata");

    profile_sort(&sparse_repo_data, "pytorch");
    profile_sort(&sparse_repo_data, "python");
    profile_sort(&sparse_repo_data, "tensorflow");
}
