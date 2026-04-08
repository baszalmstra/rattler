//! Comprehensive benchmarks for MatchSpec and NamelessMatchSpec.
//!
//! Benchmarks cover:
//! - Parsing (from string)
//! - Matching against PackageRecords
//! - Cloning
//! - Memory footprint measurement
//! - Hashing

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rattler_conda_types::{
    MatchSpec, Matches, NamelessMatchSpec, PackageRecord, ParseStrictness, RepoData,
};

/// Load repodata and return (unique_dep_strings, package_records).
fn load_test_data() -> (Vec<String>, Vec<PackageRecord>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-data/channels/pytorch/linux-64/repodata.json");
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read repodata at {}: {e}", path.display()));
    let repodata: RepoData =
        serde_json::from_str(&contents).unwrap_or_else(|e| panic!("failed to parse repodata: {e}"));

    let mut unique_deps = HashSet::new();
    let mut records = Vec::new();

    for record in repodata
        .packages
        .values()
        .chain(repodata.conda_packages.values())
    {
        for dep in &record.depends {
            unique_deps.insert(dep.clone());
        }
        for con in &record.constrains {
            unique_deps.insert(con.clone());
        }
        records.push(record.clone());
    }

    let mut specs: Vec<String> = unique_deps.into_iter().collect();
    specs.sort();
    (specs, records)
}

fn matchspec_benchmarks(c: &mut Criterion) {
    let (dep_strings, records) = load_test_data();

    // Parse all matchspecs upfront for non-parse benchmarks
    let matchspecs: Vec<MatchSpec> = dep_strings
        .iter()
        .filter_map(|s| MatchSpec::from_str(s, ParseStrictness::Lenient).ok())
        .collect();

    let nameless_specs: Vec<NamelessMatchSpec> = matchspecs
        .iter()
        .cloned()
        .map(|ms| ms.into_nameless().1)
        .collect();

    eprintln!(
        "Test data: {} unique dep strings, {} matchspecs, {} records",
        dep_strings.len(),
        matchspecs.len(),
        records.len()
    );

    // Print struct sizes for reference
    eprintln!(
        "MatchSpec size: {} bytes, NamelessMatchSpec size: {} bytes",
        std::mem::size_of::<MatchSpec>(),
        std::mem::size_of::<NamelessMatchSpec>()
    );

    // --- Parse benchmarks ---
    {
        let mut group = c.benchmark_group("parse");
        group.throughput(Throughput::Elements(dep_strings.len() as u64));
        group.sample_size(20);

        group.bench_function("MatchSpec_lenient", |b| {
            b.iter(|| {
                for s in &dep_strings {
                    let _ = black_box(MatchSpec::from_str(black_box(s), ParseStrictness::Lenient));
                }
            });
        });

        group.bench_function("MatchSpec_strict", |b| {
            b.iter(|| {
                for s in &dep_strings {
                    let _ = black_box(MatchSpec::from_str(black_box(s), ParseStrictness::Strict));
                }
            });
        });

        group.finish();
    }

    // --- Match benchmarks ---
    // This simulates the solver hot path: for each record, check if it matches
    // a set of dependency constraints.
    {
        // Use a subset of records and specs for realistic matching workload
        let sample_records: Vec<&PackageRecord> = records.iter().take(200).collect();
        let sample_specs: Vec<&NamelessMatchSpec> = nameless_specs.iter().take(100).collect();

        let match_count = (sample_records.len() * sample_specs.len()) as u64;

        let mut group = c.benchmark_group("match");
        group.throughput(Throughput::Elements(match_count));
        group.sample_size(20);

        group.bench_function("NamelessMatchSpec_vs_PackageRecord", |b| {
            b.iter(|| {
                let mut count = 0u32;
                for record in &sample_records {
                    for spec in &sample_specs {
                        if spec.matches(*record) {
                            count += 1;
                        }
                    }
                }
                black_box(count)
            });
        });

        group.finish();
    }

    // --- Clone benchmarks ---
    {
        let mut group = c.benchmark_group("clone");
        group.throughput(Throughput::Elements(nameless_specs.len() as u64));
        group.sample_size(20);

        group.bench_function("NamelessMatchSpec", |b| {
            b.iter(|| {
                for spec in &nameless_specs {
                    let _ = black_box(spec.clone());
                }
            });
        });

        group.bench_function("MatchSpec", |b| {
            b.iter(|| {
                for spec in &matchspecs {
                    let _ = black_box(spec.clone());
                }
            });
        });

        group.finish();
    }

    // --- Hash benchmarks ---
    {
        let mut group = c.benchmark_group("hash");
        group.throughput(Throughput::Elements(nameless_specs.len() as u64));
        group.sample_size(20);

        group.bench_function("NamelessMatchSpec", |b| {
            b.iter(|| {
                for spec in &nameless_specs {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    spec.hash(&mut hasher);
                    black_box(hasher.finish());
                }
            });
        });

        group.finish();
    }

    // --- Memory footprint ---
    {
        let mut group = c.benchmark_group("memory");
        group.sample_size(20);

        // Measure memory for creating N matchspecs
        let n = nameless_specs.len();
        group.throughput(Throughput::Elements(n as u64));

        group.bench_function("allocate_NamelessMatchSpec_vec", |b| {
            b.iter(|| {
                let v: Vec<NamelessMatchSpec> = nameless_specs.clone();
                black_box(&v);
                // Return size info
                black_box(v.len() * std::mem::size_of::<NamelessMatchSpec>())
            });
        });

        group.bench_function("allocate_MatchSpec_vec", |b| {
            b.iter(|| {
                let v: Vec<MatchSpec> = matchspecs.clone();
                black_box(&v);
                black_box(v.len() * std::mem::size_of::<MatchSpec>())
            });
        });

        group.finish();

        // Print memory stats
        let shallow_nameless = n * std::mem::size_of::<NamelessMatchSpec>();
        let shallow_matchspec = n * std::mem::size_of::<MatchSpec>();
        eprintln!(
            "Shallow memory for {} NamelessMatchSpecs: {} KB ({} bytes each)",
            n,
            shallow_nameless / 1024,
            std::mem::size_of::<NamelessMatchSpec>()
        );
        eprintln!(
            "Shallow memory for {} MatchSpecs: {} KB ({} bytes each)",
            n,
            shallow_matchspec / 1024,
            std::mem::size_of::<MatchSpec>()
        );
    }
}

criterion_group!(benches, matchspec_benchmarks);
criterion_main!(benches);
