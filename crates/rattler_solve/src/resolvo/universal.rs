//! The universal solve entry point of the resolvo backend: a single solve
//! whose output is valid for a whole family of environments (machines with
//! or without CUDA, any glibc above a floor, any microarchitecture, ...)
//! instead of one concrete machine.
//!
//! Virtual packages listed in
//! [`UniversalSolverTask::symbolic_virtual_packages`] are treated
//! symbolically: version sets used against them become environment literals
//! and the result is a partition of the environment space described by
//! [`UniversalSolverTask::environment_model`] into *cells*, each pairing a
//! region (a conjunction of signed [`EnvironmentLiteral`]s) with the records
//! valid throughout that region. Installation for a concrete machine is a
//! plain evaluation of the literals ([`CondaUniversalSolution::project`]),
//! no solving required.
//!
//! The result is converted eagerly into owned conda data types so that it
//! outlives the solver and can later be serialized into a lockfile.

use std::fmt::{Display, Formatter};

use rattler_conda_types::{
    GenericVirtualPackage, MatchSpec, NamelessMatchSpec, PackageName, PackageNameMatcher,
    RepoDataRecord, StringMatcher,
};
use resolvo::{
    ConditionalRequirement, EnvLiteral, EnvLiteralKind, NameId, SolvableId,
    Solver as ResolvoSolver, UniversalFailure, UniversalProblem, UniversalSolution, VersionSetId,
};

use super::{
    CondaDependencyProvider, DependencyOverride, NameType, RepoData, SolverMatchSpec,
    SolverPackageRecord, SymbolicVirtualPackage,
};
use crate::{
    CancellationToken, ChannelPriority, ExcludeNewer, IntoRepoData, SolveError, SolveStrategy,
};

/// A signed-literal building block of environment models, cell conditions
/// and seed partitions, expressed in conda terms.
///
/// An environment literal describes a property of a machine through one of
/// its virtual packages: either "the value of `package` exists and matches
/// the given spec" or "`package` is absent".
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentLiteral {
    /// The symbolic virtual package this literal refers to (e.g. `__cuda`).
    pub package: PackageName,

    /// Whether this literal is a match on the package's value or the absent
    /// sentinel.
    pub kind: EnvironmentLiteralKind,
}

/// The kind of an [`EnvironmentLiteral`].
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentLiteralKind {
    /// The environment's value for the package exists and matches the
    /// version/build parts of the given spec.
    Matches(NamelessMatchSpec),

    /// The package is absent from the environment. Only valid for packages
    /// declared with `can_be_absent: true`.
    Absent,
}

impl Display for EnvironmentLiteral {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            EnvironmentLiteralKind::Matches(spec) => {
                write!(f, "{} {spec}", self.package.as_normalized())
            }
            EnvironmentLiteralKind::Absent => {
                write!(f, "{} absent", self.package.as_normalized())
            }
        }
    }
}

impl EnvironmentLiteral {
    /// Evaluates this literal against a concrete machine described by its
    /// detected virtual packages. This is the runtime "walker" primitive:
    /// no solving is involved.
    ///
    /// `Absent` literals are true when no virtual package with the literal's
    /// name is present. `Matches` literals are true when the package is
    /// present and its version matches the spec's version part and its build
    /// string matches the spec's build part.
    ///
    /// For `__archspec`, an exact build matcher is evaluated with the same
    /// archspec DAG semantics the relation oracle uses: the machine's
    /// microarchitecture satisfies the literal when its lineage includes the
    /// matcher's name (so an `x86_64_v4` machine satisfies an `x86_64_v3`
    /// literal). This deliberately differs from the exact-string matching
    /// used for concrete `__archspec` candidate records in
    /// `filter_candidates`; the two semantics must not be mixed, see the
    /// relation oracle docs.
    pub fn evaluate(&self, machine: &[GenericVirtualPackage]) -> bool {
        let value = machine.iter().find(|vp| vp.name == self.package);
        match (&self.kind, value) {
            (EnvironmentLiteralKind::Absent, value) => value.is_none(),
            (EnvironmentLiteralKind::Matches(_), None) => false,
            (EnvironmentLiteralKind::Matches(spec), Some(value)) => {
                if let Some(version_spec) = &spec.version
                    && !version_spec.matches(&value.version)
                {
                    return false;
                }
                if let Some(build_matcher) = &spec.build {
                    return match build_matcher {
                        StringMatcher::Exact(name)
                            if self.package.as_normalized() == "__archspec" =>
                        {
                            archspec_lineage_includes(&value.build_string, name)
                        }
                        matcher => matcher.matches(&value.build_string),
                    };
                }
                true
            }
        }
    }
}

