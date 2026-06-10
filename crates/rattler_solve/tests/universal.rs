//! Integration tests for the universal solve entry point of the resolvo
//! backend (`rattler_solve::resolvo::solve_universal`).

use std::{fmt::Write as _, str::FromStr};

use rattler_conda_types::{
    Channel, ChannelConfig, MatchSpec, NamelessMatchSpec, PackageName, ParseStrictness, RepoData,
    RepoDataRecord,
};
use rattler_conda_types::{GenericVirtualPackage, Version};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};
use rattler_solve::{
    ChannelPriority, SolveStrategy,
    resolvo::{
        CondaUniversalSolution, EnvironmentLiteral, EnvironmentLiteralKind, SymbolicVirtualPackage,
        UniversalSolveError, UniversalSolverTask, solve_universal,
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

/// Loads the recursive closure of `bubblewrap` from the local conda-forge
/// fixture (`test-data/channels/conda-forge/linux-64/repodata.json`).
/// `bubblewrap` is documented in the test below.
fn read_bubblewrap_closure() -> Vec<RepoDataRecord> {
    let path = format!(
        "{}/{}",
        env!("CARGO_MANIFEST_DIR"),
        "../../test-data/channels/conda-forge/linux-64/repodata.json"
    );
    let sparse = SparseRepoData::from_file(
        Channel::from_str("conda-forge", &channel_config()).unwrap(),
        "conda-forge".to_string(),
        path,
        None,
    )
    .unwrap();
    SparseRepoData::load_records_recursive(
        [&sparse],
        [PackageName::new_unchecked("bubblewrap")],
        None,
        PackageFormatSelection::default(),
    )
    .unwrap()
    .into_iter()
    .next()
    .unwrap()
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

/// Scenario (b): widening the model to "cuda absent OR cuda >=11" makes the
/// region "cuda present in [11, 12.1)" part of the modeled space. The
/// package constrains `__cuda >=12.1`, so that region is unsolvable, and
/// the model is total: the whole universal solve must fail with exactly
/// that witness region.
#[test]
fn test_universal_cuda_unsolvable_region() {
    let records = read_repodata(&dummy_channel_json_path());
    let error = solve_universal(task(
        &records,
        &["cuda-version"],
        model(&[&["__cuda absent", "__cuda >=11"]]),
    ))
    .unwrap_err();

    let UniversalSolveError::Unsolvable {
        condition,
        conflict,
        ..
    } = &error
    else {
        panic!("expected an unsolvable region, got {error}");
    };
    insta::assert_snapshot!(
        condition,
        @"not (__cuda absent) AND __cuda >=11 AND not (__cuda >=12.1)"
    );
    // The rendered error mentions the witness region, and the scoped
    // conflict explains the package that cannot be satisfied there.
    let message = error.to_string();
    assert!(
        message.contains("not (__cuda absent) AND __cuda >=11 AND not (__cuda >=12.1)"),
        "message should mention the witness region: {message}"
    );
    assert!(
        conflict.contains("cuda-version"),
        "conflict should mention cuda-version: {conflict}"
    );
}

/// Scenario (c): a real glibc floor from the conda-forge fixture.
/// `bubblewrap 0.6.2 h166bdaf_0` depends on `__glibc >=2.17,<3.0.a0` (plus
/// libcap and libgcc-ng). With `__glibc` symbolic and the model pinned to
/// the same range, the whole closure is valid in a single cell whose
/// condition is the glibc literal forced by the dependency.
#[test]
fn test_universal_glibc_floor() {
    let records = read_bubblewrap_closure();
    let solution = solve_universal(task(
        &records,
        &["bubblewrap"],
        model(&[&["__glibc >=2.17,<3.0.a0"]]),
    ))
    .unwrap();

    assert_eq!(solution.cells.len(), 1, "expected a single cell");
    let (condition, cell_records) = &solution.cells[0];
    insta::assert_snapshot!(
        rattler_solve::resolvo::display_condition(condition),
        @"__glibc >=2.17,<3.0.a0"
    );
    assert!(
        cell_records.iter().any(|r| {
            r.package_record.name.as_normalized() == "bubblewrap"
                && r.package_record.version.as_str() == "0.6.2"
        }),
        "expected bubblewrap 0.6.2 in the cell: {:?}",
        cell_records
            .iter()
            .map(|r| r.identifier.to_string())
            .collect::<Vec<_>>(),
    );
    assert!(solution.verify().is_ok());
}

/// Scenario (c2): the same closure with the *wider* model "__glibc >=2.17"
/// fails: the pairwise relation oracle cannot express that
/// `>=2.17 AND <3.0a0` follows from the two model/dependency literals
/// together (that is a ternary entailment), so the vacuous region
/// ">=2.17 but not >=2.17,<3.0.a0" (i.e. glibc >= 3.0a0) stays inside the
/// model and is unsolvable. This documents the "vacuous cells" risk from
/// the design document on real conda data: the model literal should match
/// the bounds the ecosystem actually uses.
#[test]
fn test_universal_glibc_vacuous_region_fails() {
    let records = read_bubblewrap_closure();
    let error = solve_universal(task(
        &records,
        &["bubblewrap"],
        model(&[&["__glibc >=2.17"]]),
    ))
    .unwrap_err();

    let UniversalSolveError::Unsolvable { condition, .. } = &error else {
        panic!("expected an unsolvable region, got {error}");
    };
    insta::assert_snapshot!(
        condition,
        @"__glibc >=2.17 AND not (__glibc >=2.17,<3.0.a0)"
    );
}

/// Scenario (d): seeding a re-solve with the previous partition reproduces
/// the identical partition (uv-style stability).
#[test]
fn test_universal_seed_round_trip() {
    let records = read_repodata(&dummy_channel_json_path());
    let environment_model = model(&[&["__cuda absent", "__cuda >=12.1"]]);
    let first =
        solve_universal(task(&records, &["cuda-version"], environment_model.clone())).unwrap();

    let mut seeded_task = task(&records, &["cuda-version"], environment_model);
    seeded_task.seed_partition = first
        .cells
        .iter()
        .map(|(condition, _)| condition.clone())
        .collect();
    let second = solve_universal(seeded_task).unwrap();

    assert_eq!(render_cells(&first), render_cells(&second));
}

/// Scenario (e): the independent verification (pairwise disjointness and
/// model coverage) passes for every successful result.
#[test]
fn test_universal_verify_ok() {
    let records = read_repodata(&dummy_channel_json_path());
    let solution = solve_universal(task(
        &records,
        &["cuda-version"],
        model(&[&["__cuda absent", "__cuda >=12.1"]]),
    ))
    .unwrap();
    assert!(solution.verify().is_ok());
}

/// Scenario (f): projecting the cuda-split solution onto concrete machines.
/// A machine with cuda >= 12.1 selects the cuda cell, a machine without
/// cuda selects the baseline cell, and a machine with an older cuda is
/// outside the environment model (no cell).
#[test]
fn test_universal_projection() {
    let records = read_repodata(&dummy_channel_json_path());
    let solution = solve_universal(task(
        &records,
        &["cuda-version"],
        model(&[&["__cuda absent", "__cuda >=12.1"]]),
    ))
    .unwrap();

    let cuda_machine = [GenericVirtualPackage {
        name: PackageName::new_unchecked("__cuda"),
        version: Version::from_str("12.4").unwrap(),
        build_string: "0".to_string(),
    }];
    let with_cuda = solution.project(&cuda_machine).expect("cuda cell");
    assert_eq!(with_cuda, solution.cells[1].1.as_slice());

    let without_cuda = solution.project(&[]).expect("baseline cell");
    assert_eq!(without_cuda, solution.cells[0].1.as_slice());

    let old_cuda_machine = [GenericVirtualPackage {
        name: PackageName::new_unchecked("__cuda"),
        version: Version::from_str("11.0").unwrap(),
        build_string: "0".to_string(),
    }];
    assert!(
        solution.project(&old_cuda_machine).is_none(),
        "a cuda 11 machine is outside the model"
    );
}
