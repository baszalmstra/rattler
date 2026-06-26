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

use std::{
    fmt::{Display, Formatter},
    ops::Bound,
};

use rattler_conda_types::{
    GenericVirtualPackage, MatchSpec, NamelessMatchSpec, PackageName, PackageNameMatcher,
    ParseStrictness, RepoDataRecord, StringMatcher, Version, VersionSpec,
    match_spec::matcher::RegexMatcher,
};
use resolvo::{
    CellCondition as ResolvoCellCondition, ConditionalRequirement, EnvClause, EnvLiteral,
    EnvironmentModel, NameId, SignedEnvLiteral, SolvableId, Solver as ResolvoSolver,
    UniversalFailure, UniversalProblem, UniversalSolution, VersionSetId,
};
use version_ranges::Ranges;

use super::{
    CondaDependencyProvider, DependencyOverride, NameType, RepoData, SolverMatchSpec,
    SolverPackageRecord, SymbolicVirtualPackage,
    version_oracle::{
        literal_alternatives, match_spec_relation, version_spec_to_ranges,
        version_spec_to_ranges_envelope,
    },
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
    /// Build matching is plain [`StringMatcher::matches`], the same
    /// semantics `filter_candidates` applies to concrete records. This
    /// includes `__archspec`: per CEP 30 a machine reports exactly one
    /// microarchitecture name and specs match it exactly, so an
    /// `x86_64_v4` machine does NOT satisfy an `x86_64_v3` literal (the
    /// archspec DAG lineage does not count; conda-forge encodes lineage by
    /// shipping one `_x86_64-microarch-level` build per concrete name).
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
                    return build_matcher.matches(&value.build_string);
                }
                true
            }
        }
    }
}

/// A conjunction of signed environment literals: a `(literal, true)` entry
/// requires the literal to hold, a `(literal, false)` entry requires it not
/// to. The empty conjunction means "all environments".
pub type EnvironmentCondition = Vec<(EnvironmentLiteral, bool)>;

/// Renders an [`EnvironmentCondition`] in a human readable way, e.g.
/// `__cuda >=12.1 AND not (__glibc <2.17)`. The empty conjunction renders as
/// `<all environments>`.
///
/// Multiple version-only literals on the same package are combined into a
/// single contiguous range when that is exactly equivalent: for example
/// `__cuda >=12.0 AND not (__cuda >=13)` renders as `__cuda >=12.0,<13`.
/// An explicit `not (__cuda absent)` counts as the presence constraint that
/// makes negative-only version literals renderable, so `not (__cuda >=13)
/// AND not (__cuda absent)` renders as `__cuda <13`. Finite build-string
/// atom sets are also compacted when exact: redundant disjoint negations are
/// dropped, and anchored literal alternations can subtract exact atoms.
/// Whenever a package's literals are not exactly representable as one
/// positive literal (for example a split version range, a non-finite build
/// matcher, or an unconvertible version spec), they fall back to the plain
/// per-literal rendering.
pub fn display_condition(condition: &EnvironmentCondition) -> String {
    if condition.is_empty() {
        return "<all environments>".to_string();
    }

    let simplified = simplify_condition(condition.clone());
    let condition = &simplified;

    let combined = combinable_ranges(condition);
    let negation_sets = atom_negation_sets(condition);

    let mut parts: Vec<String> = Vec::new();
    let mut emitted: Vec<&PackageName> = Vec::new();
    for (literal, positive) in condition {
        if let Some((_, range)) = combined
            .iter()
            .find(|(package, _)| **package == literal.package)
        {
            // The combined range renders once, at the group's first literal.
            if !emitted.contains(&&literal.package) {
                emitted.push(&literal.package);
                parts.push(format!("{} {range}", literal.package.as_normalized()));
            }
        } else if let Some((_, set)) = negation_sets
            .iter()
            .find(|(package, _)| **package == literal.package)
        {
            // The negated atom set renders once, at the group's first
            // literal.
            if !emitted.contains(&&literal.package) {
                emitted.push(&literal.package);
                parts.push(set.clone());
            }
        } else if *positive {
            parts.push(format_literal(literal));
        } else {
            parts.push(format!("not ({})", format_literal(literal)));
        }
    }
    parts.join(" AND ")
}

/// Simplifies one conjunction of signed environment literals into an
/// equivalent, more compact conjunction where this is exactly representable
/// in the public [`EnvironmentCondition`] shape.
///
/// The simplifier is intentionally conservative: it only drops literals that
/// are provably redundant and only folds same-package version/build
/// constraints when the result is a single positive literal. In particular,
/// a negative-only version constraint such as `not (__cuda >=13)` is left
/// alone because it also holds when `__cuda` is absent; with an explicit
/// `not (__cuda absent)` presence literal it can be rendered as
/// `__cuda <13`.
fn simplify_condition(condition: EnvironmentCondition) -> EnvironmentCondition {
    let mut simplified = Vec::new();
    for (package, group) in group_by_package(&condition) {
        let group = drop_redundant_literals(group);
        if group.len() > 1
            && let Some(group) = finite_build_group(package, &group)
        {
            simplified.extend(group);
            continue;
        }
        if group.len() > 1
            && let Some(ranges) = group_ranges(&group)
            && let Some(rendered) = render_single_interval(&ranges)
            && let Ok(spec) = NamelessMatchSpec::from_str(&rendered, ParseStrictness::Strict)
        {
            simplified.push((
                EnvironmentLiteral {
                    package: package.clone(),
                    kind: EnvironmentLiteralKind::Matches(spec),
                },
                true,
            ));
            continue;
        }
        simplified.extend(
            group
                .into_iter()
                .map(|(literal, positive)| (literal.clone(), positive)),
        );
    }
    simplified
}

/// Computes, per package with more than one literal in the condition, the
/// single contiguous version range exactly equivalent to all the package's
/// signed literals, for the packages where that is possible (see
/// [`display_condition`]). Packages whose literals cannot be combined are
/// not in the result.
fn combinable_ranges(condition: &EnvironmentCondition) -> Vec<(&PackageName, String)> {
    let mut combined = Vec::new();
    for (package, group) in group_by_package(condition) {
        // A single literal already renders as well as it can; a group
        // without a presence constraint has no present-value domain from
        // which to subtract negative literals.
        if group.len() < 2 || !group_has_presence_constraint(&group) {
            continue;
        }
        let group = drop_redundant_literals(group);
        let Some(ranges) = group_ranges(&group) else {
            continue;
        };
        let Some(rendered) = render_single_interval(&ranges) else {
            continue;
        };
        combined.push((package, rendered));
    }
    combined
}