/// Whether the machine microarchitecture `value` satisfies the literal name
/// `wanted` under the archspec DAG semantics: its lineage includes `wanted`.
/// Names unknown to the DAG fall back to exact (case-insensitive) equality,
/// which is the only lineage information available for them.
fn archspec_lineage_includes(value: &str, wanted: &str) -> bool {
    if value.eq_ignore_ascii_case(wanted) {
        return true;
    }
    let targets = archspec::cpu::Microarchitecture::known_targets();
    match (targets.get(value), targets.get(wanted)) {
        (Some(value), Some(wanted)) => value.decendent_of(wanted),
        _ => false,
    }
}

/// A conjunction of signed environment literals: a `(literal, true)` entry
/// requires the literal to hold, a `(literal, false)` entry requires it not
/// to. The empty conjunction means "all environments".
pub type EnvironmentCondition = Vec<(EnvironmentLiteral, bool)>;

/// Renders an [`EnvironmentCondition`] in a human readable way, e.g.
/// `__cuda >=12.1 AND not (__glibc <2.17)`. The empty conjunction renders as
/// `<all environments>`.
pub fn display_condition(condition: &EnvironmentCondition) -> String {
    use std::fmt::Write;
    if condition.is_empty() {
        return "<all environments>".to_string();
    }
    let mut buf = String::new();
    for (index, (literal, positive)) in condition.iter().enumerate() {
        if index > 0 {
            buf.push_str(" AND ");
        }
        if *positive {
            write!(buf, "{literal}").unwrap();
        } else {
            write!(buf, "not ({literal})").unwrap();
        }
    }
    buf
}

/// Describes a universal resolution task: the regular solver task inputs
/// plus the symbolic virtual packages, the environment model bounding the
/// environment space, and an optional seed partition from a previous solve.
pub struct UniversalSolverTask<TAvailablePackagesIterator> {
    /// An iterator over all available packages.
    pub available_packages: TAvailablePackagesIterator,

    /// Virtual packages that stay *concrete* (injected as records describing
    /// every modeled machine, e.g. `__unix`). Must be disjoint from the
    /// symbolic set.
    pub virtual_packages: Vec<GenericVirtualPackage>,

    /// The specs to solve for.
    pub specs: Vec<MatchSpec>,

    /// Additional constraints that a chosen solvable must adhere to without
    /// requiring installation.
    pub constraints: Vec<MatchSpec>,

    /// The timeout after which the solver should stop.
    pub timeout: Option<std::time::Duration>,

    /// An optional token that can be used to cancel the solve.
    pub cancellation_token: Option<CancellationToken>,

    /// The channel priority to solve with.
    pub channel_priority: ChannelPriority,

    /// Exclude packages newer than the configured cutoff.
    pub exclude_newer: Option<ExcludeNewer>,

    /// The solve strategy.
    pub strategy: SolveStrategy,

    /// The virtual packages treated symbolically; see
    /// [`SymbolicVirtualPackage`].
    pub symbolic_virtual_packages: Vec<SymbolicVirtualPackage>,

    /// The environment model: a CNF over signed environment literals (each
    /// inner `Vec` is a disjunction) bounding the environment space the
    /// solution must cover. The model is total: every region inside it must
    /// be solvable or the whole solve fails. An empty CNF means "all
    /// environments".
    pub environment_model: Vec<EnvironmentCondition>,

    /// Cell conditions from a previous solve, solved first (in order) under
    /// assumptions, which keeps stable regions of the partition identical
    /// across re-solves. Invalid or unsolvable seeds are dropped, not fatal.
    pub seed_partition: Vec<EnvironmentCondition>,
}

