//! Replays captured resolvo `DependencySnapshot`s: times the solves and dumps
//! the selected solvables so runs against different resolvo builds can be
//! compared for both speed and identical results.
//!
//! Modes:
//!   envs   <corpus-dir> <iters> <results-out>
//!   corpus <snapshot.json> <roots.json> <results-out> <timings-out> [timeout-secs]

use std::{
    fs::File,
    io::{BufReader, Write},
    time::{Duration, Instant, SystemTime},
};

use resolvo::{
    ConditionalRequirement, DenseIndex, Interner, Problem, Solver, VersionSetId,
    snapshot::DependencySnapshot,
};

const ENV_LABELS: [&str; 5] = [
    "python",
    "xtensor-xsimd",
    "tensorflow",
    "quetz",
    "tensorboard-grpc",
];

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("envs") => run_envs(args),
        Some("corpus") => run_corpus(args),
        _ => panic!("usage: snapshot-replay envs|corpus ..."),
    }
}

fn run_envs(mut args: impl Iterator<Item = String>) {
    let dir = args.next().expect("missing corpus dir");
    let iters: usize = args.next().expect("missing iters").parse().unwrap();
    let results_out = args.next().expect("missing results-out");

    let mut results = String::new();
    for label in ENV_LABELS {
        let snapshot: DependencySnapshot = serde_json::from_reader(BufReader::new(
            File::open(format!("{dir}/{label}.snapshot.json")).unwrap(),
        ))
        .unwrap();
        let roots: Vec<usize> = serde_json::from_str(
            &std::fs::read_to_string(format!("{dir}/{label}.roots.json")).unwrap(),
        )
        .unwrap();
        let requirements: Vec<ConditionalRequirement> = roots
            .iter()
            .map(|&i| VersionSetId::from_index(i).into())
            .collect();

        let mut times: Vec<f64> = Vec::new();
        let mut solution: Option<Vec<String>> = None;
        for i in 0..=iters {
            let (elapsed, repr) = solve_once(&snapshot, requirements.clone(), None);
            if i > 0 {
                times.push(elapsed);
            }
            match &solution {
                None => solution = Some(repr),
                Some(previous) => assert_eq!(previous, &repr, "solution changed for {label}"),
            }
        }

        times.sort_by(f64::total_cmp);
        let solution = solution.unwrap();
        println!(
            "{label}: median {:.3} ms | min {:.3} ms | {} iters | {} packages selected",
            times[times.len() / 2],
            times[0],
            times.len(),
            solution.len()
        );
        results.push_str(&format!("=== {label} ===\n{}\n", solution.join("\n")));
    }
    std::fs::write(results_out, results).unwrap();
}

fn run_corpus(mut args: impl Iterator<Item = String>) {
    let snapshot_path = args.next().expect("missing snapshot path");
    let roots_path = args.next().expect("missing roots path");
    let results_out = args.next().expect("missing results-out");
    let timings_out = args.next().expect("missing timings-out");
    let timeout_secs: u64 = args.next().map_or(30, |s| s.parse().unwrap());

    eprintln!("loading snapshot...");
    let load_start = Instant::now();
    let snapshot: DependencySnapshot =
        serde_json::from_reader(BufReader::new(File::open(&snapshot_path).unwrap())).unwrap();
    let roots: Vec<(String, usize)> =
        serde_json::from_str(&std::fs::read_to_string(&roots_path).unwrap()).unwrap();
    eprintln!(
        "loaded {} solvables, {} roots in {:.1}s",
        snapshot.solvables.len(),
        roots.len(),
        load_start.elapsed().as_secs_f64()
    );

    let mut results = std::io::BufWriter::new(File::create(&results_out).unwrap());
    let mut timings = std::io::BufWriter::new(File::create(&timings_out).unwrap());
    writeln!(timings, "root\tstatus\tms").unwrap();

    let mut times: Vec<f64> = Vec::new();
    let total_start = Instant::now();
    for (i, (name, vsid)) in roots.iter().enumerate() {
        let requirement: ConditionalRequirement = VersionSetId::from_index(*vsid).into();
        let timeout = SystemTime::now() + Duration::from_secs(timeout_secs);
        let (elapsed, repr) = solve_once(&snapshot, vec![requirement], Some(timeout));
        let status = match repr.first().map(String::as_str) {
            Some("UNSOLVABLE") => "unsolvable",
            Some("CANCELLED") => "cancelled",
            _ => "ok",
        };
        times.push(elapsed);
        writeln!(timings, "{name}\t{status}\t{elapsed:.3}").unwrap();
        writeln!(results, "=== {name} ===\n{}", repr.join("\n")).unwrap();
        if (i + 1) % 100 == 0 {
            eprintln!(
                "{}/{} solves done ({:.1}s elapsed)",
                i + 1,
                roots.len(),
                total_start.elapsed().as_secs_f64()
            );
        }
    }

    times.sort_by(f64::total_cmp);
    let total: f64 = times.iter().sum();
    println!(
        "corpus: {} solves | total {:.1} ms | median {:.3} ms | p95 {:.3} ms | max {:.1} ms",
        times.len(),
        total,
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() - 1],
    );
}

fn solve_once(
    snapshot: &DependencySnapshot,
    requirements: Vec<ConditionalRequirement>,
    timeout: Option<SystemTime>,
) -> (f64, Vec<String>) {
    let mut provider = snapshot.provider();
    if let Some(timeout) = timeout {
        provider = provider.with_timeout(timeout);
    }
    let problem = Problem::new().requirements(requirements);
    let mut solver = Solver::new(provider);
    let t = Instant::now();
    let res = solver.solve(problem);
    let elapsed = t.elapsed().as_secs_f64() * 1e3;

    let repr = match &res {
        Ok(ids) => {
            let mut v: Vec<String> = ids
                .iter()
                .map(|&id| solver.provider().display_solvable(id).to_string())
                .collect();
            v.sort();
            v
        }
        Err(resolvo::UnsolvableOrCancelled::Unsolvable(_)) => vec!["UNSOLVABLE".to_string()],
        Err(resolvo::UnsolvableOrCancelled::Cancelled(_)) => vec!["CANCELLED".to_string()],
    };
    (elapsed, repr)
}