/// Drops literals in one same-package conjunction group that are provably
/// redundant from other literals in the same group.
///
/// - A positive `Matches` literal implies `not Absent`.
/// - A positive `Absent` literal implies every `not Matches` literal.
/// - A positive `Matches` literal disjoint from a negative `Matches` literal
///   implies that negative literal.
fn drop_redundant_literals<'a>(
    group: Vec<(&'a EnvironmentLiteral, bool)>,
) -> Vec<(&'a EnvironmentLiteral, bool)> {
    let has_positive_match = group.iter().any(|(literal, positive)| {
        *positive && matches!(literal.kind, EnvironmentLiteralKind::Matches(_))
    });
    let has_positive_absent = group.iter().any(|(literal, positive)| {
        *positive && matches!(literal.kind, EnvironmentLiteralKind::Absent)
    });
    let positive_specs: Vec<&NamelessMatchSpec> = group
        .iter()
        .filter_map(|(literal, positive)| match (&literal.kind, *positive) {
            (EnvironmentLiteralKind::Matches(spec), true) => Some(spec),
            _ => None,
        })
        .collect();

    group
        .into_iter()
        .filter(|(literal, positive)| {
            if *positive {
                return true;
            }
            match &literal.kind {
                EnvironmentLiteralKind::Absent => !has_positive_match,
                EnvironmentLiteralKind::Matches(negative) => {
                    !has_positive_absent
                        && !positive_specs.iter().any(|positive| {
                            match_spec_relation(positive, negative)
                                == resolvo::VersionSetRelation::Disjoint
                        })
                }
            }
        })
        .collect()
}

/// Whether a group constrains the package to be present, either through a
/// positive value match or an explicit `not absent` literal.
fn group_has_presence_constraint(group: &[(&EnvironmentLiteral, bool)]) -> bool {
    group.iter().any(|(literal, positive)| match &literal.kind {
        EnvironmentLiteralKind::Matches(_) => *positive,
        EnvironmentLiteralKind::Absent => !*positive,
    })
}

/// Groups the signed literals of a condition by package, preserving
/// first-appearance order of both the packages and each package's literals.
fn group_by_package(
    condition: &EnvironmentCondition,
) -> Vec<(&PackageName, Vec<(&EnvironmentLiteral, bool)>)> {
    let mut groups: Vec<(&PackageName, Vec<(&EnvironmentLiteral, bool)>)> = Vec::new();
    for (literal, positive) in condition {
        match groups
            .iter_mut()
            .find(|(package, _)| **package == literal.package)
        {
            Some((_, group)) => group.push((literal, *positive)),
            None => groups.push((&literal.package, vec![(literal, *positive)])),
        }
    }
    groups
}

/// The exact version range set of a group of signed literals on one
/// package: the intersection of the positive literals' ranges (or the full
/// present-value domain when `not absent` explicitly forces presence) minus
/// the negative literals' ranges. `None` when any literal is not a
/// version-only match/presence literal, a positive literal's version spec is
/// not exactly representable, or a negative literal is not exactly
/// representable yet could overlap the positive base.
fn group_ranges(group: &[(&EnvironmentLiteral, bool)]) -> Option<Ranges<Version>> {
    let mut versions = Vec::with_capacity(group.len());
    let mut has_presence_constraint = false;
    for (literal, positive) in group {
        match &literal.kind {
            EnvironmentLiteralKind::Matches(spec) => {
                if !spec_has_only_version_part(spec) {
                    return None;
                }
                has_presence_constraint |= *positive;
                versions.push((spec.version.as_ref(), *positive));
            }
            EnvironmentLiteralKind::Absent if !*positive => {
                has_presence_constraint = true;
            }
            EnvironmentLiteralKind::Absent => return None,
        }
    }
    if !has_presence_constraint {
        return None;
    }

    // The positive literals must be exactly representable; their
    // intersection is the base that the negatives carve out of. An absent
    // negative without any positive match gives a full present-value base.
    let mut positive = Ranges::full();
    for (version, is_positive) in &versions {
        if *is_positive {
            positive = positive.intersection(&version_spec_to_ranges_opt(*version)?);
        }
    }

    let mut result = positive.clone();
    for (version, is_positive) in &versions {
        if *is_positive {
            continue;
        }
        if let Some(exact) = version_spec_to_ranges_opt(*version) {
            result = result.intersection(&exact.complement());
        } else {
            // The negated spec has no exact range (e.g. a `X.0.*` prefix,
            // ambiguous under conda zero-padding). It only changes the
            // result if it can overlap the positive base; the envelope is a
            // superset, so when even that is disjoint the negation removes
            // nothing and is dropped. Otherwise the range is not exact.
            let envelope = version_spec_to_ranges_envelope_opt(*version)?;
            if !positive.intersection(&envelope).is_empty() {
                return None;
            }
        }
    }
    Some(result)
}

fn version_spec_to_ranges_opt(version: Option<&VersionSpec>) -> Option<Ranges<Version>> {
    match version {
        Some(version) => version_spec_to_ranges(version),
        None => Some(Ranges::full()),
    }
}

fn version_spec_to_ranges_envelope_opt(version: Option<&VersionSpec>) -> Option<Ranges<Version>> {
    match version {
        Some(version) => version_spec_to_ranges_envelope(version),
        None => Some(Ranges::full()),
    }
}

/// Whether the spec has no modeled field except the optional version part.
/// Strict by destructuring: a field added to [`NamelessMatchSpec`] fails to
/// compile here and must be classified explicitly.
fn spec_has_only_version_part(spec: &NamelessMatchSpec) -> bool {
    let NamelessMatchSpec {
        version: _,
        build,
        build_number,
        file_name,
        extras,
        flags,
        channel,
        subdir,
        namespace,
        md5,
        sha256,
        url,
        license,
        license_family,
        condition,
        track_features,
    } = spec;
    build.is_none()
        && build_number.is_none()
        && file_name.is_none()
        && extras.is_none()
        && flags.is_none()
        && channel.is_none()
        && subdir.is_none()
        && namespace.is_none()
        && md5.is_none()
        && sha256.is_none()
        && url.is_none()
        && license.is_none()
        && license_family.is_none()
        && condition.is_none()
        && track_features.is_none()
}

/// Extracts the finite-build atom form of a spec: an optional version part
/// plus a finite set of exact build strings, and nothing else. Such a
/// literal describes one or more atomic values (e.g. `__archspec 1.*
/// skylake` or `__archspec 1.* ^(skylake|zen4)$`), so groups of them render
/// as name sets. Strict by destructuring, like
/// [`spec_has_only_version_part`].
fn spec_as_finite_atom_set(
    spec: &NamelessMatchSpec,
) -> Option<(Option<&VersionSpec>, Vec<String>)> {
    let NamelessMatchSpec {
        version,
        build: Some(build),
        build_number: None,
        file_name: None,
        extras: None,
        flags: None,
        channel: None,
        subdir: None,
        namespace: None,
        md5: None,
        sha256: None,
        url: None,
        license: None,
        license_family: None,
        condition: None,
        track_features: None,
    } = spec
    else {
        return None;
    };
    Some((version.as_ref(), literal_alternatives(build)?))
}

/// Renders an atom name set as `<package> <version> in {a, b, c}` (names
/// alphabetical, deduplicated; the version part is omitted when absent).
fn format_atom_set(
    package: &PackageName,
    version: Option<&str>,
    names: &mut Vec<String>,
) -> String {
    normalize_literal_names(names);
    match version {
        Some(version) => format!(
            "{} {version} in {{{}}}",
            package.as_normalized(),
            names.join(", ")
        ),
        None => format!("{} in {{{}}}", package.as_normalized(), names.join(", ")),
    }
}

