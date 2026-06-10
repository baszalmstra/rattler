//! Conversion of [`VersionSpec`]s into [`version_ranges::Ranges`] interval
//! sets over [`Version`], which makes range algebra (intersection, union,
//! complement, subset and disjointness reasoning) available for conda
//! version specs.
//!
//! # Exactness
//!
//! [`Ranges`] is an interval set defined over the same `Ord` that
//! [`VersionSpec::matches`] uses for its comparison operators. For a spec to
//! have an exact interval form, the set of versions it matches must satisfy
//! two properties:
//!
//! 1. it must be a union of intervals under [`Version`]'s ordering, and
//! 2. it must be closed under `Ord`-equality: conda versions pad missing
//!    trailing components with zeros, so distinct versions compare equal
//!    (`1.0 == 1.0.0`) and an interval cannot contain one without the other.
//!
//! The plain comparison operators (`>`, `>=`, `<`, `<=`) satisfy both
//! trivially, and so do the equality operators (`==`, `!=`) because
//! [`Version`]'s `Eq` agrees with its `Ord`. The prefix operators
//! (`StartsWith`, i.e. `1.2.*`, and `Compatible`, i.e. `~=1.2`) are interval
//! shaped too, but their boundaries are unattainable limits that need care
//! (the conversion bounds them with `dev` pre-release floors, the closest
//! attainable versions), and some prefix shapes have no interval form at
//! all. The conversion is therefore fallible: [`VersionSpecRangesError`]
//! names each excluded shape together with the counterexample that proves
//! its non-representability.
//!
//! # Contract
//!
//! The contract validated by the agreement property test in this module:
//! whenever [`VersionSpec::to_ranges`] returns `Ok(ranges)`,
//! `spec.matches(&version) == ranges.contains(&version)` for every version,
//! with one precisely documented blind spot for versions that version
//! themselves below their own `dev` floor (see
//! [`VersionSpec::to_ranges`]).

use std::str::FromStr;

use thiserror::Error;
use version_ranges::Ranges;

use crate::{
    Component, Version, VersionBumpType, VersionSpec,
    version_spec::{EqualityOperator, LogicalOperator, RangeOperator, StrictRangeOperator},
};

/// The reason a [`VersionSpec`] has no exact representation as a
/// [`version_ranges::Ranges`] interval set under [`Version`]'s ordering.
///
/// Every variant is a genuine non-representability backed by a concrete
/// counterexample, not a shortcut of the implementation; see the module
/// documentation for the two properties an exact conversion requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
pub enum VersionSpecRangesError {
    /// The prefix of a starts-with (`1.0+a.*`) or compatible (`~=1.0+a`)
    /// operator contains a local version part. A local prefix selects on the
    /// local component, which is not contiguous in the version order:
    /// `1.0+a` and `1.0.1+a` match `=1.0+a` but versions ordered in between,
    /// such as `1.0.0.5+b`, do not.
    #[error(
        "a prefix with a local version part selects versions non-contiguously (`1.0+a` and `1.0.1+a` match `=1.0+a`, versions ordered in between do not)"
    )]
    LocalVersionPrefix,

    /// A non-last segment of the prefix ends in an identifier, so the
    /// matching set is not closed under `Ord`-equality: `1.1a.5` matches
    /// `1.1a.5.*` while the equal-sorting `1.1a0.5` does not, and no
    /// interval can contain one without the other.
    #[error(
        "a non-last prefix segment ends in an identifier, so the matching set is not closed under ordering-equality (`1.1a.5` matches `1.1a.5.*`, the equal-sorting `1.1a0.5` does not)"
    )]
    NonNumeralPrefixSegment,

    /// The last segment of the prefix is not a single numeral, so the prefix
    /// has no constructible successor to bound the interval from above (and
    /// identifier segments re-create the `Ord`-equality closure problem of
    /// [`VersionSpecRangesError::NonNumeralPrefixSegment`]).
    #[error(
        "the last prefix segment is not a single numeral, so no successor bound exists (e.g. `1.1a.*`)"
    )]
    NonNumeralLastPrefixSegment,

    /// The prefix ends in a zero segment, so the matching set is not closed
    /// under `Ord`-equality: `2024a` matches `2024.0.*` while the
    /// equal-sorting `2024a.0` does not.
    #[error(
        "a prefix with a trailing zero segment is not closed under ordering-equality (`2024a` matches `2024.0.*`, the equal-sorting `2024a.0` does not)"
    )]
    TrailingZeroPrefix,
}

