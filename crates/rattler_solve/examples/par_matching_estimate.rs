//! TEMPORARY: estimates the ceiling of solve-wide parallel matchspec parsing
//! and matching — the work a `Sync` resolvo pool would allow to be batched and
//! parallelized. Not for merge.

use std::{collections::HashMap, hint::black_box, path::Path, time::Instant};

use rattler_conda_types::{
    Channel, MatchSpec, Matches, NamelessMatchSpec, PackageName, RepoDataRecord,
};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};
use rayon::prelude::*;

fn measure(sparse_repo_data: &SparseRepoData, root: &str) {
    let package_name = PackageName::try_from(root).unwrap();
    let repodata = SparseRepoData::load_records_recursive(
        [sparse_repo_data],
        [package_name],
        None,
        PackageFormatSelection::default(),
    )
    .expect("failed to load records");
    let records: Vec<&RepoDataRecord> = repodata.iter().flatten().collect();

    // Index candidates by package name, mimicking the pool's per-name lists.
    let mut by_name: HashMap<&str, Vec<&RepoDataRecord>> = HashMap::new();
    for record in &records {
        by_name
            .entry(record.package_record.name.as_normalized())
            .or_default()
            .push(record);
    }

    // Collect the unique dependency strings, as the parse cache would see them.
    let mut unique_specs: Vec<&str> = records
        .iter()
        .flat_map(|r| r.package_record.depends.iter())
        .map(String::as_str)
        .collect();
    unique_specs.sort_unstable();
    unique_specs.dedup();

    // ---

    let parse =
        |s: &str| MatchSpec::from_str(s, rattler_conda_types::ParseStrictness::Lenient).ok();

    let t = Instant::now();
    let parsed_seq: Vec<Option<MatchSpec>> = unique_specs.iter().map(|s| parse(s)).collect();
    let parse_seq = t.elapsed();
    black_box(&parsed_seq);

    let t = Instant::now();
    let parsed_par: Vec<Option<MatchSpec>> = unique_specs.par_iter().map(|s| parse(s)).collect();
    let parse_par = t.elapsed();
    black_box(&parsed_par);

    // --- matching: for every unique parsed spec, scan its name's candidates
    // (what `filter_candidates` / `find_highest_version` do per version set).

    let specs: Vec<(&str, NamelessMatchSpec)> = parsed_seq
        .iter()
        .flatten()
        .filter_map(|spec| {
            let name = spec.name.as_exact()?.as_normalized().to_owned();
            let (_, nameless) = spec.clone().into_nameless();
            by_name
                .get_key_value(name.as_str())
                .map(|(k, _)| (*k, nameless))
        })
        .collect();

    let match_all = |(name, spec): &(&str, NamelessMatchSpec)| -> usize {
        by_name.get(name).map_or(0, |candidates| {
            candidates.iter().filter(|r| spec.matches(**r)).count()
        })
    };

    let t = Instant::now();
    let matched_seq: usize = specs.iter().map(match_all).sum();
    let match_seq = t.elapsed();

    let t = Instant::now();
    let matched_par: usize = specs.par_iter().map(match_all).sum();
    let match_par = t.elapsed();
    assert_eq!(matched_seq, matched_par);

    println!(
        "=== {root}: {} records, {} unique dep specs, {} matchable specs, {} total matches ===",
        records.len(),
        unique_specs.len(),
        specs.len(),
        matched_seq,
    );
    println!(
        "parse:  seq {:>8.3} ms | par {:>8.3} ms | speedup {:.2}x",
        parse_seq.as_secs_f64() * 1e3,
        parse_par.as_secs_f64() * 1e3,
        parse_seq.as_secs_f64() / parse_par.as_secs_f64()
    );
    println!(
        "match:  seq {:>8.3} ms | par {:>8.3} ms | speedup {:.2}x",
        match_seq.as_secs_f64() * 1e3,
        match_par.as_secs_f64() * 1e3,
        match_seq.as_secs_f64() / match_par.as_secs_f64()
    );
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

    println!(
        "rayon threads: {}\n",
        rayon::current_num_threads()
    );

    for root in ["pytorch", "python", "tensorflow", "quetz"] {
        measure(&sparse_repo_data, root);
    }
}