/// A dependency edge of a [`CondaUniversalSolution`], aggregated over the
/// cells in which it is active.
#[derive(Clone, Debug)]
pub struct CondaCellEdge {
    /// The record whose requirement this edge satisfies, or `None` when the
    /// requirement comes from the root task.
    pub parent: Option<RepoDataRecord>,

    /// A human readable rendering of the requirement (`name spec`).
    pub requirement: String,

    /// The record chosen to satisfy the requirement, or `None` when the
    /// requirement is on a symbolic virtual package (the environment itself
    /// satisfies it; there is nothing to install).
    pub target: Option<RepoDataRecord>,

    /// The disjunction of the conditions of the cells in which this edge is
    /// active (simplified within the model bounds).
    pub presence: Vec<EnvironmentCondition>,
}

/// The result of a successful [`solve_universal`] call, converted eagerly
/// into owned conda data types (it does not borrow solver state and can be
/// serialized later).
#[derive(Clone, Debug)]
pub struct CondaUniversalSolution {
    /// The enumerated cells: pairwise disjoint regions of the environment
    /// space, in deterministic enumeration order (baseline first), together
    /// covering the environment model. Each cell pairs its condition with
    /// the records valid throughout the region.
    pub cells: Vec<(EnvironmentCondition, Vec<RepoDataRecord>)>,

    /// The merged presence-condition view: one entry per distinct record,
    /// paired with the OR of the conditions of the cells containing it
    /// (simplified within the model bounds; an entry containing the empty
    /// conjunction is present in all environments).
    pub merged: Vec<(RepoDataRecord, Vec<EnvironmentCondition>)>,

    /// The aggregated conditional dependency edges: what a lockfile
    /// serializer stores to enable installation by graph walk.
    pub edges: Vec<CondaCellEdge>,

    /// The outcome of resolvo's independent post-hoc verification (pairwise
    /// cell disjointness and model coverage), captured at solve time against
    /// the live relation oracle. Violations are rendered human readable.
    verification: Result<(), Vec<String>>,
}

impl CondaUniversalSolution {
    /// Returns the outcome of the independent verification of the solution
    /// invariants (pairwise disjointness of cells with different record
    /// sets, and model coverage), computed at solve time.
    pub fn verify(&self) -> Result<(), &[String]> {
        match &self.verification {
            Ok(()) => Ok(()),
            Err(violations) => Err(violations),
        }
    }

    /// Returns the records of the unique cell whose condition holds on the
    /// concrete machine described by its detected virtual packages. This is
    /// the runtime "walker" entry point: evaluating environment literals
    /// replaces solving at install time.
    ///
    /// Returns `None` when no cell matches, which only happens for machines
    /// outside the environment model.
    pub fn project(&self, machine: &[GenericVirtualPackage]) -> Option<&[RepoDataRecord]> {
        let mut found: Option<&[RepoDataRecord]> = None;
        for (condition, records) in &self.cells {
            let matches = condition
                .iter()
                .all(|(literal, sign)| literal.evaluate(machine) == *sign);
            if !matches {
                continue;
            }
            debug_assert!(
                found.is_none(),
                "broken invariant: multiple cells match the same machine"
            );
            if found.is_none() {
                found = Some(records);
            }
        }
        found
    }
}

/// The errors of an unsuccessful [`solve_universal`] call.
#[derive(Debug, thiserror::Error)]
pub enum UniversalSolveError {
    /// Some region of the environment model has no solution. The model is
    /// total, so the whole universal solve fails.
    #[error("cannot solve for environments where {condition}: {conflict}")]
    Unsolvable {
        /// The witness region, as a conda-typed condition.
        condition_literals: EnvironmentCondition,
        /// The witness region, rendered human readable.
        condition: String,
        /// resolvo's user-friendly conflict rendering, scoped to the witness
        /// region.
        conflict: String,
    },

    /// The solve was cancelled (through the cancellation token or timeout).
    #[error("the universal solve was cancelled")]
    Cancelled,