fn normalize_literal_names(names: &mut Vec<String>) {
    names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
}

fn contains_literal_name(names: &[String], candidate: &str) -> bool {
    names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(candidate))
}

fn plain_regex_literal(name: &str) -> bool {
    const META: &[u8] = b".^$*+?()[]{}\\|";
    !name.is_empty() && !name.bytes().any(|c| META.contains(&c))
}

fn matcher_for_literal_names(names: &[String]) -> Option<StringMatcher> {
    match names {
        [] => None,
        [single] => Some(StringMatcher::Exact(single.clone())),
        many if many.iter().all(|name| plain_regex_literal(name)) => {
            let pattern = format!("^({})$", many.join("|"));
            Some(StringMatcher::Regex(Box::new(
                RegexMatcher::new(&pattern).ok()?,
            )))
        }
        _ => None,
    }
}

fn format_literal(literal: &EnvironmentLiteral) -> String {
    if let EnvironmentLiteralKind::Matches(spec) = &literal.kind
        && let Some((version, mut names)) = spec_as_finite_atom_set(spec)
        && names.len() > 1
    {
        return format_atom_set(
            &literal.package,
            version.map(ToString::to_string).as_deref(),
            &mut names,
        );
    }
    literal.to_string()
}

/// Simplifies a group of finite build-string atoms sharing the same package
/// and version part. Positive atoms intersect their finite build sets;
/// negative atoms subtract from that finite base. The result is represented
/// as a single positive exact/anchored-alternation build matcher when that is
/// possible.
fn finite_build_group(
    package: &PackageName,
    group: &[(&EnvironmentLiteral, bool)],
) -> Option<EnvironmentCondition> {
    let mut version: Option<Option<VersionSpec>> = None;
    let mut selected: Option<Vec<String>> = None;
    let mut exclusions: Vec<Vec<String>> = Vec::new();

    for (literal, positive) in group {
        let EnvironmentLiteralKind::Matches(spec) = &literal.kind else {
            return None;
        };
        let (spec_version, mut names) = spec_as_finite_atom_set(spec)?;
        normalize_literal_names(&mut names);
        let spec_version = spec_version.cloned();
        match &version {
            None => version = Some(spec_version.clone()),
            Some(existing) if *existing == spec_version => {}
            Some(_) => return None,
        }
        if *positive {
            selected = Some(match selected.take() {
                Some(current) => current
                    .into_iter()
                    .filter(|name| contains_literal_name(&names, name))
                    .collect(),
                None => names,
            });
        } else {
            exclusions.push(names);
        }
    }

    let mut selected = selected?;
    for excluded in exclusions {
        selected.retain(|name| !contains_literal_name(&excluded, name));
    }
    normalize_literal_names(&mut selected);
    let build = matcher_for_literal_names(&selected)?;
    let spec = NamelessMatchSpec {
        version: version.flatten(),
        build: Some(build),
        ..NamelessMatchSpec::default()
    };
    Some(vec![(
        EnvironmentLiteral {
            package: package.clone(),
            kind: EnvironmentLiteralKind::Matches(spec),
        },
        true,
    )])
}

/// Computes, per package whose literals in the condition are two or more
/// negated exact-build atoms sharing one version part, the compact
/// `not (<package> <version> in {a, b, c})` rendering. Packages whose
/// literals do not have that shape are not in the result.
fn atom_negation_sets(condition: &EnvironmentCondition) -> Vec<(&PackageName, String)> {
    let mut sets = Vec::new();
    'packages: for (package, group) in group_by_package(condition) {
        if group.len() < 2 || group.iter().any(|(_, positive)| *positive) {
            continue;
        }
        let mut names: Vec<String> = Vec::new();
        let mut version_part: Option<Option<String>> = None;
        for (literal, _) in &group {
            let EnvironmentLiteralKind::Matches(spec) = &literal.kind else {
                continue 'packages;
            };
            let Some((version, mut atom_names)) = spec_as_finite_atom_set(spec) else {
                continue 'packages;
            };
            let rendered = version.map(ToString::to_string);
            match &version_part {
                None => version_part = Some(rendered),
                Some(existing) if *existing == rendered => {}
                Some(_) => continue 'packages,
            }
            names.append(&mut atom_names);
        }
        let version = version_part.flatten();
        let set = format_atom_set(package, version.as_deref(), &mut names);
        sets.push((package, format!("not ({set})")));
    }
    sets
}

