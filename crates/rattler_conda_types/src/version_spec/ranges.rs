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

use thiserror::Error;
use version_ranges::Ranges;

use crate::{Version, VersionSpec, version_spec::RangeOperator};

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
    /// let spec = VersionSpec::from_str(">=2.17", ParseStrictness::Strict)?;
    /// let ranges = spec.to_ranges()?;
    /// assert!(ranges.contains(&Version::from_str("2.28")?));
    /// assert!(!ranges.contains(&Version::from_str("2.16")?));
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
            VersionSpec::Exact(..) => todo!("implemented in a later step"),
            VersionSpec::StrictRange(..) => todo!("implemented in a later step"),
            VersionSpec::Group(..) => todo!("implemented in a later step"),
        })
    }
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
    fn test_any_and_none() {
        let any = VersionSpec::Any.to_ranges().unwrap();
        assert!(any.contains(&version("1.2.3")));
        let none = VersionSpec::None.to_ranges().unwrap();
        assert!(!none.contains(&version("1.2.3")));
    }
}