    /// Constructing the solver input failed.
    #[error(transparent)]
    Setup(#[from] SolveError),
}

/// Solves the given [`UniversalSolverTask`], producing a partition of the
/// environment model into cells with the records valid throughout each cell.
///
/// This is the universal counterpart of
/// [`crate::SolverImpl::solve`][crate::SolverImpl] for the resolvo backend.
pub fn solve_universal<'a, R, TAvailablePackagesIterator>(
    task: UniversalSolverTask<TAvailablePackagesIterator>,
) -> Result<CondaUniversalSolution, UniversalSolveError>
where
    R: IntoRepoData<'a, RepoData<'a>>,
    TAvailablePackagesIterator: IntoIterator<Item = R>,
{
    let UniversalSolverTask {
        available_packages,
        virtual_packages,
        specs,
        constraints,
        timeout,
        cancellation_token,
        channel_priority,
        exclude_newer,
        strategy,
        symbolic_virtual_packages,
        environment_model,
        seed_partition,
    } = task;

    let stop_time = timeout.map(|timeout| std::time::SystemTime::now() + timeout);

    #[allow(clippy::redundant_closure_for_method_calls)]
    let provider = CondaDependencyProvider::new(
        available_packages.into_iter().map(|r| r.into()),
        &[],
        &[],
        &virtual_packages,
        specs.as_ref(),
        stop_time,
        cancellation_token,
        channel_priority,
        exclude_newer.as_ref(),
        strategy,
        Vec::<DependencyOverride>::new(),
        symbolic_virtual_packages,
    )?;

    // Like the concrete solve, the *concrete* virtual packages are added as
    // root requirements so their records appear in the solution. Symbolic
    // virtual packages must NOT be required: they are environment packages,
    // not installable records.
    let virtual_package_requirements = virtual_packages.iter().map(|vp| {
        let name_id = provider.pool.intern_package_name(NameType::from(&vp.name));
        provider
            .pool
            .intern_version_set(name_id, NamelessMatchSpec::default().into())
    });

    let root_requirements: Vec<ConditionalRequirement> = virtual_package_requirements
        .map(ConditionalRequirement::from)
        .chain(specs.iter().flat_map(|spec| {
            super::version_sets_for_match_spec(&provider.pool, spec.clone())
                .into_iter()
                .map(ConditionalRequirement::from)
        }))
        .collect();

    let root_constraints = constraints
        .iter()
        .map(|spec| {
            let (PackageNameMatcher::Exact(name), spec) = spec.clone().into_nameless() else {
                unimplemented!("only exact package names are supported");
            };
            let name_id = provider.pool.intern_package_name(NameType::from(&name));
            provider.pool.intern_version_set(name_id, spec.into())
        })
        .collect();

    let environment_model = environment_model
        .iter()
        .map(|disjunction| {
            disjunction
                .iter()
                .map(|(literal, positive)| (intern_literal(&provider, literal), *positive))
                .collect()
        })
        .collect();
    let seed_partition = seed_partition
        .iter()
        .map(|condition| {
            resolvo::CellCondition(
                condition
                    .iter()
                    .map(|(literal, positive)| (intern_literal(&provider, literal), *positive))
                    .collect(),
            )
        })
        .collect();

    let problem = UniversalProblem::new()
        .requirements(root_requirements)
        .constraints(root_constraints)
        .environment_model(environment_model)
        .seed_partition(seed_partition);

    let mut solver = ResolvoSolver::new(provider);
    let solution = match solver.solve_universal(problem) {
        Ok(solution) => solution,
        Err(UniversalFailure::Unsolvable { cell, conflict }) => {
            return Err(UniversalSolveError::Unsolvable {
                condition_literals: convert_condition(solver.provider(), &cell),
                condition: cell.display(solver.provider()).to_string(),
                conflict: conflict.display_user_friendly(&solver).to_string(),
            });
        }
        Err(UniversalFailure::Cancelled(_)) => {
            return Err(UniversalSolveError::Cancelled);
        }
    };

    // Verify while the relation oracle is still available, then convert
    // everything eagerly into owned conda types.
    let provider = solver.provider();
    let verification = solution.verify(provider).map_err(|violations| {
        violations
            .into_iter()
            .map(|violation| render_violation(provider, &solution, violation))
            .collect()
    });

    let cells = solution
        .cells
        .iter()
        .map(|(condition, solvables)| {
            (
                convert_condition(provider, condition),
                solvables
                    .iter()
                    .filter_map(|&solvable| record_for_solvable(provider, solvable))
                    .collect(),
            )
        })
        .collect();

    let merged = solution
        .merged()
        .into_iter()
        .filter_map(|(solvable, presence)| {
            Some((
                record_for_solvable(provider, solvable)?,
                convert_presence(provider, &presence),
            ))
        })
        .collect();

    // Dependency edges. Edges whose parent or target is an extra or a
    // concrete virtual package record are skipped: they carry no payload a
    // conda lockfile stores (extras re-derive from the parent record).
    let edges = solution
        .edges()
        .into_iter()
        .filter_map(|(edge, presence)| {
            let parent = match edge.parent {
                None => None,
                Some(solvable) => Some(record_for_solvable(provider, solvable)?),
            };
            let target = match edge.target {
                None => None,
                Some(solvable) => Some(record_for_solvable(provider, solvable)?),
            };
            Some(CondaCellEdge {
                parent,
                requirement: edge.requirement.display(provider).to_string(),
                target,
                presence: convert_presence(provider, &presence),
            })
        })
        .collect();

    Ok(CondaUniversalSolution {
        cells,
        merged,
        edges,
        verification,
    })
}