impl VersionSpec {
    /// Converts this spec into the equivalent [`Ranges`] interval set over
    /// [`Version`], or an error when the spec's matching set is not exactly
    /// representable as an interval set under [`Version`]'s ordering (see
    /// the module documentation).
    ///
    /// The conversion is exact: the returned ranges contain precisely the
    /// versions that [`VersionSpec::matches`] accepts, so set operations on
    /// the result (intersection, union, complement, subset and disjointness
    /// tests) are sound with respect to the matching semantics.
    ///
    /// # Known blind spot (documented, deliberate)
    ///
    /// Versions that version themselves below their own `dev` floor, i.e.
    /// nest a second pre-release marker below a `dev` marker (`1.2dev0dev`,
    /// `1.2dev.rc1`), are misclassified by the prefix operators' `dev` floor
    /// bounds, and provably no interval representation can classify them
    /// correctly: the true boundary of a prefix's matching set is an
    /// unattainable limit (the prefix extended by an unbounded descending
    /// tower of `dev` components). No real-world package versions itself
    /// below its own `dev` pre-release, so the practical impact is nil; the
    /// agreement property test's version grammar documents and enforces
    /// exactly this exclusion.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::str::FromStr;
    ///
    /// use rattler_conda_types::{ParseStrictness, Version, VersionSpec, VersionSpecRangesError};
    ///
    /// let spec = VersionSpec::from_str(">=2.17,<3", ParseStrictness::Strict)?;
    /// let ranges = spec.to_ranges()?;
    /// assert!(ranges.contains(&Version::from_str("2.28")?));
    /// assert!(!ranges.contains(&Version::from_str("3.0")?));
    ///
    /// // Range algebra: `1.1.*` is a subset of `>=1,<2`.
    /// let prefix = VersionSpec::from_str("1.1.*", ParseStrictness::Strict)?.to_ranges()?;
    /// let bounds = VersionSpec::from_str(">=1,<2", ParseStrictness::Strict)?.to_ranges()?;
    /// assert!(prefix.subset_of(&bounds));
    ///
    /// // Specs whose matching set has no interval form are refused.
    /// let local = VersionSpec::from_str("=1.0+abc", ParseStrictness::Lenient)?;
    /// assert_eq!(
    ///     local.to_ranges(),
    ///     Err(VersionSpecRangesError::LocalVersionPrefix)
    /// );
    /// # Ok::<_, Box<dyn std::error::Error>>(())
    /// ```
    pub fn to_ranges(&self) -> Result<Ranges<Version>, VersionSpecRangesError> {
        Ok(match self {
            VersionSpec::None => Ranges::empty(),
            VersionSpec::Any => Ranges::full(),
            // The comparison operators evaluate through `Version`'s `Ord`,
            // which is exactly the order `Ranges` uses; these are exact by
            // definition.
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
                    result = result.intersection(&sub.to_ranges()?);
                }
                result
            }
            VersionSpec::Group(LogicalOperator::Or, group) => {
                let mut result = Ranges::empty();
                for sub in group {
                    result = result.union(&sub.to_ranges()?);
                }
                result
            }
        })
    }

    /// Converts this spec into a SUPERSET of its matching set: every
    /// matching version is contained in the returned ranges, which may also
    /// contain non-matching versions. Returns `None` when no useful envelope
    /// exists.
    ///
    /// The envelope is only sound for proving DISJOINTNESS (disjoint
    /// supersets imply disjoint sets); it must never feed subset or equality
    /// reasoning. Where [`VersionSpec::to_ranges`] succeeds the envelope is
    /// the exact set; otherwise:
    ///
    /// - `prefix.*` (starts-with): every matching version sorts strictly
    ///   below the `dev` floor of the prefix bumped at its deepest non-zero
    ///   pure-numeral segment (trailing zero and non-numeral segments are
    ///   dropped first), or below the next epoch when no segment qualifies.
    ///   Identifier segments cannot be bumped (`Version::bump` rewrites
    ///   identifiers to `a`, so `10.rc2` bumps to `10.a3`, which sorts
    ///   BELOW `10.rc2`) and trailing zero segments are skippable in the
    ///   matching semantics (`1post` matches `1.0.*` yet sorts above
    ///   `1.1dev`); the envelope superset property test found both corners.
    ///   Leaving the lower side unbounded also swallows the
    ///   prefix-extension dev corner.
    /// - `~=limit` (compatible): `v >= limit` is part of the operator's
    ///   exact semantics, and the same bumped-prefix upper bound applies
    ///   when it is constructible.
    /// - negated starts-with and compatible: no envelope (that would require
    ///   an under-approximation to complement).
    /// - `and` groups: intersection, skipping branches without an envelope
    ///   (dropping a conjunct only enlarges the set).
    /// - `or` groups: union; no envelope if any branch lacks one.
    ///
    /// # Examples
    ///
    /// ```
    /// use rattler_conda_types::{ParseStrictness, VersionSpec};
    ///
    /// // `11.0.*` has no exact interval form (trailing zero prefix), but
    /// // every version it matches sorts below `11.1dev`, so its envelope
    /// // proves it disjoint from `>=12`.
    /// let envelope = VersionSpec::from_str("11.0.*", ParseStrictness::Strict)?
    ///     .to_ranges_envelope()
    ///     .unwrap();
    /// let other = VersionSpec::from_str(">=12", ParseStrictness::Strict)?.to_ranges()?;
    /// assert!(envelope.is_disjoint(&other));
    /// # Ok::<_, Box<dyn std::error::Error>>(())
    /// ```
    pub fn to_ranges_envelope(&self) -> Option<Ranges<Version>> {
        if let Ok(exact) = self.to_ranges() {
            return Some(exact);
        }
        Some(match self {
            VersionSpec::StrictRange(StrictRangeOperator::StartsWith, prefix) => {
                Ranges::strictly_lower_than(starts_with_envelope_bound(&prefix.0)?)
            }
            VersionSpec::StrictRange(StrictRangeOperator::Compatible, limit) => {
                let lower = Ranges::higher_than(limit.0.clone());
                match compatible_upper_bound(&limit.0) {
                    Some(high) => lower.intersection(&Ranges::strictly_lower_than(high)),
                    None => lower,
                }
            }
            VersionSpec::Group(LogicalOperator::And, group) => {
                let mut result = Ranges::full();
                for sub in group {
                    if let Some(envelope) = sub.to_ranges_envelope() {
                        result = result.intersection(&envelope);
                    }
                }
                result
            }
            VersionSpec::Group(LogicalOperator::Or, group) => {
                let mut result = Ranges::empty();
                for sub in group {
                    result = result.union(&sub.to_ranges_envelope()?);
                }
                result
            }
            _ => return None,
        })
    }
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
/// The conversion succeeds only when all of the following hold; each
/// exclusion is a genuine non-representability documented on the returned
/// error variant:
///
/// - **no local part**: a local prefix selects on the local component, which
///   is not contiguous in the version order.
/// - **every non-last segment ends in a numeral**: an identifier-ending
///   segment breaks closure under `Ord`-equality.
/// - **the last segment is a single numeral that is only zero when it is the
///   sole segment**: a trailing zero segment re-creates the closure problem
///   with realistic versions, and an identifier-ending last segment has no
///   constructible successor bound.
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
fn starts_with_ranges(prefix: &Version) -> Result<Ranges<Version>, VersionSpecRangesError> {
    prefix_representability(prefix)?;
    let low = dev_floor(prefix)
        .expect("a representable prefix ends in a numeral, so its dev floor is a valid version");
    let bumped = prefix
        .bump(VersionBumpType::Last)
        .expect("a representable prefix ends in a numeral, which can always be bumped");
    let high = dev_floor(&bumped)
        .expect("a bumped representable prefix ends in a numeral, so its dev floor is valid");
    Ok(Ranges::between(low, high))
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
fn compatible_ranges(limit: &Version) -> Result<Ranges<Version>, VersionSpecRangesError> {
    if limit.has_local() {
        return Err(VersionSpecRangesError::LocalVersionPrefix);
    }
    let high = if limit.segment_count() == 1 {
        next_epoch_floor(limit)
    } else {
        let prefix = limit
            .pop_segments(1)
            .expect("a multi-segment version can pop one segment");
        prefix_representability(&prefix)?;
        let bumped = prefix
            .bump(VersionBumpType::Last)
            .expect("a representable prefix ends in a numeral, which can always be bumped");
        dev_floor(&bumped)
            .expect("a bumped representable prefix ends in a numeral, so its dev floor is valid")
    };
    Ok(Ranges::higher_than(limit.clone()).intersection(&Ranges::strictly_lower_than(high)))
}

/// The upper bound of the envelope for the compatible operator (see
/// [`VersionSpec::to_ranges_envelope`]): the starts-with envelope bound of
/// the all-but-last-segment prefix, or the next epoch's dev floor for
/// single-segment limits.
fn compatible_upper_bound(limit: &Version) -> Option<Version> {
    if limit.segment_count() == 1 {
        Some(next_epoch_floor(limit))
    } else {
        starts_with_envelope_bound(&limit.pop_segments(1)?)
    }
}

/// The upper bound of the envelope for the starts-with operator (see
/// [`VersionSpec::to_ranges_envelope`]): the dev floor of the prefix bumped
/// at its deepest non-zero pure-numeral segment, after dropping every
/// trailing segment that does not qualify; the dev floor of the next epoch
/// when no segment qualifies.
///
/// Two trailing shapes cannot be bumped where they stand, and both were
/// found by the envelope superset property test:
///
/// - identifier-bearing segments: [`Version::bump`] rewrites identifier
///   components to `a`, which can produce a SMALLER version (`10.rc2` bumps
///   to `10.a3`, and `a` sorts below `rc`), so a bound built from it would
///   cut matching versions out of the envelope.
/// - zero segments (unless the prefix is a single segment): the matching
///   semantics skip trailing zero prefix segments, which allows extra
///   components one level up (`1post` matches `1.0.*`), and those sort
///   above the zero segment's bumped dev floor (`1post > 1.1dev`).
///
/// Dropping trailing segments is sound because a version matching the
/// original prefix agrees componentwise with every shorter prefix up to the
/// shorter prefix's last segment, where it can only EXTEND the segment's
/// numeral, so it still sorts strictly below that numeral's bumped dev
/// floor. The epoch fallback is sound because matching versions share the
/// prefix's epoch.
fn starts_with_envelope_bound(prefix: &Version) -> Option<Version> {
    let mut prefix = prefix.strip_local().into_owned();
    loop {
        // Note `segments().last()` rather than `next_back()`: the
        // iterator returned by `Version::segments` computes component
        // offsets in a stateful forward pass, so consuming it from the back
        // yields segments with wrong offsets.
        let last_segment = prefix
            .segments()
            .last()
            .expect("a version has at least one segment");
        let mut components = last_segment.components();
        let pure_numeral =
            matches!(components.next(), Some(Component::Numeral(_))) && components.next().is_none();
        let bumpable = pure_numeral && (!last_segment.is_zero() || prefix.segment_count() == 1);
        drop(components);
        if bumpable {
            let bumped = prefix
                .bump(VersionBumpType::Last)
                .expect("bumping a pure-numeral segment cannot fail");
            return dev_floor(&bumped);
        }
        match prefix.pop_segments(1) {
            Some(shorter) => prefix = shorter,
            None => return Some(next_epoch_floor(&prefix)),
        }
    }
}

/// Checks that a `starts_with` prefix has the supported shape described in
/// [`starts_with_ranges`]: no local part, every non-last segment ends in a
/// numeral component, and the last segment is a single numeral that is only
/// zero when it is the sole segment.
fn prefix_representability(prefix: &Version) -> Result<(), VersionSpecRangesError> {
    if prefix.has_local() {
        return Err(VersionSpecRangesError::LocalVersionPrefix);
    }
    let segment_count = prefix.segments().len();
    for (index, segment) in prefix.segments().enumerate() {
        let is_last = index + 1 == segment_count;
        if is_last {
            let mut components = segment.components();
            let single = components.next();
            if components.next().is_some() {
                return Err(VersionSpecRangesError::NonNumeralLastPrefixSegment);
            }
            match single {
                Some(Component::Numeral(_)) => {}
                _ => return Err(VersionSpecRangesError::NonNumeralLastPrefixSegment),
            }
            if segment_count > 1 && segment.is_zero() {
                return Err(VersionSpecRangesError::TrailingZeroPrefix);
            }
        } else {
            match segment.components().next_back() {
                Some(Component::Numeral(_)) => {}
                _ => return Err(VersionSpecRangesError::NonNumeralPrefixSegment),
            }
        }
    }
    Ok(())
}

/// Returns the `dev` pre-release floor of a version: the version with a
/// `dev` component appended to its last segment (`1.2 -> 1.2dev`). The
/// floor is only meaningful for versions whose last segment ends in a
/// numeral, which the exact conversion guarantees through
/// [`prefix_representability`]; the envelope also applies it to other
/// shapes, where the result still sorts above every version matching the
/// corresponding prefix (all that an upper bound needs).
fn dev_floor(version: &Version) -> Option<Version> {
    Version::from_str(&format!("{version}dev")).ok()
}

/// Returns the `dev` floor of the next epoch (`2 -> 1!0dev`), the upper
/// bound of a single-segment compatible operator: with an empty
/// all-but-last-segment prefix only epoch equality remains, and every
/// version of the next epoch sorts at or above this floor.
fn next_epoch_floor(limit: &Version) -> Version {
    Version::from_str(&format!("{}!0dev", limit.epoch() + 1))
        .expect("an epoch followed by a `0dev` segment is a valid version")
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::{ParseStrictness, StrictVersion};

    fn spec(s: &str) -> VersionSpec {
        VersionSpec::from_str(s, ParseStrictness::Lenient).unwrap()
    }

    fn version(s: &str) -> Version {
        Version::from_str(s).unwrap()
    }

    /// Asserts that the spec converts and that membership of each version
    /// agrees with `VersionSpec::matches` (the matching semantics are also
    /// checked, since the expectation lists are hand-written from them).
    fn assert_members(spec_str: &str, contained: &[&str], excluded: &[&str]) {
        let spec = spec(spec_str);
        let ranges = spec
            .to_ranges()
            .unwrap_or_else(|err| panic!("`{spec_str}` should be representable: {err}"));
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
        let any = VersionSpec::Any.to_ranges().unwrap();
        assert!(any.contains(&version("1.2.3")));
        let none = VersionSpec::None.to_ranges().unwrap();
        assert!(!none.contains(&version("1.2.3")));
    }

    #[test]
    fn test_group_operators() {
        assert_members(
            ">=2.17,<3.0a0",
            &["2.17", "2.28"],
            &["2.16", "3.0a0", "3.0", "3.1"],
        );
        // Nested groups.
        assert_members(">=1.2,<2|>3.1", &["1.5", "3.2"], &["1.1", "2.5", "3.1"]);
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
        // Single-segment limit: only the epoch bounds remain. `~=1` reaches
        // up to but not into the next epoch: `1999` is compatible, `1!1999`
        // is not (the upper bound is the next epoch's dev floor, `1!0dev`).
        assert_members("~=2", &["2", "3", "99.5"], &["1.9", "1!1", "1!3"]);
        assert_members("~=1", &["1", "1999", "99.5"], &["0.9", "1dev", "1!1999"]);
    }

    #[test]
    fn test_compatible_unrepresentable() {
        // Compatible inherits the prefix shape rules for its
        // all-but-last-segment prefix: the prefix of `~=1.0.3` is `1.0`,
        // which ends in a zero segment (`1post` matches `~=1.0.3` while the
        // `Ord`-equal `1post.0` does not).
        assert_eq!(
            spec("~=1.0.3").to_ranges(),
            Err(VersionSpecRangesError::TrailingZeroPrefix)
        );
        // A local part in the limit is refused.
        assert_eq!(
            spec("~=1.2+abc").to_ranges(),
            Err(VersionSpecRangesError::LocalVersionPrefix)
        );
    }

    #[test]
    fn test_starts_with_membership() {
        // rattler's `starts_with` allows arbitrary extra components within
        // the last prefix segment, so `1.1a0` and `1.1dev` both match
        // `1.1.*` (conda/rattler#1914 semantics). Note `1.1a0` and `1.1dev`
        // sort BELOW `1.1`: a naive `>=1.1` lower bound would exclude them,
        // which is why the conversion uses the `dev` floor `1.1dev`.
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
    fn test_group_with_prefix_operators() {
        assert_members("11.*|12.*", &["11.1", "12.9"], &["10.9", "13.0"]);
        // An unrepresentable member poisons the whole group.
        assert_eq!(
            spec(">=1.2,1.0.*").to_ranges(),
            Err(VersionSpecRangesError::TrailingZeroPrefix)
        );
        // ... but representable groups stay representable.
        assert!(spec(">=1.2,<2").to_ranges().is_ok());
    }

    #[test]
    fn test_unrepresentable_prefixes() {
        // A trailing zero segment makes the matching set inconsistent under
        // `Ord`-equality: `2024a` matches `2024.0.*` while the Ord-equal
        // `2024a.0` does not. Both directions are realistic versions, so no
        // interval representation exists and the conversion must refuse.
        assert_eq!(
            spec("1.0.*").to_ranges(),
            Err(VersionSpecRangesError::TrailingZeroPrefix)
        );
        assert_eq!(
            spec("2.38.0.*").to_ranges(),
            Err(VersionSpecRangesError::TrailingZeroPrefix)
        );
        // An identifier-ending last segment has no constructible successor
        // bound.
        assert_eq!(
            VersionSpec::StrictRange(
                StrictRangeOperator::StartsWith,
                StrictVersion(version("1.1a")),
            )
            .to_ranges(),
            Err(VersionSpecRangesError::NonNumeralLastPrefixSegment)
        );
        // Local parts in the prefix select on the local component, which is
        // not contiguous in the version order.
        assert_eq!(
            VersionSpec::StrictRange(
                StrictRangeOperator::StartsWith,
                StrictVersion(version("1.1+x")),
            )
            .to_ranges(),
            Err(VersionSpecRangesError::LocalVersionPrefix)
        );
    }

    /// Permanent record of the counterexamples that shaped the conversion
    /// (from the Zulip discussion linked in the pull request and from the
    /// reference implementation). Each block asserts the matching semantics
    /// that make the case hard AND the conversion's answer to it.
    #[test]
    fn test_named_counterexamples() {
        // `=2.0` matches `2.0a`, which sorts BEFORE `2.0`: a naive `>=2.0`
        // lower bound would be wrong (the settled answer is the dev floor,
        // see `test_starts_with_membership`). `2.0.*` itself is refused
        // outright because of its trailing zero segment (next block).
        let two_zero = spec("=2.0");
        assert!(two_zero.matches(&version("2.0a")));
        assert!(version("2.0a") < version("2.0"));
        assert_eq!(
            two_zero.to_ranges(),
            Err(VersionSpecRangesError::TrailingZeroPrefix)
        );

        // The dev floor is a floor, not an alpha floor: `1.2dev` matches
        // `1.2.*` yet sorts below `1.2a0`, so an `a0`-based lower bound
        // (an earlier attempt from the Zulip thread) would exclude it.
        assert!(spec("1.2.*").matches(&version("1.2dev")));
        assert!(version("1.2dev") < version("1.2a0"));

        // Trailing-zero prefixes break `Ord`-equality closure: `2024a`
        // matches `2024.0.*` while the equal-sorting `2024a.0` does not, so
        // no interval can represent the matching set.
        let trailing = spec("2024.0.*");
        assert!(trailing.matches(&version("2024a")));
        assert!(!trailing.matches(&version("2024a.0")));
        assert_eq!(version("2024a").cmp(&version("2024a.0")), Ordering::Equal);
        assert_eq!(
            trailing.to_ranges(),
            Err(VersionSpecRangesError::TrailingZeroPrefix)
        );

        // Identifier-ending inner segments break the same closure: `1.1a.5`
        // matches `1.1a.5.*` while the equal-sorting `1.1a0.5` does not.
        let inner = spec("1.1a.5.*");
        assert!(inner.matches(&version("1.1a.5")));
        assert!(!inner.matches(&version("1.1a0.5")));
        assert_eq!(version("1.1a.5").cmp(&version("1.1a0.5")), Ordering::Equal);
        assert_eq!(
            inner.to_ranges(),
            Err(VersionSpecRangesError::NonNumeralPrefixSegment)
        );

        // Local prefixes select non-contiguously: `=1.2+abc` matches
        // `1.2+abc1`, `1.2.1+abc`, `1.2.2+abc` and `1.2.3+abc`, but not
        // `1.2.2+bbc`, which sorts between the last two.
        let local = spec("=1.2+abc");
        for v in ["1.2+abc1", "1.2.1+abc", "1.2.2+abc", "1.2.3+abc"] {
            assert!(local.matches(&version(v)), "`=1.2+abc` should match `{v}`");
        }
        assert!(!local.matches(&version("1.2.2+bbc")));
        assert!(version("1.2.2+abc") < version("1.2.2+bbc"));
        assert!(version("1.2.2+bbc") < version("1.2.3+abc"));
        assert_eq!(
            local.to_ranges(),
            Err(VersionSpecRangesError::LocalVersionPrefix)
        );
    }

    #[test]
    fn test_envelope_disjointness() {
        // Where the exact conversion succeeds, the envelope is the exact
        // set.
        assert_eq!(
            spec(">=1.2,<2").to_ranges_envelope().unwrap(),
            spec(">=1.2,<2").to_ranges().unwrap()
        );
        // `11.0.*` has no exact interval form (trailing-zero prefix), but
        // every matching version sorts below `11.1dev`, which is disjoint
        // from `>=12.0`.
        let envelope = spec("11.0.*").to_ranges_envelope().unwrap();
        assert!(envelope.is_disjoint(&spec(">=12.0").to_ranges().unwrap()));
        // Overlapping envelopes prove nothing: `>=11` overlaps it.
        assert!(!envelope.is_disjoint(&spec(">=11").to_ranges().unwrap()));
        // An OR group with an unrepresentable branch still envelopes.
        let group = spec("11.0.*|10.2.*").to_ranges_envelope().unwrap();
        assert!(group.is_disjoint(&spec(">=12.0").to_ranges().unwrap()));
        // Compatible keeps its exact lower bound and the bumped-prefix upper
        // bound even when the exact conversion refuses.
        let compat = spec("~=1.0.3").to_ranges_envelope().unwrap();
        assert!(compat.is_disjoint(&spec(">=2").to_ranges().unwrap()));
        assert!(compat.is_disjoint(&spec("<1.0.3").to_ranges().unwrap()));
        // `Version::bump` rewrites identifiers to `a`, so bumping `10.rc2`
        // yields `10.a3`, which sorts BELOW the prefix. The envelope bound
        // must therefore come from the deepest pure-numeral segment (`10`,
        // giving `11dev`) for the superset property to hold.
        let ident = VersionSpec::StrictRange(
            StrictRangeOperator::StartsWith,
            StrictVersion(version("10.rc2")),
        );
        assert!(version("10.a3") < version("10.rc2"));
        let envelope = ident.to_ranges_envelope().unwrap();
        assert!(envelope.contains(&version("10.rc2")));
        assert!(envelope.contains(&version("10.rc2.5")));
        assert!(envelope.is_disjoint(&spec(">=11").to_ranges().unwrap()));
        // Trailing zero prefix segments are skippable in the matching
        // semantics, so `1post` matches `1.0.*` yet sorts above `1.1dev`:
        // the bound must come from the last non-zero segment (`1`, giving
        // `2dev`).
        let envelope = spec("1.0.*").to_ranges_envelope().unwrap();
        assert!(envelope.contains(&version("1post")));
        assert!(envelope.is_disjoint(&spec(">=2").to_ranges().unwrap()));
        // Negated prefix operators have no envelope when not exactly
        // representable (that would require an under-approximation to
        // complement).
        let negated = VersionSpec::StrictRange(
            StrictRangeOperator::NotStartsWith,
            StrictVersion(version("1.0")),
        );
        assert!(negated.to_ranges_envelope().is_none());
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

    /// Whether the spec contains a prefix (strict-range) operator, the
    /// interesting case for boundary coverage.
    fn has_strict_operator(spec: &VersionSpec) -> bool {
        match spec {
            VersionSpec::StrictRange(..) => true,
            VersionSpec::Group(_, group) => group.iter().any(has_strict_operator),
            _ => false,
        }
    }

    /// The hand-picked boundary probes for the property tests, augmented
    /// with a pool of generated versions.
    fn version_pool(rng: &mut Rng, generated: usize) -> Vec<Version> {
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
        for _ in 0..generated {
            versions.push(gen_version(rng));
        }
        versions
    }

    /// For every generated `(spec, version)` pair the converted ranges must
    /// agree exactly with `VersionSpec::matches`. Any disagreement is a bug
    /// in the conversion; the conversion is fixed, never the test.
    #[test]
    fn test_agreement_property() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let versions = version_pool(&mut rng, 70);
        let specs: Vec<VersionSpec> = (0..600).map(|_| gen_spec(&mut rng, &versions, 2)).collect();

        let mut convertible = 0usize;
        let mut convertible_strict = 0usize;
        let mut checks = 0usize;
        for spec in &specs {
            let Ok(ranges) = spec.to_ranges() else {
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

    /// The envelope must be a SUPERSET of the matching set for every spec
    /// that has one: that containment is what makes disjointness proofs on
    /// envelopes sound. (The reverse direction deliberately does not hold.)
    #[test]
    fn test_envelope_superset_property() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let versions = version_pool(&mut rng, 70);
        let specs: Vec<VersionSpec> = (0..600).map(|_| gen_spec(&mut rng, &versions, 2)).collect();

        let mut enveloped = 0usize;
        for spec in &specs {
            let Some(envelope) = spec.to_ranges_envelope() else {
                continue;
            };
            enveloped += 1;
            for v in &versions {
                if spec.matches(v) {
                    assert!(
                        envelope.contains(v),
                        "envelope of `{spec}` should contain matching version `{v}`",
                    );
                }
            }
        }
        assert!(
            enveloped * 100 >= specs.len() * 60,
            "only {enveloped} of {} specs have an envelope",
            specs.len(),
        );
    }
}
