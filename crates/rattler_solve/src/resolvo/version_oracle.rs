//! Conversion of conda [`VersionSpec`]s into [`version_ranges::Ranges`] over
//! [`Version`], the basis of the relation oracle for symbolic virtual
//! packages (universal solving).
//!
//! [`Ranges`] is an interval set defined over the same `Ord` that
//! [`VersionSpec::matches`] uses for its comparison operators, so all plain
//! comparison operators translate exactly. The prefix operators
//! (`StartsWith`, i.e. `1.2.*`, and `Compatible`, i.e. `~=1.2`) are interval
//! shaped too, but their boundaries need care; see
//! [`starts_with_ranges`] for the full derivation, the supported prefix
//! shapes, and the precisely documented blind spots.
//!
//! The contract validated by the agreement property test below: whenever
//! [`version_spec_to_ranges`] returns `Some(ranges)`,
//! `spec.matches(&version) == ranges.contains(&version)` for every version.

use std::str::FromStr;

use rattler_conda_types::{
    Component, Version, VersionBumpType, VersionSpec,
    version_spec::{EqualityOperator, LogicalOperator, RangeOperator, StrictRangeOperator},
};
use version_ranges::Ranges;

/// Converts a [`VersionSpec`] into the equivalent [`Ranges`] over
/// [`Version`], or `None` when the spec is not exactly representable as an
/// interval set under `Version`'s `Ord` (see the module docs).
///
/// Returning `None` is always safe for the relation oracle: it degrades the
/// answer to [`resolvo::VersionSetRelation::Unknown`].
// The relation oracle that consumes this lands in a follow-up commit.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn version_spec_to_ranges(spec: &VersionSpec) -> Option<Ranges<Version>> {
    Some(match spec {
        VersionSpec::None => Ranges::empty(),
        VersionSpec::Any => Ranges::full(),
        // The comparison operators evaluate through `Version`'s `Ord`, which
        // is exactly the order `Ranges` uses; these are exact by definition.
        VersionSpec::Range(op, limit) => match op {
            RangeOperator::Greater => Ranges::strictly_higher_than(limit.clone()),
            RangeOperator::GreaterEquals => Ranges::higher_than(limit.clone()),
            RangeOperator::Less => Ranges::strictly_lower_than(limit.clone()),
            RangeOperator::LessEquals => Ranges::lower_than(limit.clone()),
        },
        // `Version`'s `Eq` agrees with `Ord` (both pad missing components
        // with zeros), so singleton sets are exact as well.
        VersionSpec::Exact(op, limit) => match op {
            EqualityOperator::Equals => Ranges::singleton(limit.clone()),
            EqualityOperator::NotEquals => Ranges::singleton(limit.clone()).complement(),
        },
        VersionSpec::StrictRange(op, prefix) => match op {
            StrictRangeOperator::StartsWith => starts_with_ranges(&prefix.0)?,
            StrictRangeOperator::NotStartsWith => starts_with_ranges(&prefix.0)?.complement(),
            StrictRangeOperator::Compatible => compatible_ranges(&prefix.0)?,
            StrictRangeOperator::NotCompatible => compatible_ranges(&prefix.0)?.complement(),
        },
        VersionSpec::Group(LogicalOperator::And, group) => {
            let mut result = Ranges::full();
            for sub in group {
                result = result.intersection(&version_spec_to_ranges(sub)?);
            }
            result
        }
        VersionSpec::Group(LogicalOperator::Or, group) => {
            let mut result = Ranges::empty();
            for sub in group {
                result = result.union(&version_spec_to_ranges(sub)?);
            }
            result
        }
    })
}

