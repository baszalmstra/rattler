//! Integration tests for the universal solve entry point of the resolvo
//! backend (`rattler_solve::resolvo::solve_universal`).

use std::fmt::Write as _;

use rattler_conda_types::{
    Channel, ChannelConfig, MatchSpec, NamelessMatchSpec, PackageName, ParseStrictness, RepoData,
    RepoDataRecord,
};
use rattler_solve::{
    ChannelPriority, SolveStrategy,
    resolvo::{
        CondaUniversalSolution, EnvironmentLiteral, EnvironmentLiteralKind, SymbolicVirtualPackage,
        UniversalSolverTask, solve_universal,
    },
};

fn channel_config() -> ChannelConfig {
    ChannelConfig::default_with_root_dir(std::env::current_dir().unwrap())
}

fn dummy_channel_json_path() -> String {
    format!(
        "{}/{}",
        env!("CARGO_MANIFEST_DIR"),
        "../../test-data/channels/dummy/linux-64/repodata.json"
    )
}

fn read_repodata(path: &str) -> Vec<RepoDataRecord> {
    let repo_data: RepoData =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    repo_data.into_repo_data_records(&Channel::from_str("conda-forge", &channel_config()).unwrap())
}

/// Builds the signed environment literal for the test DSL: `"__cuda absent"`
/// is the absent literal, anything else is `"<package> <spec>"`. A `"not "`
/// prefix negates the literal.
fn literal(s: &str) -> (EnvironmentLiteral, bool) {
    let (s, positive) = match s.strip_prefix("not ") {
        Some(rest) => (rest, false),
        None => (s, true),
    };
    let literal = if let Some(package) = s.strip_suffix(" absent") {
        EnvironmentLiteral {
            package: PackageName::new_unchecked(package),
            kind: EnvironmentLiteralKind::Absent,
        }
    } else {
        let (package, spec) = s.split_once(' ').expect("literal is `<package> <spec>`");
        EnvironmentLiteral {
            package: PackageName::new_unchecked(package),
            kind: EnvironmentLiteralKind::Matches(
                NamelessMatchSpec::from_str(spec, ParseStrictness::Lenient).unwrap(),
            ),
        }
    };
    (literal, positive)
}

/// Parses a CNF model in the literal DSL.
fn model(clauses: &[&[&str]]) -> Vec<Vec<(EnvironmentLiteral, bool)>> {
    clauses
        .iter()
        .map(|clause| clause.iter().map(|s| literal(s)).collect())
        .collect()
}

/// Builds a task over the given records with the v1 symbolic set.
fn task<'a>(
    records: &'a [RepoDataRecord],
    specs: &[&str],
    environment_model: Vec<Vec<(EnvironmentLiteral, bool)>>,
) -> UniversalSolverTask<Vec<&'a [RepoDataRecord]>> {
    UniversalSolverTask {
        available_packages: vec![records],
        virtual_packages: Vec::new(),
        specs: specs
            .iter()
            .map(|s| MatchSpec::from_str(s, ParseStrictness::Lenient).unwrap())
            .collect(),
        constraints: Vec::new(),
        timeout: None,
        cancellation_token: None,
        channel_priority: ChannelPriority::default(),
        exclude_newer: None,
        strategy: SolveStrategy::default(),
        symbolic_virtual_packages: SymbolicVirtualPackage::default_v1_set(),
        environment_model,
        seed_partition: Vec::new(),
    }
}

/// Renders the cell partition for inline snapshots: one `cell:` line with
/// the condition per cell, followed by the records as `name=version=build`.
fn render_cells(solution: &CondaUniversalSolution) -> String {
    let mut buf = String::new();
    for (condition, records) in &solution.cells {
        writeln!(
            buf,
            "cell: {}",
            rattler_solve::resolvo::display_condition(condition)
        )
        .unwrap();
        let mut lines: Vec<String> = records
            .iter()
            .map(|r| {
                format!(
                    "  {}={}={}",
                    r.package_record.name.as_normalized(),
                    r.package_record.version.as_str(),
                    r.package_record.build,
                )
            })
            .collect();
        lines.sort();
        for line in lines {
            writeln!(buf, "{line}").unwrap();
        }
    }
    buf
}

/// Scenario (a): the dummy channel's `cuda-version` package constrains
/// `__cuda >=12.1`. With `__cuda` symbolic and the model "cuda absent OR
/// cuda >=12.1", the universal solve must produce exactly two cells with
/// the same record: the absent baseline first (split policy: the absent
/// branch is explored first), then the cuda region.
#[test]
fn test_universal_cuda_split() {
    let records = read_repodata(&dummy_channel_json_path());
    let solution = solve_universal(task(
        &records,
        &["cuda-version"],
        model(&[&["__cuda absent", "__cuda >=12.1"]]),
    ))
    .unwrap();

    insta::assert_snapshot!(render_cells(&solution), @r"
    cell: __cuda absent
      cuda-version=12.5=hd4f0392_3
    cell: __cuda >=12.1
      cuda-version=12.5=hd4f0392_3
    ");
}