/// Interns a conda environment literal into the provider's pool, returning
/// the resolvo representation.
fn intern_literal(
    provider: &CondaDependencyProvider<'_>,
    literal: &EnvironmentLiteral,
) -> EnvLiteral<NameId> {
    let name_id = provider
        .pool
        .intern_package_name(NameType::from(&literal.package));
    let kind = match &literal.kind {
        EnvironmentLiteralKind::Matches(spec) => EnvLiteralKind::Matches(
            provider
                .pool
                .intern_version_set(name_id, spec.clone().into()),
        ),
        EnvironmentLiteralKind::Absent => EnvLiteralKind::Absent,
    };
    EnvLiteral {
        package: name_id,
        kind,
    }
}

/// Converts a resolvo cell condition back into conda terms.
fn convert_condition(
    provider: &CondaDependencyProvider<'_>,
    condition: &resolvo::CellCondition<NameId>,
) -> EnvironmentCondition {
    condition
        .0
        .iter()
        .map(|(literal, positive)| (convert_literal(provider, literal), *positive))
        .collect()
}

/// Converts a resolvo presence (a disjunction of cell conditions) back into
/// conda terms.
fn convert_presence(
    provider: &CondaDependencyProvider<'_>,
    presence: &resolvo::Presence<NameId>,
) -> Vec<EnvironmentCondition> {
    presence
        .0
        .iter()
        .map(|condition| convert_condition(provider, condition))
        .collect()
}

/// Converts a resolvo environment literal back into conda terms.
fn convert_literal(
    provider: &CondaDependencyProvider<'_>,
    literal: &EnvLiteral<NameId>,
) -> EnvironmentLiteral {
    let package = package_name(provider, literal.package);
    let kind = match literal.kind {
        EnvLiteralKind::Matches(version_set) => {
            EnvironmentLiteralKind::Matches(nameless_spec(provider, version_set))
        }
        EnvLiteralKind::Absent => EnvironmentLiteralKind::Absent,
    };
    EnvironmentLiteral { package, kind }
}

/// Resolves an interned name back to a conda package name. Environment
/// literals only ever reference base names.
fn package_name(provider: &CondaDependencyProvider<'_>, name: NameId) -> PackageName {
    match provider.pool.resolve_package_name(name) {
        NameType::Base(name) => PackageName::new_unchecked(name.clone()),
        NameType::Extra { .. } => {
            unreachable!("environment literals never reference extra names")
        }
    }
}

/// Resolves an interned version set back to its match spec. Environment
/// literals only ever reference match spec version sets.
fn nameless_spec(provider: &CondaDependencyProvider<'_>, id: VersionSetId) -> NamelessMatchSpec {
    match provider.pool.resolve_version_set(id) {
        SolverMatchSpec::MatchSpec(spec) => spec.clone(),
        _ => unreachable!("environment literals never reference extra version sets"),
    }
}

