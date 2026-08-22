//! TEMPORARY: samples end-to-end resolvo solves with pprof and prints the
//! top functions by self time, plus writes a flamegraph. Not for merge.
#![cfg(target_os = "linux")]

use std::{collections::HashMap, hint::black_box, path::Path};

use rattler_conda_types::ParseStrictness::Strict;
use rattler_conda_types::{Channel, ChannelConfig, MatchSpec};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};
use rattler_solve::{SolverImpl, SolverTask};

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
    let base = format!(
        "{}/../../test-data/channels/conda-forge",
        env!("CARGO_MANIFEST_DIR")
    );
    let sparse_repo_data = vec![
        read_sparse_repodata(&format!("{base}/linux-64/repodata.json")),
        read_sparse_repodata(&format!("{base}/noarch/repodata.json")),
    ];

    let env_specs: Vec<(&str, Vec<&str>)> = vec![
        ("tensorflow", vec!["tensorflow"]),
        ("quetz", vec!["quetz"]),
        ("tensorboard-grpc", vec!["tensorboard=2.1.1", "grpc-cpp=1.39.1"]),
    ];

    for (label, specs) in env_specs {
        let specs = specs
            .iter()
            .map(|s| MatchSpec::from_str(s, Strict).unwrap())
            .collect::<Vec<MatchSpec>>();
        let names = specs.iter().map(|s| s.name.as_exact().unwrap().clone());
        let available_packages = SparseRepoData::load_records_recursive(
            &sparse_repo_data,
            names,
            None,
            PackageFormatSelection::default(),
        )
        .unwrap();

        // Warm up once outside the profiler.
        let result = rattler_solve::resolvo::Solver
            .solve(SolverTask {
                specs: specs.clone(),
                ..SolverTask::from_iter(&available_packages)
            })
            .unwrap();
        black_box(result);

        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(2000)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap();

        const ITERS: usize = 20;
        let t = std::time::Instant::now();
        for _ in 0..ITERS {
            let result = rattler_solve::resolvo::Solver
                .solve(black_box(SolverTask {
                    specs: specs.clone(),
                    ..SolverTask::from_iter(&available_packages)
                }))
                .unwrap();
            black_box(result);
        }
        let elapsed = t.elapsed();

        let report = guard.report().build().unwrap();

        // Aggregate self samples per innermost meaningful frame.
        let mut self_counts: HashMap<String, isize> = HashMap::new();
        let mut total: isize = 0;
        for (frames, count) in &report.data {
            total += *count as isize;
            if let Some(frame) = frames.frames.iter().flatten().next() {
                let name = frame.name().to_string();
                *self_counts.entry(name).or_default() += *count as isize;
            }
        }
        let mut top: Vec<(String, isize)> = self_counts.into_iter().collect();
        top.sort_by_key(|(_, c)| -*c);

        println!(
            "==== {label}: {ITERS} solves in {:.1} ms ({:.1} ms/solve), {total} samples ====",
            elapsed.as_secs_f64() * 1e3,
            elapsed.as_secs_f64() * 1e3 / ITERS as f64
        );
        for (name, count) in top.iter().take(30) {
            println!(
                "{:6.2}%  {}",
                *count as f64 * 100.0 / total as f64,
                &name[..name.len().min(120)]
            );
        }
        println!();

        let file = std::fs::File::create(format!(
            "/tmp/claude-0/-home-user-rattler/30d079d1-e065-504b-adb5-ba4b11afa7a9/scratchpad/flame-{label}.svg"
        ))
        .unwrap();
        report.flamegraph(file).unwrap();
    }
}
