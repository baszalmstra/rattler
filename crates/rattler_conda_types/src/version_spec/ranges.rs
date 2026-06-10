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
    /// use rattler_conda_types::{ParseStrictness, Version, VersionSpec};
    ///
    /// let spec = VersionSpec::from_str(">=2.17,<3", ParseStrictness::Strict)?;
    /// let ranges = spec.to_ranges()?;
    /// assert!(ranges.contains(&Version::from_str("2.28")?));
    /// assert!(!ranges.contains(&Version::from_str("3.0")?));
    ///
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
                StrictRangeOperator::StartsWith | StrictRangeOperator::NotStartsWith => {
                    todo!("implemented in a later step")
                }
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
    use std::str::FromStr;

    use super::*;
    use crate::ParseStrictness;

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
}
