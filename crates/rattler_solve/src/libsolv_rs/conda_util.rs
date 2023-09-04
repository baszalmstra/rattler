use crate::libsolv_rs::SolverMatchSpec;
use rattler_conda_types::Version;
use rattler_libsolv_rs::{Mapping, Pool, SolvableId, VersionSetId};
use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Returns the order of two candidates based on the order used by conda.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare_candidates(
    a: SolvableId,
    b: SolvableId,
    pool: &Pool<SolverMatchSpec>,
    match_spec_to_candidates: &Mapping<VersionSetId, OnceCell<Vec<SolvableId>>>,
    match_spec_highest_version: &Mapping<VersionSetId, OnceCell<Option<(Version, bool)>>>,
) -> Ordering {
    let a_solvable = pool.resolve_solvable(a);
    let b_solvable = pool.resolve_solvable(b);

    let a_record = &a_solvable.record();
    let b_record = &b_solvable.record();

    // First compare by "tracked_features". If one of the packages has a tracked feature it is
    // sorted below the one that doesn't have the tracked feature.
    let a_has_tracked_features = !a_record.track_features.is_empty();
    let b_has_tracked_features = !b_record.track_features.is_empty();
    match a_has_tracked_features.cmp(&b_has_tracked_features) {
        Ordering::Less => return Ordering::Less,
        Ordering::Greater => return Ordering::Greater,
        Ordering::Equal => {}
    };

    // Otherwise, select the variant with the highest version
    match a_record.version.cmp(&b_record.version) {
        Ordering::Less => return Ordering::Greater,
        Ordering::Greater => return Ordering::Less,
        Ordering::Equal => {}
    };

    // Otherwise, select the variant with the highest build number
    match a_record.build_number.cmp(&b_record.build_number) {
        Ordering::Less => return Ordering::Greater,
        Ordering::Greater => return Ordering::Less,
        Ordering::Equal => {}
    };

    // Otherwise, compare the dependencies of the variants. If there are similar
    // dependencies select the variant that selects the highest version of the dependency.
    let a_match_specs = a_solvable
        .dependencies()
        .iter()
        .map(|id| (*id, pool.resolve_version_set(*id)));
    let b_match_specs = b_solvable
        .dependencies()
        .iter()
        .map(|id| (*id, pool.resolve_version_set(*id)));

    let b_specs_by_name: HashMap<_, _> = b_match_specs
        .filter_map(|(spec_id, spec)| spec.name.as_ref().map(|name| (name, (spec_id))))
        .collect();

    let a_specs_by_name = a_match_specs
        .filter_map(|(spec_id, spec)| spec.name.as_ref().map(|name| (name, (spec_id))));

    let mut total_score = 0;
    for (a_dep_name, a_spec_id) in a_specs_by_name {
        if let Some(b_spec_id) = b_specs_by_name.get(&a_dep_name) {
            if &a_spec_id == b_spec_id {
                continue;
            }

            // Find which of the two specs selects the highest version
            let highest_a = find_highest_version(
                a_spec_id,
                pool,
                match_spec_to_candidates,
                match_spec_highest_version,
            );
            let highest_b = find_highest_version(
                *b_spec_id,
                pool,
                match_spec_to_candidates,
                match_spec_highest_version,
            );

            // Skip version if no package is selected by either spec
            let (a_version, a_tracked_features, b_version, b_tracked_features) = if let (
                Some((a_version, a_tracked_features)),
                Some((b_version, b_tracked_features)),
            ) =
                (highest_a, highest_b)
            {
                (a_version, a_tracked_features, b_version, b_tracked_features)
            } else {
                continue;
            };

            // If one of the dependencies only selects versions with tracked features, down-
            // weigh that variant.
            if let Some(score) = match a_tracked_features.cmp(&b_tracked_features) {
                Ordering::Less => Some(-100),
                Ordering::Greater => Some(100),
                Ordering::Equal => None,
            } {
                total_score += score;
                continue;
            }

            // Otherwise, down-weigh the version with the lowest selected version.
            total_score += match a_version.cmp(&b_version) {
                Ordering::Less => 1,
                Ordering::Equal => 0,
                Ordering::Greater => -1,
            };
        }
    }

    // If ranking the dependencies provides a score, use that for the sorting.
    match total_score.cmp(&0) {
        Ordering::Equal => {}
        ord => return ord,
    };

    // Otherwise, order by timestamp
    b_record.timestamp.cmp(&a_record.timestamp)
}

pub(crate) fn find_highest_version(
    match_spec_id: VersionSetId,
    pool: &Pool<SolverMatchSpec>,
    match_spec_to_candidates: &Mapping<VersionSetId, OnceCell<Vec<SolvableId>>>,
    match_spec_highest_version: &Mapping<VersionSetId, OnceCell<Option<(Version, bool)>>>,
) -> Option<(Version, bool)> {
    match_spec_highest_version[match_spec_id]
        .get_or_init(|| {
            let candidates = match_spec_to_candidates[match_spec_id]
                .get_or_init(|| pool.find_matching_solvables(match_spec_id));

            candidates
                .iter()
                .map(|id| pool.resolve_solvable(*id).record())
                .fold(None, |init, record| {
                    Some(init.map_or_else(
                        || {
                            (
                                record.version.version().clone(),
                                !record.track_features.is_empty(),
                            )
                        },
                        |(version, has_tracked_features)| {
                            (
                                version.max(record.version.version().clone()),
                                has_tracked_features && record.track_features.is_empty(),
                            )
                        },
                    ))
                })
        })
        .as_ref()
        .map(|(version, has_tracked_features)| (version.clone(), *has_tracked_features))
}