/// Renders a presence (a disjunction of conditions, as stored in
/// [`CondaUniversalSolution::merged`] and edges) in a human readable way.
/// Disjuncts that differ only in the name of a single positive exact-build
/// atom literal merge into one disjunct with a name set: seventeen
/// `__archspec` alternatives over the same glibc range render as
/// `__glibc >=2.17,<3.0.a0 AND __archspec 1.* in {broadwell, ...}`.
/// Remaining disjuncts render through [`display_condition`], joined with
/// `OR`; multi-literal disjuncts are parenthesized when there is more than
/// one. The empty disjunction renders as `<no environment>`.
pub fn display_presence(presence: &[EnvironmentCondition]) -> String {
    struct Group<'a> {
        base: &'a EnvironmentCondition,
        rendered: Vec<(String, bool)>,
        /// The merged atom slot: literal index in `base` plus the rendered
        /// version part shared by all merged atoms.
        slot: Option<(usize, Option<String>)>,
        names: Vec<String>,
    }

    if presence.is_empty() {
        return "<no environment>".to_string();
    }

    // The literal at `index` as a mergeable atom set: positive, finite-build
    // atom(s), and the only literal of its package in the disjunct.
    let atom_at =
        |condition: &EnvironmentCondition, index: usize| -> Option<(Option<String>, Vec<String>)> {
            let (literal, positive) = &condition[index];
            if !positive {
                return None;
            }
            let EnvironmentLiteralKind::Matches(spec) = &literal.kind else {
                return None;
            };
            let (version, mut names) = spec_as_finite_atom_set(spec)?;
            normalize_literal_names(&mut names);
            let package_is_unique = condition
                .iter()
                .enumerate()
                .all(|(other, (l, _))| other == index || l.package != literal.package);
            package_is_unique.then(|| (version.map(ToString::to_string), names))
        };

    let mut groups: Vec<Group<'_>> = Vec::new();
    'disjuncts: for condition in presence {
        let rendered: Vec<(String, bool)> = condition
            .iter()
            .map(|(literal, positive)| (literal.to_string(), *positive))
            .collect();
        for group in &mut groups {
            if group.rendered.len() != rendered.len() {
                continue;
            }
            if let Some((index, version)) = &group.slot {
                let index = *index;
                let same_rest = rendered
                    .iter()
                    .enumerate()
                    .all(|(i, part)| i == index || *part == group.rendered[i]);
                if !same_rest {
                    continue;
                }
                let Some((atom_version, names)) = atom_at(condition, index) else {
                    continue;
                };
                if condition[index].0.package != group.base[index].0.package
                    || atom_version != *version
                {
                    continue;
                }
                group.names.extend(names);
                continue 'disjuncts;
            } else {
                let differing: Vec<usize> = (0..rendered.len())
                    .filter(|&i| rendered[i] != group.rendered[i])
                    .collect();
                let [index] = differing.as_slice() else {
                    continue;
                };
                let index = *index;
                let (Some((base_version, base_names)), Some((version, names))) =
                    (atom_at(group.base, index), atom_at(condition, index))
                else {
                    continue;
                };
                if condition[index].0.package != group.base[index].0.package
                    || base_version != version
                {
                    continue;
                }
                group.slot = Some((index, base_version));
                group.names.extend(base_names);
                group.names.extend(names);
                continue 'disjuncts;
            }
        }
        groups.push(Group {
            base: condition,
            rendered,
            slot: None,
            names: Vec::new(),
        });
    }

    let rendered_groups: Vec<String> = groups
        .iter()
        .map(|group| match &group.slot {
            None => display_condition(group.base),
            Some((index, version)) => {
                let mut names = group.names.clone();
                let set = format_atom_set(
                    &group.base[*index].0.package,
                    version.as_deref(),
                    &mut names,
                );
                let rest: EnvironmentCondition = group
                    .base
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| i != index)
                    .map(|(_, literal)| literal.clone())
                    .collect();
                if rest.is_empty() {
                    set
                } else {
                    format!("{} AND {set}", display_condition(&rest))
                }
            }
        })
        .collect();

    if rendered_groups.len() == 1 {
        rendered_groups.into_iter().next().expect("one group")
    } else {
        rendered_groups
            .iter()
            .map(|part| {
                if part.contains(" AND ") {
                    format!("({part})")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" OR ")
    }
}

/// Renders a range set as a conda-style version range when it is a single
/// non-empty contiguous interval, `None` otherwise: `>=x` / `>x` for the
/// lower bound and `<y` / `<=y` for the upper bound, joined by a comma; the
/// full set renders as `*`.
fn render_single_interval(ranges: &Ranges<Version>) -> Option<String> {
    let mut segments = ranges.iter();
    let (low, high) = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    let low = match low {
        Bound::Included(version) => Some(format!(">={version}")),
        Bound::Excluded(version) => Some(format!(">{version}")),
        Bound::Unbounded => None,
    };
    let high = match high {
        Bound::Excluded(version) => Some(format!("<{version}")),
        Bound::Included(version) => Some(format!("<={version}")),
        Bound::Unbounded => None,
    };
    Some(match (low, high) {
        (Some(low), Some(high)) => format!("{low},{high}"),
        (Some(low), None) => low,
        (None, Some(high)) => high,
        (None, None) => "*".to_string(),
    })
}

/// The canonical form of one package's literals within a presence disjunct;
/// see [`simplify_presence`].
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
enum CanonicalEntry {
    /// The package's literals are exactly one `Absent` literal with the
    /// given sign.
    Absent(bool),

    /// All the package's literals are version-only matches, at least one of
    /// them positive (so the package is necessarily present): the exact
    /// version interval set they describe.
    Ranges(Ranges<Version>),

    /// Anything else (a build matcher or other non-version field, a
    /// negative-only group, an unconvertible version spec): the original
    /// signed literals, compared verbatim.
    Raw(Vec<(EnvironmentLiteral, bool)>),
}

/// One disjunct of a presence disjunction in canonical form: a map from
/// package name to [`CanonicalEntry`], in first-appearance order.
#[derive(Clone, Debug)]
struct CanonicalDisjunct {
    /// A semantically accurate rendering used verbatim when the canonical
    /// entries cannot be rendered back into literals.
    fallback: EnvironmentCondition,

    /// The per-package entries, or `None` when a future canonicalization
    /// step cannot model the disjunct structurally; such a disjunct only
    /// takes part in identical-disjunct deduplication.
    entries: Option<Vec<(PackageName, CanonicalEntry)>>,
}

impl CanonicalDisjunct {
    /// Whether the disjunct describes an empty region (some package's
    /// version interval set is empty). An empty region contributes nothing
    /// to a disjunction.
    fn is_empty_region(&self) -> bool {
        self.entries.as_ref().is_some_and(|entries| {
            entries.iter().any(|(_, entry)| {
                matches!(entry, CanonicalEntry::Ranges(ranges) if ranges.iter().next().is_none())
            })
        })
    }
}

/// Canonicalizes one disjunct; see [`CanonicalDisjunct`].
fn canonicalize_disjunct(condition: EnvironmentCondition) -> CanonicalDisjunct {
    let entries = canonical_entries(&condition);
    CanonicalDisjunct {
        fallback: condition,
        entries,
    }
}

/// Computes the per-package canonical entries of a disjunct.
fn canonical_entries(
    condition: &EnvironmentCondition,
) -> Option<Vec<(PackageName, CanonicalEntry)>> {
    let mut entries = Vec::new();
    for (package, group) in group_by_package(condition) {
        let group = drop_redundant_literals(group);
        if group.iter().any(|(literal, positive)| {
            *positive && matches!(literal.kind, EnvironmentLiteralKind::Absent)
        }) && group.len() != 1
        {
            return None;
        }
        let has_absent = group
            .iter()
            .any(|(literal, _)| matches!(literal.kind, EnvironmentLiteralKind::Absent));
        let entry = if has_absent && group.len() == 1 {
            CanonicalEntry::Absent(group[0].1)
        } else if group_has_presence_constraint(&group)
            && let Some(ranges) = group_ranges(&group)
        {
            // A positive literal or `not absent` implies the package is
            // present, so the group is exactly a version interval set. A
            // negative-only group without an explicit presence constraint
            // also holds when the package is absent, which no interval set
            // can express; it stays `Raw`.
            CanonicalEntry::Ranges(ranges)
        } else {
            CanonicalEntry::Raw(
                group
                    .iter()
                    .map(|(literal, positive)| ((*literal).clone(), *positive))
                    .collect(),
            )
        };
        entries.push((package.clone(), entry));
    }
    Some(entries)
}

/// Renders canonical entries back into an [`EnvironmentCondition`]:
/// packages keep their first-appearance order; an `Absent` entry becomes
/// the absent literal with its sign; a `Raw` entry keeps its original
/// literals; a `Ranges` entry becomes one positive literal parsed
/// (strictly) from the conda-style rendering of its interval. `None` when
/// some `Ranges` entry is not a single contiguous interval or its rendering
/// does not parse back.
fn render_entries(entries: &[(PackageName, CanonicalEntry)]) -> Option<EnvironmentCondition> {
    let mut condition = Vec::new();
    for (package, entry) in entries {
        match entry {
            CanonicalEntry::Absent(sign) => condition.push((
                EnvironmentLiteral {
                    package: package.clone(),
                    kind: EnvironmentLiteralKind::Absent,
                },
                *sign,
            )),
            CanonicalEntry::Raw(literals) => condition.extend(literals.iter().cloned()),
            CanonicalEntry::Ranges(ranges) => {
                let rendered = render_single_interval(ranges)?;
                let spec = NamelessMatchSpec::from_str(&rendered, ParseStrictness::Strict).ok()?;
                condition.push((
                    EnvironmentLiteral {
                        package: package.clone(),
                        kind: EnvironmentLiteralKind::Matches(spec),
                    },
                    true,
                ));
            }
        }
    }
    Some(condition)
}

/// Whether `x` describes a superset region of `y`, making `y` redundant in
/// a disjunction: every package `x` constrains appears in `y` with an
/// at-least-as-tight entry, and `x` constrains no package that `y` does not
/// (extra constraints in `y` only shrink its region further). Disjuncts
/// that failed to canonicalize only absorb, and are absorbed by, verbatim
/// identical ones.
fn absorbs(x: &CanonicalDisjunct, y: &CanonicalDisjunct) -> bool {
    let (Some(xs), Some(ys)) = (&x.entries, &y.entries) else {
        return x.entries.is_none() && y.entries.is_none() && x.fallback == y.fallback;
    };
    xs.iter().all(|(package, x_entry)| {
        ys.iter()
            .find(|(candidate, _)| candidate == package)
            .is_some_and(|(_, y_entry)| match (x_entry, y_entry) {
                (CanonicalEntry::Ranges(x_ranges), CanonicalEntry::Ranges(y_ranges)) => {
                    y_ranges.subset_of(x_ranges)
                }
                (x_entry, y_entry) => x_entry == y_entry,
            })
    })
}

/// Merges two disjuncts that constrain the same packages and differ in
/// exactly one package whose entries are both `Ranges`, provided the union
/// of those ranges is a single contiguous interval: the pair describes
/// `common AND (range1 OR range2)`, which the merged disjunct expresses as
/// one range. Entries keep `x`'s order. `None` when the pair is not
/// mergeable, including when the merged disjunct would not render back into
/// literals (every surviving disjunct must stay renderable because a merged
/// disjunct has no faithful original to fall back to).
fn try_merge(x: &CanonicalDisjunct, y: &CanonicalDisjunct) -> Option<CanonicalDisjunct> {
    let (Some(xs), Some(ys)) = (&x.entries, &y.entries) else {
        return None;
    };
    if xs.len() != ys.len() {
        return None;
    }
    let mut merged = Vec::with_capacity(xs.len());
    let mut differing = 0_usize;
    for (package, x_entry) in xs {
        let (_, y_entry) = ys.iter().find(|(candidate, _)| candidate == package)?;
        if x_entry == y_entry {
            merged.push((package.clone(), x_entry.clone()));
            continue;
        }
        let (CanonicalEntry::Ranges(x_ranges), CanonicalEntry::Ranges(y_ranges)) =
            (x_entry, y_entry)
        else {
            return None;
        };
        let union = x_ranges.union(y_ranges);
        {
            let mut segments = union.iter();
            if segments.next().is_none() || segments.next().is_some() {
                return None;
            }
        }
        differing += 1;
        merged.push((package.clone(), CanonicalEntry::Ranges(union)));
    }
    if differing != 1 {
        return None;
    }
    let fallback = render_entries(&merged)?;
    Some(CanonicalDisjunct {
        fallback,
        entries: Some(merged),
    })
}

/// Simplifies a presence disjunction (an OR of conjunctions of signed
/// environment literals) into fewer, more readable disjuncts describing
/// exactly the same region of the environment space:
///
/// - empty-region disjuncts are dropped;
/// - a disjunct whose region lies inside another disjunct's region is
///   absorbed (this also deduplicates identical disjuncts);
/// - two disjuncts that differ in exactly one package by version ranges
///   whose union is a single contiguous interval are merged into one.
///
/// The rules are applied as a fixpoint over all pairs (disjunction sizes
/// are small, so the quadratic passes are cheap) and surviving disjuncts
/// keep their first-appearance relative order. Each survivor is rendered
/// back in canonical form: per package one positive range literal where the
/// package's literals are exactly a contiguous version interval, the
/// original literals otherwise.
fn simplify_presence(presence: Vec<EnvironmentCondition>) -> Vec<EnvironmentCondition> {
    let mut disjuncts: Vec<CanonicalDisjunct> = presence
        .into_iter()
        .map(canonicalize_disjunct)
        .filter(|disjunct| !disjunct.is_empty_region())
        .collect();

    'fixpoint: loop {
        // ABSORPTION: drop any disjunct whose region lies inside another's.
        for i in 0..disjuncts.len() {
            for j in 0..disjuncts.len() {
                if i != j && absorbs(&disjuncts[i], &disjuncts[j]) {
                    // Mutual absorption means equal regions: keep the
                    // earlier disjunct.
                    let drop = if absorbs(&disjuncts[j], &disjuncts[i]) {
                        i.max(j)
                    } else {
                        j
                    };
                    disjuncts.remove(drop);
                    continue 'fixpoint;
                }
            }
        }
        // MERGE: replace the first mergeable pair by its union.
        for i in 0..disjuncts.len() {
            for j in i + 1..disjuncts.len() {
                if let Some(merged) = try_merge(&disjuncts[i], &disjuncts[j]) {
                    disjuncts[i] = merged;
                    disjuncts.remove(j);
                    continue 'fixpoint;
                }
            }
        }
        break;
    }

    disjuncts
        .into_iter()
        .map(|disjunct| {
            disjunct
                .entries
                .as_deref()
                .and_then(render_entries)
                .unwrap_or(disjunct.fallback)
        })
        .collect()
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
    ///
    /// Every entry must hold on EVERY machine in the modeled space: derive
    /// this set from the *target* platform (`__unix` and `__linux` for a
    /// linux solve), never from host detection. A host-detected set
    /// describes one machine, and for a cross-platform solve the wrong
    /// operating system entirely; a missing always-true package such as
    /// `__unix` silently excludes every build that depends on it, which
    /// both degrades the solution (packages fall back to old builds without
    /// the dependency) and blows up solve time (the search becomes
    /// near-unsatisfiable).
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

    let environment_model = EnvironmentModel::new(
        environment_model
            .iter()
            .map(|disjunction| {
                EnvClause::new(
                    disjunction
                        .iter()
                        .map(|(literal, positive)| {
                            SignedEnvLiteral::from((intern_literal(&provider, literal), *positive))
                        })
                        .collect(),
                )
            })
            .collect(),
    );
    let seed_partition = seed_partition
        .iter()
        .filter_map(|condition| {
            let literals = condition
                .iter()
                .map(|(literal, positive)| {
                    SignedEnvLiteral::from((intern_literal(&provider, literal), *positive))
                })
                .collect();
            ResolvoCellCondition::new(literals).ok()
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
            let condition_literals = convert_condition(solver.provider(), &cell);
            return Err(UniversalSolveError::Unsolvable {
                condition: display_condition(&condition_literals),
                condition_literals,
                conflict: conflict.display_user_friendly(&solver).to_string(),
            });
        }
        Err(UniversalFailure::InvalidInput(e)) => {
            return Err(SolveError::Unsolvable(vec![format!("{e:?}")]).into());
        }
        Err(UniversalFailure::Cancelled(_)) => {
            return Err(UniversalSolveError::Cancelled);
        }
        Err(other) => {
            return Err(SolveError::Unsolvable(vec![format!("{other:?}")]).into());
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
        .cells()
        .iter()
        .map(|cell| {
            (
                convert_condition(provider, cell.condition()),
                cell.solvables()
                    .iter()
                    .filter_map(|&solvable| record_for_solvable(provider, solvable))
                    .collect(),
            )
        })
        .collect();

    // Distinct presences are few (bounded by cell combinations) while merged
    // entries and edges number in the hundreds, and converting a presence is
    // expensive (the disjunct simplification fixpoint). Convert each
    // distinct presence once; `Presence` is `Eq` but not `Hash`, so this is
    // a linear-scan memo, which is fine at these sizes.
    let mut presence_cache: Vec<(resolvo::Presence<NameId>, Vec<EnvironmentCondition>)> =
        Vec::new();
    let mut cached_convert = |presence: resolvo::Presence<NameId>| -> Vec<EnvironmentCondition> {
        match presence_cache.iter().find(|(key, _)| *key == presence) {
            Some((_, converted)) => converted.clone(),
            None => {
                let converted = convert_presence(provider, &presence);
                presence_cache.push((presence, converted.clone()));
                converted
            }
        }
    };

    let merged = solution
        .merged()
        .into_iter()
        .filter_map(|(solvable, presence)| {
            Some((
                record_for_solvable(provider, solvable)?,
                cached_convert(presence),
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
                presence: cached_convert(presence),
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
    match &literal.kind {
        EnvironmentLiteralKind::Matches(spec) => EnvLiteral::Matches(
            provider
                .pool
                .intern_version_set(name_id, spec.clone().into()),
        ),
        EnvironmentLiteralKind::Absent => EnvLiteral::Absent(name_id),
    }
}

/// Converts a resolvo cell condition back into conda terms.
fn convert_condition(
    provider: &CondaDependencyProvider<'_>,
    condition: &resolvo::CellCondition<NameId>,
) -> EnvironmentCondition {
    simplify_condition(
        condition
            .literals()
            .map(|signed| (convert_literal(provider, &signed.literal), signed.positive))
            .collect(),
    )
}

/// Converts a resolvo presence (a disjunction of cell conditions) back into
/// conda terms and simplifies the disjunction into fewer equivalent
/// disjuncts (see [`simplify_presence`]).
fn convert_presence(
    provider: &CondaDependencyProvider<'_>,
    presence: &resolvo::Presence<NameId>,
) -> Vec<EnvironmentCondition> {
    simplify_presence(
        presence
            .disjuncts()
            .map(|condition| convert_condition(provider, condition))
            .collect(),
    )
}

/// Converts a resolvo environment literal back into conda terms.
fn convert_literal(
    provider: &CondaDependencyProvider<'_>,
    literal: &EnvLiteral<NameId>,
) -> EnvironmentLiteral {
    match *literal {
        EnvLiteral::Matches(version_set) => EnvironmentLiteral {
            package: package_name(
                provider,
                provider.pool.resolve_version_set_package_name(version_set),
            ),
            kind: EnvironmentLiteralKind::Matches(nameless_spec(provider, version_set)),
        },
        EnvLiteral::Absent(package) => EnvironmentLiteral {
            package: package_name(provider, package),
            kind: EnvironmentLiteralKind::Absent,
        },
    }
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
            solution.cells()[first].condition().display(provider),
            solution.cells()[second].condition().display(provider),
        ),
        resolvo::Violation::UnprovenDisjointness { first, second } => format!(
            "cells {first} ({}) and {second} ({}) have different records and their \
             disjointness could not be proven",
            solution.cells()[first].condition().display(provider),
            solution.cells()[second].condition().display(provider),
        ),
        resolvo::Violation::UncoveredRegion(condition) => format!(
            "the environment region {} is not covered by any cell",
            condition.display(provider),
        ),
        other => format!("unexpected verification violation: {other:?}"),
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

    fn absent_literal(package: &str) -> EnvironmentLiteral {
        EnvironmentLiteral {
            package: PackageName::new_unchecked(package),
            kind: EnvironmentLiteralKind::Absent,
        }
    }

    /// Two or more negated exact-build atoms of one package render as a
    /// single negated name set instead of a chain of `not (...)` literals
    /// (the complement region of an archspec partition).
    #[test]
    fn test_display_condition_renders_negated_atom_set() {
        let condition = vec![
            (matches_literal("__glibc", ">=2.17,<3.0.a0"), true),
            (matches_literal("__archspec", "1 zen4"), false),
            (matches_literal("__archspec", "1 haswell"), false),
            (matches_literal("__archspec", "1 core2"), false),
        ];
        insta::assert_snapshot!(
            display_condition(&condition),
            @"__glibc >=2.17,<3.0.a0 AND not (__archspec ==1 in {core2, haswell, zen4})"
        );
    }

    /// A single negated atom keeps the plain rendering, and atoms with
    /// differing version parts do not collapse into a set.
    #[test]
    fn test_display_condition_negated_atom_set_fallbacks() {
        let single = vec![
            (matches_literal("__glibc", ">=2.17"), true),
            (matches_literal("__archspec", "1 zen4"), false),
        ];
        insta::assert_snapshot!(
            display_condition(&single),
            @"__glibc >=2.17 AND not (__archspec ==1 zen4)"
        );

        let mixed_versions = vec![
            (matches_literal("__archspec", "1 zen4"), false),
            (matches_literal("__archspec", "2 haswell"), false),
        ];
        insta::assert_snapshot!(
            display_condition(&mixed_versions),
            @"not (__archspec ==1 zen4) AND not (__archspec ==2 haswell)"
        );
    }

    /// Presence disjuncts that differ only in the name of one positive
    /// exact-build atom merge into a single disjunct with a name set.
    #[test]
    fn test_display_presence_merges_atom_alternatives() {
        let disjunct = |name: &str| {
            vec![
                (matches_literal("__glibc", ">=2.17,<3.0.a0"), true),
                (matches_literal("__archspec", &format!("1 {name}")), true),
            ]
        };
        let presence = vec![disjunct("zen4"), disjunct("haswell"), disjunct("core2")];
        insta::assert_snapshot!(
            display_presence(&presence),
            @"__glibc >=2.17,<3.0.a0 AND __archspec ==1 in {core2, haswell, zen4}"
        );
    }

    /// Disjuncts that differ in more than the atom name stay separate, and
    /// multi-literal disjuncts are parenthesized when more than one remains.
    #[test]
    fn test_display_presence_keeps_distinct_disjuncts() {
        let presence = vec![
            vec![(matches_literal("__cuda", ">=12.0"), true)],
            vec![
                (absent_literal("__cuda"), true),
                (matches_literal("__glibc", ">=2.17"), true),
            ],
        ];
        insta::assert_snapshot!(
            display_presence(&presence),
            @"__cuda >=12.0 OR (__cuda absent AND __glibc >=2.17)"
        );
    }

    /// Atom merging composes with the per-disjunct range combining: the
    /// non-atom literals render through display_condition.
    #[test]
    fn test_display_presence_merges_atoms_and_combines_ranges() {
        let disjunct = |name: &str| {
            vec![
                (matches_literal("__cuda", ">=12.0"), true),
                (matches_literal("__cuda", ">=13"), false),
                (matches_literal("__archspec", &format!("1 {name}")), true),
            ]
        };
        let presence = vec![disjunct("zen4"), disjunct("haswell")];
        insta::assert_snapshot!(
            display_presence(&presence),
            @"__cuda >=12.0,<13 AND __archspec ==1 in {haswell, zen4}"
        );
    }

    /// Same-package version-only literals combine into one contiguous
    /// range: a positive floor with a negated higher floor becomes a
    /// half-open interval.
    #[test]
    fn test_display_condition_combines_cuda_range() {
        let condition = vec![
            (matches_literal("__cuda", ">=12.0"), true),
            (matches_literal("__cuda", ">=13"), false),
        ];
        assert_eq!(display_condition(&condition), "__cuda >=12.0,<13");
    }

    /// Shared `<3.0.a0` style upper bounds cancel exactly: the combined
    /// range only keeps the bounds that actually differ.
    #[test]
    fn test_display_condition_combines_glibc_range() {
        let condition = vec![
            (matches_literal("__glibc", ">=2.17,<3.0.a0"), true),
            (matches_literal("__glibc", ">=2.28,<3.0.a0"), false),
        ];
        assert_eq!(display_condition(&condition), "__glibc >=2.17,<2.28");
    }

    /// A negative build matcher that is disjoint from a positive build
    /// matcher is redundant and is dropped.
    #[test]
    fn test_display_condition_drops_redundant_build_matcher() {
        let condition = vec![
            (matches_literal("__archspec", "1 x86_64_v3"), true),
            (matches_literal("__archspec", "1 x86_64_v2"), false),
        ];
        assert_eq!(display_condition(&condition), "__archspec ==1 x86_64_v3");
    }

    /// A finite build matcher can be subtracted by negated exact build
    /// atoms and rendered as a smaller build-name set.
    #[test]
    fn test_display_condition_subtracts_finite_build_set() {
        let condition = vec![
            (
                matches_literal("__archspec", "1 ^(core2|haswell|zen4)$"),
                true,
            ),
            (matches_literal("__archspec", "1 haswell"), false),
        ];
        assert_eq!(
            display_condition(&condition),
            "__archspec ==1 in {core2, zen4}"
        );
    }

    /// A `not absent` literal is redundant when a positive version literal
    /// already implies the package is present, so it is dropped and the
    /// group combines into the version range.
    #[test]
    fn test_display_condition_drops_redundant_absent() {
        let condition = vec![
            (matches_literal("__cuda", ">=12.0"), true),
            (absent_literal("__cuda"), false),
        ];
        assert_eq!(display_condition(&condition), "__cuda >=12.0");
    }

    /// A `not absent` literal with no positive version literal is a presence
    /// constraint, so negative version literals can be subtracted from the
    /// full present-version domain.
    #[test]
    fn test_display_condition_uses_not_absent_as_presence() {
        let condition = vec![
            (matches_literal("__cuda", ">=13"), false),
            (absent_literal("__cuda"), false),
        ];
        assert_eq!(display_condition(&condition), "__cuda <13");
    }

    /// A group without any positive literal is untouched.
    #[test]
    fn test_display_condition_negative_only_group_untouched() {
        let condition = vec![
            (matches_literal("__cuda", ">=13"), false),
            (matches_literal("__cuda", "<11"), false),
        ];
        assert_eq!(
            display_condition(&condition),
            "not (__cuda >=13) AND not (__cuda <11)"
        );
    }

    /// An empty combined range (contradictory literals) is untouched, and
    /// unrelated packages render per literal exactly as before.
    #[test]
    fn test_display_condition_empty_or_split_results_fall_back() {
        // Contradictory positives: the intersection is empty.
        let condition = vec![
            (matches_literal("__cuda", ">=13"), true),
            (matches_literal("__cuda", "<12"), true),
        ];
        assert_eq!(display_condition(&condition), "__cuda >=13 AND __cuda <12");
        // A negation punching a hole splits the range in two intervals.
        let condition = vec![
            (matches_literal("__cuda", ">=11"), true),
            (matches_literal("__cuda", "==12"), false),
        ];
        assert_eq!(
            display_condition(&condition),
            "__cuda >=11 AND not (__cuda ==12)"
        );
    }

    /// Combining is per package: a combinable group renders at the position
    /// of its first literal while other packages keep their rendering.
    #[test]
    fn test_display_condition_mixed_packages() {
        let condition = vec![
            (matches_literal("__cuda", ">=12.0"), true),
            (absent_literal("__osx"), true),
            (matches_literal("__cuda", ">=13"), false),
        ];
        assert_eq!(
            display_condition(&condition),
            "__cuda >=12.0,<13 AND __osx absent"
        );
    }

    /// `__archspec` literals evaluate with exact build-string matching, the
    /// same semantics conda and `filter_candidates` apply: per CEP 30 a
    /// machine reports exactly one microarchitecture name, and a literal is
    /// satisfied only by that name. DAG lineage does NOT count (an
    /// `x86_64_v4` machine does not satisfy an `x86_64_v3` literal); the
    /// conda-forge `_x86_64-microarch-level` metapackages encode lineage by
    /// shipping one build per concrete microarchitecture name instead.
    #[test]
    fn test_evaluate_archspec_literal_exact() {
        let v4_machine = [virtual_package("__archspec", "1", "x86_64_v4")];
        assert!(matches_literal("__archspec", "* x86_64_v4").evaluate(&v4_machine));
        assert!(!matches_literal("__archspec", "* x86_64_v3").evaluate(&v4_machine));
        assert!(!matches_literal("__archspec", "* x86_64").evaluate(&v4_machine));
        assert!(!matches_literal("__archspec", "* aarch64").evaluate(&v4_machine));
        let skylake_machine = [virtual_package("__archspec", "1", "skylake_avx512")];
        assert!(!matches_literal("__archspec", "* sapphirerapids").evaluate(&skylake_machine));
        assert!(matches_literal("__archspec", "* skylake_avx512").evaluate(&skylake_machine));
        // Names outside the archspec DAG are still just strings.
        let unknown_machine = [virtual_package("__archspec", "1", "mysterychip")];
        assert!(matches_literal("__archspec", "* mysterychip").evaluate(&unknown_machine));
        assert!(!matches_literal("__archspec", "* x86_64").evaluate(&unknown_machine));
    }

    /// Renders a simplified presence for assertion.
    fn simplified(presence: Vec<EnvironmentCondition>) -> Vec<String> {
        simplify_presence(presence)
            .iter()
            .map(display_condition)
            .collect()
    }

    /// Whether a presence disjunction holds on a concrete machine.
    fn presence_holds(
        presence: &[EnvironmentCondition],
        machine: &[GenericVirtualPackage],
    ) -> bool {
        presence.iter().any(|condition| {
            condition
                .iter()
                .all(|(literal, sign)| literal.evaluate(machine) == *sign)
        })
    }

    /// The real-world 12-disjunct `__glibc` x `__cuda` grid: the glibc bands
    /// `2.17..2.28`, `2.28..2.34` and `2.34..3.0.a0` partition
    /// `[2.17, 3.0.a0)` and the cuda bands stack up likewise, so the grid
    /// collapses to three disjuncts describing exactly the same region
    /// (checked by evaluation below): per glibc band one contiguous cuda
    /// range, plus the cuda-absent band over all of glibc.
    #[test]
    fn test_simplify_presence_collapses_cuda_glibc_grid() {
        let g = |spec: &str| (matches_literal("__glibc", spec), true);
        let c = |spec: &str| (matches_literal("__cuda", spec), true);
        let presence = vec![
            vec![g(">=2.34,<3.0.a0"), c(">=12.4,<12.8")],
            vec![g(">=2.28,<2.34"), c(">=12.4")],
            vec![g(">=2.17,<2.28"), c(">=12.4")],
            vec![g(">=2.34,<3.0.a0"), c(">=12.2,<12.4")],
            vec![g(">=2.28,<2.34"), c(">=12.2,<12.4")],
            vec![c(">=12.0,<12.2"), g(">=2.34,<3.0.a0")],
            vec![c(">=12.0,<12.2"), g(">=2.28,<2.34")],
            vec![g(">=2.17,<2.28"), c(">=12.2,<12.4")],
            vec![c(">=12.0,<12.2"), g(">=2.17,<2.28")],
            vec![(absent_literal("__cuda"), true), g(">=2.34,<3.0.a0")],
            vec![(absent_literal("__cuda"), true), g(">=2.28,<2.34")],
            vec![(absent_literal("__cuda"), true), g(">=2.17,<2.28")],
        ];

        let result = simplify_presence(presence.clone());
        assert_eq!(
            result.iter().map(display_condition).collect::<Vec<_>>(),
            vec![
                "__glibc >=2.34,<3.0.a0 AND __cuda >=12.0,<12.8",
                "__glibc >=2.17,<2.34 AND __cuda >=12.0",
                "__cuda absent AND __glibc >=2.17,<3.0.a0",
            ]
        );

        // The simplified disjunction describes exactly the same region:
        // evaluate both on a grid of machines straddling every band edge.
        let glibcs = [
            None,
            Some("2.10"),
            Some("2.17"),
            Some("2.20"),
            Some("2.28"),
            Some("2.30"),
            Some("2.34"),
            Some("2.40"),
            Some("3.0"),
        ];
        let cudas = [
            None,
            Some("11.0"),
            Some("12.0"),
            Some("12.1"),
            Some("12.2"),
            Some("12.3"),
            Some("12.4"),
            Some("12.5"),
            Some("12.8"),
            Some("12.9"),
        ];
        for glibc in glibcs {
            for cuda in cudas {
                let mut machine = Vec::new();
                if let Some(glibc) = glibc {
                    machine.push(virtual_package("__glibc", glibc, "0"));
                }
                if let Some(cuda) = cuda {
                    machine.push(virtual_package("__cuda", cuda, "0"));
                }
                assert_eq!(
                    presence_holds(&result, &machine),
                    presence_holds(&presence, &machine),
                    "region mismatch for glibc={glibc:?} cuda={cuda:?}"
                );
            }
        }
    }

    /// A disjunct with FEWER constraints describes a LARGER region: the
    /// plain glibc disjunct is a superset of the glibc-band-plus-cuda-absent
    /// disjunct (the extra cuda constraint only shrinks it), so the latter
    /// is absorbed.
    #[test]
    fn test_simplify_presence_absorbs_more_constrained_disjunct() {
        let presence = vec![
            vec![(matches_literal("__glibc", ">=2.17,<3.0.a0"), true)],
            vec![
                (matches_literal("__glibc", ">=2.28,<2.34"), true),
                (absent_literal("__cuda"), true),
            ],
        ];
        assert_eq!(simplified(presence), vec!["__glibc >=2.17,<3.0.a0"]);
    }

    /// A `Raw` entry (a build matcher is not version-only) participates in
    /// merging as long as it is identical on both sides: the other
    /// package's adjacent glibc bands merge across it.
    #[test]
    fn test_simplify_presence_merges_across_identical_raw_entry() {
        let presence = vec![
            vec![
                (matches_literal("__archspec", "1 x86_64_v3"), true),
                (matches_literal("__glibc", ">=2.17,<2.28"), true),
            ],
            vec![
                (matches_literal("__archspec", "1 x86_64_v3"), true),
                (matches_literal("__glibc", ">=2.28,<3.0.a0"), true),
            ],
        ];
        assert_eq!(
            simplified(presence),
            vec!["__archspec ==1 x86_64_v3 AND __glibc >=2.17,<3.0.a0"]
        );
    }

    /// `Raw` entries are never range-merged themselves: two disjuncts
    /// differing only in their `__archspec` build matcher stay separate.
    #[test]
    fn test_simplify_presence_never_merges_raw_entries() {
        let presence = vec![
            vec![
                (matches_literal("__archspec", "1 x86_64_v3"), true),
                (matches_literal("__cuda", ">=12"), true),
            ],
            vec![
                (matches_literal("__archspec", "1 x86_64_v2"), true),
                (matches_literal("__cuda", ">=12"), true),
            ],
        ];
        assert_eq!(
            simplified(presence),
            vec![
                "__archspec ==1 x86_64_v3 AND __cuda >=12",
                "__archspec ==1 x86_64_v2 AND __cuda >=12",
            ]
        );
    }

    /// A non-contiguous union (`<11` and `>=12` leave a gap) must not
    /// merge: the merged range would wrongly include the gap.
    #[test]
    fn test_simplify_presence_keeps_non_contiguous_union() {
        let presence = vec![
            vec![(matches_literal("__cuda", "<11"), true)],
            vec![(matches_literal("__cuda", ">=12"), true)],
        ];
        assert_eq!(simplified(presence), vec!["__cuda <11", "__cuda >=12"]);
    }

    /// A disjunct whose version constraints are contradictory describes the
    /// empty region and contributes nothing to the disjunction: it is
    /// dropped entirely.
    #[test]
    fn test_simplify_presence_drops_empty_range_disjunct() {
        let presence = vec![
            vec![
                (matches_literal("__cuda", ">=13"), true),
                (matches_literal("__cuda", "<12"), true),
            ],
            vec![(matches_literal("__glibc", ">=2.17"), true)],
        ];
        assert_eq!(simplified(presence), vec!["__glibc >=2.17"]);
    }

    /// `not absent` participates in canonicalization: it is redundant next
    /// to a positive value match, so duplicate disjuncts still collapse.
    #[test]
    fn test_simplify_presence_canonicalizes_not_absent() {
        let weird = vec![
            (matches_literal("__cuda", ">=12.0"), true),
            (absent_literal("__cuda"), false),
        ];
        let presence = vec![weird.clone(), weird];
        assert_eq!(simplified(presence), vec!["__cuda >=12.0"]);
    }
}