/// Converts the `StartsWith` operator (`prefix.*`) into ranges.
///
/// # Derivation
///
/// `Version::starts_with(prefix)` holds when the epoch matches, every
/// non-last prefix segment is matched componentwise (the version may omit
/// trailing zero components, but may not add components), the version's
/// segment at the last prefix position starts componentwise with the prefix
/// segment (arbitrary extra components are allowed there: `1.0.1c` starts
/// with `1.0.1`), and any further version segments are free.
///
/// For prefixes of the supported shape (below) this set is an interval in
/// the version order:
///
/// - every version deviating from a non-last prefix segment sorts strictly
///   outside the prefix's own segment value (identifier components sort
///   below the zero padding, `post` above), and
/// - within the last prefix segment, all and only the component lists that
///   start with the prefix's final numeral `n` lie between the lists led by
///   `n - 1` and those led by `n + 1`.
///
/// The boundaries of that interval are limits that no version attains (the
/// infimum is `prefix` extended by an unbounded descending `dev` tower), so
/// the conversion uses the closest attainable bounds, built with the `dev`
/// component, which sorts below every other component:
///
/// - lower bound (included): `{prefix}dev`, the `dev` pre-release of the
///   prefix itself (e.g. `1.1.* -> 1.1dev`). Every matching version sorts at
///   or above it; every version below the prefix's numeral cut sorts below.
/// - upper bound (excluded): `{bump_last(prefix)}dev`, the `dev`
///   pre-release of the prefix successor (e.g. `1.1.* -> 1.2dev`), the floor
///   of the first non-matching region above.
///
/// # Supported prefix shapes
///
/// `Some` is returned only when all of the following hold; each exclusion is
/// a genuine non-representability, not a shortcut:
///
/// - **no local part**: a local prefix selects on the local component, which
///   is not contiguous in the version order (`1.0+a` and `1.0.1+a` match
///   `1.0+a.*` but the in-between `1.0.0.5+b` does not).
/// - **every non-last segment ends in a numeral**: an identifier-ending
///   segment is `Ord`-equal to itself plus a trailing zero, but only one of
///   the two matches (`1.1a.5` matches `1.1a.5.*`, the `Ord`-equal
///   `1.1a0.5` does not), so the matching set is not closed under
///   `Ord`-equality and no interval can represent it.
/// - **the last segment is a pure nonzero-or-only numeral and, when the
///   prefix has more than one segment, not zero**: a trailing zero segment
///   re-creates the closure problem with realistic versions (`2024a`
///   matches `2024.0.*` while the `Ord`-equal `2024a.0` does not), and an
///   identifier-ending last segment has no constructible successor bound.
///
/// # Known blind spot (documented, deliberate)
///
/// Versions that continue below a `dev` floor, i.e. extend the prefix (or
/// its successor) with a `dev` component followed by further sub-zero
/// components (`1.2dev0dev`, `1.2dev.rc1` against `1.1.*`), are
/// misclassified. No interval representation can handle them: the true
/// boundary is an unattainable limit (a proof sketch lives in the agreement
/// test's generator docs). No real-world package versions itself below its
/// own `dev` pre-release, so the practical impact is nil; the agreement
/// test's version grammar documents and enforces exactly this exclusion.
fn starts_with_ranges(prefix: &Version) -> Option<Ranges<Version>> {
    if !prefix_is_representable(prefix) {
        return None;
    }
    let low = dev_floor(prefix)?;
    let high = dev_floor(&prefix.bump(VersionBumpType::Last).ok()?)?;
    Some(Ranges::between(low, high))
}

/// Converts the `Compatible` operator (`~=limit`) into ranges.
///
/// `Version::compatible_with(limit)` is `self >= limit` (exact under `Ord`)
/// AND epoch equality AND `starts_with` on the all-but-last-segment prefix
/// of `limit`. The `>= limit` part supplies an exact lower bound, which also
/// cuts away every below-prefix deviation, so only the upper bound of the
/// `starts_with` component is needed:
///
/// - multi-segment limit: the dev floor of the bumped prefix, exactly as in
///   [`starts_with_ranges`] (`~=1.2.3 -> >=1.2.3, <1.3dev`). The prefix must
///   be of the supported shape; note a trailing zero segment in the prefix
///   is again non-representable (`1post` matches `~=1.0.3` while the
///   `Ord`-equal `1post.0` does not).
/// - single-segment limit: the prefix is empty and only epoch equality
///   remains; the upper bound is the dev floor of the next epoch
///   (`~=2 -> >=2, <1!0dev`).
fn compatible_ranges(limit: &Version) -> Option<Ranges<Version>> {
    if limit.has_local() {
        return None;
    }
    let high = if limit.segment_count() == 1 {
        Version::from_str(&format!("{}!0dev", limit.epoch() + 1)).ok()?
    } else {
        let prefix = limit
            .pop_segments(1)
            .expect("a multi-segment version can pop one segment");
        if !prefix_is_representable(&prefix) {
            return None;
        }
        dev_floor(&prefix.bump(VersionBumpType::Last).ok()?)?
    };
    Some(Ranges::higher_than(limit.clone()).intersection(&Ranges::strictly_lower_than(high)))
}