/// Returns the repodata record of a solvable, or `None` for extras and
/// concrete virtual package records (which have no record to install).
fn record_for_solvable(
    provider: &CondaDependencyProvider<'_>,
    solvable: SolvableId,
) -> Option<RepoDataRecord> {
    match &provider.pool.resolve_solvable(solvable).record {
        SolverPackageRecord::Record(record) => Some((*record).clone()),
        SolverPackageRecord::VirtualPackage(_) | SolverPackageRecord::Extra { .. } => None,
    }
}

/// Renders a verification violation human readable.
fn render_violation(
    provider: &CondaDependencyProvider<'_>,
    solution: &UniversalSolution<SolvableId, NameId>,
    violation: resolvo::Violation<NameId>,
) -> String {
    match violation {
        resolvo::Violation::OverlappingCells { first, second } => format!(
            "cells {first} ({}) and {second} ({}) overlap but have different records",
            solution.cells[first].0.display(provider),
            solution.cells[second].0.display(provider),
        ),
        resolvo::Violation::UnprovenDisjointness { first, second } => format!(
            "cells {first} ({}) and {second} ({}) have different records and their \
             disjointness could not be proven",
            solution.cells[first].0.display(provider),
            solution.cells[second].0.display(provider),
        ),
        resolvo::Violation::UncoveredRegion(condition) => format!(
            "the environment region {} is not covered by any cell",
            condition.display(provider),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rattler_conda_types::{ParseStrictness, Version};

    use super::*;

    fn virtual_package(name: &str, version: &str, build: &str) -> GenericVirtualPackage {
        GenericVirtualPackage {
            name: PackageName::new_unchecked(name),
            version: Version::from_str(version).unwrap(),
            build_string: build.to_string(),
        }
    }

    fn matches_literal(package: &str, spec: &str) -> EnvironmentLiteral {
        EnvironmentLiteral {
            package: PackageName::new_unchecked(package),
            kind: EnvironmentLiteralKind::Matches(
                NamelessMatchSpec::from_str(spec, ParseStrictness::Lenient).unwrap(),
            ),
        }
    }

    /// Literal evaluation against a concrete machine: version matches,
    /// absent semantics and the missing-package case.
    #[test]
    fn test_evaluate_version_and_absent_literals() {
        let machine = [virtual_package("__cuda", "12.4", "0")];
        assert!(matches_literal("__cuda", ">=12.1").evaluate(&machine));
        assert!(!matches_literal("__cuda", ">=12.5").evaluate(&machine));
        let absent = EnvironmentLiteral {
            package: PackageName::new_unchecked("__cuda"),
            kind: EnvironmentLiteralKind::Absent,
        };
        assert!(!absent.evaluate(&machine));
        assert!(absent.evaluate(&[]));
        assert!(!matches_literal("__cuda", ">=12.1").evaluate(&[]));
    }

    /// `__archspec` literals evaluate with the archspec DAG semantics: a
    /// machine microarchitecture satisfies a literal when its lineage
    /// includes the literal's name, NOT only on exact equality (which is
    /// what concrete candidate filtering uses).
    #[test]
    fn test_evaluate_archspec_literal_uses_dag_lineage() {
        let v4_machine = [virtual_package("__archspec", "1", "x86_64_v4")];
        assert!(matches_literal("__archspec", "* x86_64_v3").evaluate(&v4_machine));
        assert!(matches_literal("__archspec", "* x86_64").evaluate(&v4_machine));
        assert!(matches_literal("__archspec", "* x86_64_v4").evaluate(&v4_machine));
        // The lineage does not go downward.
        let v2_machine = [virtual_package("__archspec", "1", "x86_64_v2")];
        assert!(!matches_literal("__archspec", "* x86_64_v3").evaluate(&v2_machine));
        // Different families never satisfy each other.
        assert!(!matches_literal("__archspec", "* aarch64").evaluate(&v4_machine));
        // Unknown names fall back to exact equality.
        let unknown_machine = [virtual_package("__archspec", "1", "mysterychip")];
        assert!(matches_literal("__archspec", "* mysterychip").evaluate(&unknown_machine));
        assert!(!matches_literal("__archspec", "* x86_64").evaluate(&unknown_machine));
    }
}
