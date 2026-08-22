//! Replays captured resolvo `DependencySnapshot`s: times the solve and dumps
//! the selected solvables so runs against different resolvo builds can be
//! compared for both speed and identical results.

use std::{fs::File, io::BufReader, time::Instant};

use resolvo::{
    ConditionalRequirement, DenseIndex, Interner, Problem, Solver, VersionSetId,
    snapshot::DependencySnapshot,
};

const LABELS: [&str; 5] = [
    "python",
    "xtensor-xsimd",
    "tensorflow",
    "quetz",
    "tensorboard-grpc",
];

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .expect("usage: snapshot-replay <corpus-dir> <iters> <results-out>");
    let iters: usize = args.next().expect("missing iters").parse().unwrap();
    let results_out = args.next().expect("missing results-out");

    let mut results = String::new();
    for label in LABELS {
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
            let provider = snapshot.provider();
            let problem = Problem::new().requirements(requirements.clone());
            let mut solver = Solver::new(provider);
            let t = Instant::now();
            let res = solver.solve(problem);
            let elapsed = t.elapsed().as_secs_f64() * 1e3;
            if i > 0 {
                // The first iteration is warmup.
                times.push(elapsed);
            }

            let repr: Vec<String> = match &res {
                Ok(ids) => {
                    let mut v: Vec<String> = ids
                        .iter()
                        .map(|&id| solver.provider().display_solvable(id).to_string())
                        .collect();
                    v.sort();
                    v
                }
                Err(_) => vec!["UNSOLVABLE".to_string()],
            };
            match &solution {
                None => solution = Some(repr),
                Some(previous) => assert_eq!(
                    previous, &repr,
                    "solution changed between iterations for {label}"
                ),
            }
        }

        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        let solution = solution.unwrap();
        println!(
            "{label}: median {median:.3} ms | min {:.3} ms | {} iters | {} packages selected",
            times[0],
            times.len(),
            solution.len()
        );
        results.push_str(&format!("=== {label} ===\n{}\n", solution.join("\n")));
    }
    std::fs::write(results_out, results).unwrap();
}