/// Whether a `starts_with` prefix has the supported shape described in
/// [`starts_with_ranges`]: no local part, every non-last segment ends in a
/// numeral component, and the last segment is a single numeral that is only
/// zero when it is the sole segment.
fn prefix_is_representable(prefix: &Version) -> bool {
    if prefix.has_local() {
        return false;
    }
    let segment_count = prefix.segments().len();
    for (index, segment) in prefix.segments().enumerate() {
        let is_last = index + 1 == segment_count;
        if is_last {
            let mut components = segment.components();
            let single = components.next();
            if components.next().is_some() {
                return false;
            }
            match single {
                Some(Component::Numeral(_)) => {}
                _ => return false,
            }
            if segment_count > 1 && segment.is_zero() {
                return false;
            }
        } else {
            match segment.components().next_back() {
                Some(Component::Numeral(_)) => {}
                _ => return false,
            }
        }
    }
    true
}

/// Returns the `dev` pre-release floor of a version: the version with a
/// `dev` component appended to its last segment (`1.2 -> 1.2dev`). Only
/// valid for versions whose last segment ends in a numeral, which
/// [`prefix_is_representable`] and [`Version::bump`] guarantee here.
fn dev_floor(version: &Version) -> Option<Version> {
    Version::from_str(&format!("{version}dev")).ok()
}

#[cfg(test)]
mod tests {
    use rattler_conda_types::{ParseStrictness, StrictVersion};

    use super::*;

    fn spec(s: &str) -> VersionSpec {
        VersionSpec::from_str(s, ParseStrictness::Lenient).unwrap()
    }

    fn version(s: &str) -> Version {
        Version::from_str(s).unwrap()
    }

    /// Asserts that the spec converts and that membership of each version
    /// agrees with `VersionSpec::matches` (the conversion is also checked,
    /// the expectation lists are hand-written from the matching semantics).
    fn assert_members(spec_str: &str, contained: &[&str], excluded: &[&str]) {
        let spec = spec(spec_str);
        let ranges = version_spec_to_ranges(&spec)
            .unwrap_or_else(|| panic!("`{spec_str}` should be representable"));
        for v in contained {
            let v = version(v);
            assert!(spec.matches(&v), "`{spec_str}` should match `{v}`");
            assert!(
                ranges.contains(&v),
                "ranges of `{spec_str}` should contain `{v}`"
            );
        }
        for v in excluded {
            let v = version(v);
            assert!(!spec.matches(&v), "`{spec_str}` should not match `{v}`");
            assert!(
                !ranges.contains(&v),
                "ranges of `{spec_str}` should not contain `{v}`"
            );
        }
    }

    #[test]
    fn test_plain_comparison_operators() {
        // Note `2.17dev.1` sorts below `2.17`: the `dev` component sorts
        // below the zero padding of the shorter version.
        assert_members(
            ">=2.17",
            &["2.17", "2.17.1", "3"],
            &["2.16", "2.17dev", "2.17dev.1", "2"],
        );
        assert_members(">2.17", &["2.17.1", "3"], &["2.17", "2.17.0", "2.16"]);
        assert_members(
            "<=12.1",
            &["12.1", "12.1.0", "12.0", "11"],
            &["12.1.1", "12.2", "13"],
        );
        assert_members(
            "<12.1",
            &["12.0", "12.1a0", "12.1dev"],
            &["12.1", "12.1.0.0", "12.2"],
        );
    }

    #[test]
    fn test_exact_operators() {
        // `Version` equality pads with zeros, so `12.1` == `12.1.0`.
        assert_members("==12.1", &["12.1", "12.1.0"], &["12.1.1", "12.1a0", "12.0"]);
        assert_members("!=12.1", &["12.1.1", "12.0", "0"], &["12.1", "12.1.0"]);
    }

    #[test]
    fn test_any_and_none() {
        let any = version_spec_to_ranges(&VersionSpec::Any).unwrap();
        assert!(any.contains(&version("1.2.3")));
        let none = version_spec_to_ranges(&VersionSpec::None).unwrap();
        assert!(!none.contains(&version("1.2.3")));
    }

    #[test]
    fn test_starts_with_membership() {
        // rattler's `starts_with` allows arbitrary extra components within
        // the last prefix segment, so `1.1a0` and `1.1dev` both match
        // `1.1.*` (conda/rattler#1914 semantics).
        assert_members(
            "1.1.*",
            &[
                "1.1", "1.1.5", "1.1.0.4", "1.1a0", "1.1c", "1.1dev", "1.1rc1", "1.1+x.y",
            ],
            &["1.2", "1.0.9", "1.2dev", "1.10", "1.2a0", "2.1", "1!1.1"],
        );
        // Single-segment prefix: anything whose first segment starts with
        // `12` (and only those).
        assert_members(
            "12.*",
            &["12", "12.9", "12dev", "12a", "12.0.1"],
            &["11.9", "13", "120", "13dev", "1.2"],
        );
        // Epochs are part of the prefix.
        assert_members("1!1.2.*", &["1!1.2", "1!1.2.3"], &["1.2", "2!1.2", "1!1.3"]);
    }

    #[test]
    fn test_not_starts_with_membership() {
        assert_members(
            "!=1.1.*",
            &["1.2", "1.0.9", "1.2dev"],
            &["1.1", "1.1.5", "1.1a0"],
        );
    }

    #[test]
    fn test_compatible_membership() {
        // `~=2.4` is `>=2.4` and sharing the all-but-last-segment prefix.
        assert_members(
            "~=2.4",
            &["2.4", "2.5", "2.99", "2.4.9"],
            &["2.3", "3.1", "3.0dev", "1!2.5"],
        );
        assert_members(
            "~=1.2.3",
            &["1.2.3", "1.2.10", "1.2.3post"],
            &["1.2.2", "1.3.0", "1.3dev", "2.0"],
        );
        // Single-segment limit: only the epoch bounds remain.
        assert_members("~=2", &["2", "3", "99.5"], &["1.9", "1!1", "1!3"]);
    }

    #[test]
    fn test_group_operators() {
        assert_members(
            ">=2.17,<3.0a0",
            &["2.17", "2.28"],
            &["2.16", "3.0a0", "3.0", "3.1"],
        );
        assert_members("11.*|12.*", &["11.1", "12.9"], &["10.9", "13.0"]);
        // Nested groups.
        assert_members(">=1.2,<2|>3.1", &["1.5", "3.2"], &["1.1", "2.5", "3.1"]);
    }

    #[test]
    fn test_unrepresentable_prefixes() {
        // A trailing zero segment makes the matching set inconsistent under
        // `Ord`-equality: `2024a` matches `2024.0.*` while the Ord-equal
        // `2024a.0` does not. Both directions are realistic versions, so no
        // interval representation exists and the conversion must refuse.
        assert!(version_spec_to_ranges(&spec("1.0.*")).is_none());
        assert!(version_spec_to_ranges(&spec("2.38.0.*")).is_none());
        // An iden-ending last segment has no constructible successor bound
        // (and non-last iden-ending segments break Ord-equality closure:
        // `1.1a.5` matches `1.1a.5.*` while the Ord-equal `1.1a0.5` does
        // not).
        assert!(
            version_spec_to_ranges(&VersionSpec::StrictRange(
                StrictRangeOperator::StartsWith,
                StrictVersion(version("1.1a")),
            ))
            .is_none()
        );
        // Local parts in the prefix select on the local component, which is
        // not contiguous in the version order.
        assert!(
            version_spec_to_ranges(&VersionSpec::StrictRange(
                StrictRangeOperator::StartsWith,
                StrictVersion(version("1.1+x")),
            ))
            .is_none()
        );
        // Compatible inherits the prefix rules for its all-but-last prefix.
        assert!(version_spec_to_ranges(&spec("~=1.0.3")).is_none());
        // An unrepresentable member poisons the whole group.
        assert!(version_spec_to_ranges(&spec(">=1.2,1.0.*")).is_none());
        // ... but representable groups stay representable.
        assert!(version_spec_to_ranges(&spec(">=1.2,<2")).is_some());
    }

    // =======================================================================
    // The agreement property test: the soundness anchor of the conversion.
    // =======================================================================

    /// A deterministic xorshift* generator so the test is reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn chance(&mut self, pct: u64) -> bool {
            self.next() % 100 < pct
        }

        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[self.below(xs.len())]
        }
    }

    /// Generates a diverse but realistic version: optional epoch, one to
    /// four numeric segments, an optional letter suffix on the last
    /// segment, an optional pre-release tag segment, optional trailing zero
    /// segments and an optional local part.
    ///
    /// Deliberately excluded shapes (with reasoning, see the module docs):
    ///
    /// - a second pre-release marker nested below a `dev` marker (e.g.
    ///   `1.2dev0dev`, `1.2dev.rc1`): these sort below the dev floor of
    ///   their own release and no real package versions itself like that.
    ///   No interval representation can classify them correctly (the
    ///   infimum of a prefix's matching set is not attainable).
    /// - letter components attached to a non-final segment with further
    ///   segments following (e.g. `1c.5`, `1.0rc1.2`): only relevant for
    ///   trailing-zero prefixes, which the conversion refuses anyway.
    fn gen_version(rng: &mut Rng) -> Version {
        const NUMERALS: &[&str] = &[
            "0", "1", "2", "3", "5", "10", "11", "12", "100", "217", "228", "2024",
        ];
        const SUFFIXES: &[&str] = &["a", "b", "c", "g", "rc", "dev", "post"];
        const TAGS: &[&str] = &[
            "a0", "b1", "rc1", "rc2", "alpha", "beta", "dev", "dev0", "post",
        ];
        const LOCALS: &[&str] = &["1", "3.4", "x.y", "0a"];

        let mut s = String::new();
        if rng.chance(15) {
            s.push_str(if rng.chance(50) { "1!" } else { "2!" });
        }
        let nseg = 1 + rng.below(4);
        for i in 0..nseg {
            if i > 0 {
                s.push('.');
            }
            s.push_str(rng.pick(NUMERALS).as_ref());
        }
        let mut dev_terminal = false;
        if rng.chance(25) {
            let suffix = rng.pick(SUFFIXES);
            dev_terminal = *suffix == "dev";
            s.push_str(suffix);
        }
        if !dev_terminal && rng.chance(20) {
            s.push('.');
            s.push_str(rng.pick(TAGS).as_ref());
        }
        if rng.chance(15) {
            s.push_str(".0");
            if rng.chance(30) {
                s.push_str(".0");
            }
        }
        if rng.chance(10) {
            s.push('+');
            s.push_str(rng.pick(LOCALS).as_ref());
        }
        Version::from_str(&s).unwrap()
    }

    /// Generates an atomic spec over a random version from the pool.
    fn gen_atom(rng: &mut Rng, versions: &[Version]) -> VersionSpec {
        let v = rng.pick(versions).clone();
        match rng.below(12) {
            0 => VersionSpec::Range(RangeOperator::Greater, v),
            1 => VersionSpec::Range(RangeOperator::GreaterEquals, v),
            2 => VersionSpec::Range(RangeOperator::Less, v),
            3 => VersionSpec::Range(RangeOperator::LessEquals, v),
            4 => VersionSpec::Exact(EqualityOperator::Equals, v),
            5 => VersionSpec::Exact(EqualityOperator::NotEquals, v),
            6 | 7 => VersionSpec::StrictRange(StrictRangeOperator::StartsWith, StrictVersion(v)),
            8 => VersionSpec::StrictRange(StrictRangeOperator::NotStartsWith, StrictVersion(v)),
            9 => VersionSpec::StrictRange(StrictRangeOperator::Compatible, StrictVersion(v)),
            10 => VersionSpec::StrictRange(StrictRangeOperator::NotCompatible, StrictVersion(v)),
            _ => VersionSpec::Any,
        }
    }

    /// Generates a spec: an atom or a (possibly nested) group of atoms.
    fn gen_spec(rng: &mut Rng, versions: &[Version], depth: usize) -> VersionSpec {
        if depth == 0 || rng.chance(60) {
            return gen_atom(rng, versions);
        }
        let op = if rng.chance(50) {
            LogicalOperator::And
        } else {
            LogicalOperator::Or
        };
        let n = 2 + rng.below(2);
        VersionSpec::Group(
            op,
            (0..n).map(|_| gen_spec(rng, versions, depth - 1)).collect(),
        )
    }

    /// For every generated `(spec, version)` pair the converted ranges must
    /// agree exactly with `VersionSpec::matches`. Any disagreement is a bug
    /// in the conversion; the conversion is fixed, never the test.
    #[test]
    fn test_agreement_property() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

        // Hand-picked boundary probes plus a large generated pool.
        let mut versions: Vec<Version> = [
            "0",
            "0.0",
            "1",
            "1.0",
            "1.1",
            "1.2",
            "2",
            "2.4",
            "2.17",
            "2.38",
            "3.0a0",
            "11",
            "11.8",
            "12",
            "12.1",
            "217",
            "228",
            "1.1dev",
            "1.2dev",
            "1.1.dev",
            "1.2dev.0",
            "1.1a0",
            "1.2a0",
            "1.0.1c",
            "2.39dev",
            "1!1.2",
            "1!1.2.3",
            "1.2.3+4.5",
            "2.17.1",
            "1.10",
            "1.1rc1",
            "1post",
            "1.1post",
            "2.38.0",
        ]
        .iter()
        .map(|s| version(s))
        .collect();
        for _ in 0..70 {
            versions.push(gen_version(&mut rng));
        }

        let specs: Vec<VersionSpec> = (0..600).map(|_| gen_spec(&mut rng, &versions, 2)).collect();

        /// Whether the spec contains a prefix (strict-range) operator, the
        /// interesting case for boundary coverage.
        fn has_strict_operator(spec: &VersionSpec) -> bool {
            match spec {
                VersionSpec::StrictRange(..) => true,
                VersionSpec::Group(_, group) => group.iter().any(has_strict_operator),
                _ => false,
            }
        }

        let mut convertible = 0usize;
        let mut convertible_strict = 0usize;
        let mut checks = 0usize;
        for spec in &specs {
            let Some(ranges) = version_spec_to_ranges(spec) else {
                continue;
            };
            convertible += 1;
            convertible_strict += usize::from(has_strict_operator(spec));
            for v in &versions {
                checks += 1;
                assert_eq!(
                    spec.matches(v),
                    ranges.contains(v),
                    "conversion disagreement for spec `{spec}` and version `{v}`",
                );
            }
        }

        // The conversion must cover the bulk of realistic specs; with the
        // grammar above only prefix operators on unrepresentable shapes
        // (trailing-zero / suffixed / local prefixes) may refuse. The
        // boundary-heavy prefix operators must be well represented among
        // the converted specs, or the test would not exercise them.
        assert!(
            convertible * 100 >= specs.len() * 60,
            "only {convertible} of {} specs converted",
            specs.len(),
        );
        assert!(
            convertible_strict >= 50,
            "only {convertible_strict} converted specs contain a prefix operator",
        );
        assert!(
            checks >= 30_000,
            "expected at least 30000 agreement checks, performed {checks}",
        );
        println!(
            "agreement: {} specs, {} versions, {convertible} convertible specs \
             ({convertible_strict} with prefix operators), {checks} checks",
            specs.len(),
            versions.len(),
        );
    }
}
