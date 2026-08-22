use std::{
    cmp::Ordering,
    collections::{HashMap, hash_map::Entry},
};

use futures::future::FutureExt;
use itertools::Itertools;
use rattler_conda_types::Version;
use resolvo::{
    Dependencies, NameId, Requirement, SolvableId, SolverCache, VersionSetId, utils::Pool,
};

use super::{NameType, SolverMatchSpec, SolverPackageRecord};
use crate::{CancellationToken, ChannelPriority, resolvo::CondaDependencyProvider};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum CompareStrategy {
    Default,
    LowestVersion,
}

/// Sort the candidates based on the dependencies.
/// This sorts in two steps:
/// 1. Sort by tracked features, version, and build number
/// 2. Sort by trying to sort the solvable that selects the highest versions of
///    the shared set of dependencies
pub struct SolvableSorter<'a, 'repo> {
    solver: &'a SolverCache<CondaDependencyProvider<'repo>>,
    strategy: CompareStrategy,
    dependency_strategy: CompareStrategy,
}

impl<'a, 'repo> SolvableSorter<'a, 'repo> {
    pub fn new(
        solver: &'a SolverCache<CondaDependencyProvider<'repo>>,
        strategy: CompareStrategy,
        dependency_strategy: CompareStrategy,
    ) -> Self {
        Self {
            solver,
            strategy,
            dependency_strategy,
        }
    }

    /// Get a reference to the solvable record.
    fn solvable_record(&self, id: SolvableId) -> &SolverPackageRecord<'repo> {
        let pool = self.pool();
        let solvable = pool.resolve_solvable(id);

        &solvable.record
    }

    /// Returns the channel-priority rank of a solvable. A lower rank indicates a
    /// higher-priority channel. Records without a channel (and virtual packages
    /// / extras) rank last.
    fn channel_rank(&self, id: SolvableId) -> u32 {
        match self.solvable_record(id) {
            SolverPackageRecord::Record(rec) => self.solver.provider().channel_rank(&rec.channel),
            SolverPackageRecord::Extra { .. } | SolverPackageRecord::VirtualPackage(..) => u32::MAX,
        }
    }

    /// Reference to the pool
    fn pool(&self) -> &Pool<SolverMatchSpec<'repo>, NameType> {
        &self.solver.provider().pool
    }

    /// Sort the candidates based on the dependencies.
    /// This sorts in two steps:
    /// 1. Sort by tracked features, version, and build number
    /// 2. Sort by trying to find the candidate that selects the highest
    ///    versions of the shared set of dependencies
    pub fn sort(
        self,
        solvables: &mut [SolvableId],
        version_cache: &mut HashMap<VersionSetId, Option<(Version, bool)>>,
    ) {
        use super::sort_stats as st;
        st::add(&st::SORT_CALLS, 1);
        st::add(&st::SORTED_SOLVABLES, solvables.len() as u64);
        let t = std::time::Instant::now();
        self.sort_by_tracked_version_build(solvables);
        st::add(&st::STAGE1_NANOS, t.elapsed().as_nanos() as u64);
        let t = std::time::Instant::now();
        self.sort_by_highest_dependency_versions(solvables, version_cache);
        st::add(&st::STAGE2_NANOS, t.elapsed().as_nanos() as u64);
    }

    /// This function can be used for the initial sorting of the candidates.
    fn sort_by_tracked_version_build(&self, solvables: &mut [SolvableId]) {
        // The pool and solver cache are not `Sync`, so a parallel sort cannot
        // call `simple_compare` directly. Instead extract plain `Send + Sync`
        // sort keys sequentially and sort those in parallel. Only do this for
        // larger lists where the parallelism outweighs the extraction cost.
        const PAR_SORT_THRESHOLD: usize = 500;
        if solvables.len() < PAR_SORT_THRESHOLD {
            solvables.sort_by(|a, b| self.simple_compare(*a, *b));
            return;
        }

        struct SortKey<'r> {
            id: SolvableId,
            tracked: bool,
            channel_rank: u32,
            version: Option<&'r Version>,
            build_number: u64,
        }

        let flexible = self.solver.provider().channel_priority == ChannelPriority::Flexible;
        let strategy = self.strategy;
        let mut keyed: Vec<SortKey<'_>> = solvables
            .iter()
            .map(|&id| {
                let record = self.solvable_record(id);
                SortKey {
                    id,
                    tracked: !record.track_features().is_empty(),
                    channel_rank: if flexible { self.channel_rank(id) } else { 0 },
                    version: record.version(),
                    build_number: record.build_number(),
                }
            })
            .collect();

        use rayon::slice::ParallelSliceMut;
        keyed.par_sort_by(|a, b| {
            match (a.tracked, b.tracked) {
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                _ => {}
            }
            match a.channel_rank.cmp(&b.channel_rank) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
            match (strategy, a.version.cmp(&b.version)) {
                (CompareStrategy::Default, Ordering::Greater)
                | (CompareStrategy::LowestVersion, Ordering::Less) => return Ordering::Less,
                (CompareStrategy::Default, Ordering::Less)
                | (CompareStrategy::LowestVersion, Ordering::Greater) => return Ordering::Greater,
                (_, Ordering::Equal) => {}
            }
            b.build_number.cmp(&a.build_number)
        });

        for (slot, key) in solvables.iter_mut().zip(keyed) {
            *slot = key.id;
        }
    }

    /// Sort the candidates based on:
    /// 1. Whether the package has tracked features
    /// 2. The version of the package
    /// 3. The build number of the package
    fn simple_compare(&self, a: SolvableId, b: SolvableId) -> Ordering {
        let a_record = &self.solvable_record(a);
        let b_record = &self.solvable_record(b);

        // First compare by "tracked_features". If one of the packages has a tracked
        // feature it is sorted below the one that doesn't have the tracked feature.
        let a_has_tracked_features = !a_record.track_features().is_empty();
        let b_has_tracked_features = !b_record.track_features().is_empty();
        match (a_has_tracked_features, b_has_tracked_features) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        };

        // Under flexible channel priority, prefer candidates from
        // higher-priority channels (lower rank) before comparing versions. This
        // makes the solver exhaust a higher-priority channel's versions before
        // falling back to a lower-priority channel, while still leaving the
        // lower-priority channels available as a fallback.
        if self.solver.provider().channel_priority == ChannelPriority::Flexible {
            match self.channel_rank(a).cmp(&self.channel_rank(b)) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }

        // Otherwise, select the variant with the highest version
        match (self.strategy, a_record.version().cmp(&b_record.version())) {
            (CompareStrategy::Default, Ordering::Greater)
            | (CompareStrategy::LowestVersion, Ordering::Less) => return Ordering::Less,
            (CompareStrategy::Default, Ordering::Less)
            | (CompareStrategy::LowestVersion, Ordering::Greater) => return Ordering::Greater,
            (_, Ordering::Equal) => {}
        };

        // Otherwise, select the variant with the highest build number first
        b_record.build_number().cmp(&a_record.build_number())
    }

    fn sort_by_highest_dependency_versions(
        &self,
        solvables: &mut [SolvableId],
        version_cache: &mut HashMap<VersionSetId, Option<(Version, bool)>>,
    ) {
        // Because the list can contain multiple versions, tracked features, and builds
        // of the same package we need to create sub list of solvables that have
        // the same version, build, and tracked features and sort these sub
        // lists by the highest version of the dependencies shared by the solvables.
        let mut start = 0usize;
        let entire_len = solvables.len();
        while start < entire_len {
            let mut end = start + 1;

            // Find the range of solvables with the same: version, build, tracked features
            while end < entire_len
                && self.simple_compare(solvables[start], solvables[end]) == Ordering::Equal
            {
                end += 1;
            }

            // Take the sub list of solvables
            let sub = &mut solvables[start..end];
            if sub.len() > 1 {
                {
                    use super::sort_stats as st;
                    st::add(&st::TIEBREAK_RUNS, 1);
                    st::add(&st::TIEBREAK_RUN_SOLVABLES, sub.len() as u64);
                    st::max(&st::TIEBREAK_MAX_RUN, sub.len() as u64);
                }
                let cache_hit = {
                    let cache = self.solver.provider().dependency_tiebreak_cache.borrow();
                    if let Some(cached) = cache.entries.get(sub) {
                        sub.copy_from_slice(cached);
                        true
                    } else {
                        false
                    }
                };

                if !cache_hit {
                    let stored_candidate_ids = sub.len().saturating_mul(2);
                    let cache_key = {
                        let cache = self.solver.provider().dependency_tiebreak_cache.borrow();
                        (cache
                            .stored_candidate_ids
                            .saturating_add(stored_candidate_ids)
                            <= super::DEPENDENCY_TIEBREAK_CACHE_CANDIDATE_LIMIT)
                            .then(|| sub.to_vec())
                    };

                    // Sort the sub list of solvables by the highest version of the dependencies.
                    let completed =
                        self.sort_subset_by_highest_dependency_versions(sub, version_cache);

                    if completed && let Some(cache_key) = cache_key {
                        let mut cache = self
                            .solver
                            .provider()
                            .dependency_tiebreak_cache
                            .borrow_mut();
                        cache.stored_candidate_ids += stored_candidate_ids;
                        cache.entries.insert(cache_key, sub.to_vec());
                    }
                }
            }

            start = end;
        }
    }

    /// Sorts the solvables by the highest version of the dependencies shared by
    /// the solvables. what this function does is:
    /// 1. Find the first unsorted solvable in the list
    /// 2. Get the dependencies for each solvable
    /// 3. Get the known dependencies for each solvable, filter out the unknown
    ///    dependencies
    /// 4. Retain the dependencies that are shared by all the solvables
    /// 6. Calculate a total score by counting the position of the solvable in
    ///    the list with sorted dependencies
    /// 7. Sort by the score per solvable and use timestamp of the record as a
    ///    tie breaker
    fn sort_subset_by_highest_dependency_versions(
        &self,
        solvables: &mut [SolvableId],
        version_cache: &mut HashMap<VersionSetId, Option<(Version, bool)>>,
    ) -> bool {
        use super::sort_stats as st;
        let t_gather = std::time::Instant::now();
        // Get the dependencies for each solvable
        let dependencies = solvables
            .iter()
            .map(|id| {
                self.solver
                    .get_or_cache_dependencies(*id)
                    .now_or_never()
                    .expect("get_or_cache_dependencies failed")
                    .map(|deps| (id, deps))
            })
            .collect::<Result<Vec<_>, _>>();

        let dependencies = match dependencies {
            Ok(dependencies) => dependencies,
            // Solver cancellation, lets just return
            Err(_) => return false,
        };

        // Get the known dependencies for each solvable. Solvables with unknown
        // dependencies are moved to the end of the array (sorted lower).
        let mut id_and_deps: HashMap<_, Vec<_>> = HashMap::with_capacity(dependencies.len());
        let mut name_count: HashMap<NameId, usize> = HashMap::new();
        let mut known_count = solvables.len();
        let mut solvable_idx = 0;
        while solvable_idx < known_count {
            let solvable_id = solvables[solvable_idx];
            let dependencies = self
                .solver
                .get_or_cache_dependencies(solvable_id)
                .now_or_never()
                .expect("get_or_cache_dependencies failed");
            let known = match dependencies {
                Ok(Dependencies::Known(known_dependencies)) => known_dependencies,
                Ok(Dependencies::Unknown(_)) => {
                    // Swap to end and don't advance index - need to check swapped-in element
                    known_count -= 1;
                    solvables.swap(solvable_idx, known_count);
                    continue;
                }
                // Solver cancellation, lets just return
                Err(_) => return false,
            };

            for requirement in &known.requirements {
                let version_set_id = match &requirement.requirement {
                    // Ignore union requirements, these do not occur in the conda ecosystem
                    // currently
                    Requirement::Union(_) => {
                        unreachable!("Union requirements, are not implemented in the ordering")
                    }
                    Requirement::Single(version_set_id) => version_set_id,
                };

                // Get the name of the dependency and add the version set id to the list of
                // version sets for a particular package. A single solvable can depend on a
                // single package multiple times.
                let dependency_name = self
                    .pool()
                    .resolve_version_set_package_name(*version_set_id);

                // Check how often we have seen this dependency name
                let name_count = match name_count.entry(dependency_name) {
                    Entry::Occupied(entry) if entry.get() + 1 >= solvable_idx => entry.into_mut(),
                    Entry::Vacant(entry) if solvable_idx == 0 => entry.insert(0),
                    _ => {
                        // We have already not seen this dependency name for all solvables so there
                        // is no need to allocate additional memory to track
                        // it.
                        continue;
                    }
                };

                match id_and_deps.entry((solvable_id, dependency_name)) {
                    Entry::Occupied(mut entry) => entry.get_mut().push(*version_set_id),
                    Entry::Vacant(entry) => {
                        entry.insert(vec![*version_set_id]);
                        *name_count += 1;
                    }
                }
            }

            solvable_idx += 1;
        }

        // Only sort solvables with known dependencies (unknown deps are already at the end)
        let solvables = &mut solvables[..known_count];

        // Sort all the dependencies that the solvables have in common by their name.
        let sorted_unique_names = name_count
            .into_iter()
            .filter_map(|(name, count)| {
                if count == known_count {
                    Some(name)
                } else {
                    None
                }
            })
            .sorted_by_key(|name| self.pool().resolve_package_name(*name))
            .collect_vec();

        // The best version of a dependency that this solvable can end up with.
        //
        // A solvable can require the same package more than once: a recipe lists a bare
        // `nodejs` next to the `nodejs >=26.5.1,<27.0a0` pin from its run-export. Only
        // versions matching all of the requirements can be selected, so take the lowest
        // of their highest versions. Taking the highest would score every build by the
        // bare requirement, which matches everything and separates nothing.
        //
        // This is an approximation. It does not intersect the requirements, only takes
        // the lowest of their individual maxima. Enough to order candidates.
        let mut find_best_selectable_version = |version_set_ids: &Vec<VersionSetId>| {
            version_set_ids
                .iter()
                .filter_map(|id| find_highest_version(*id, self.solver, version_cache))
                .map(|v| TrackedFeatureVersion::new(v.0, v.1))
                .reduce(|a, b| {
                    // Better sorts first, so `Greater` means `a` is the more restrictive.
                    if a.compare_with_strategy(&b, CompareStrategy::Default) == Ordering::Greater {
                        a
                    } else {
                        b
                    }
                })
        };

        st::add(&st::DEP_GATHER_NANOS, t_gather.elapsed().as_nanos() as u64);
        let t_final = std::time::Instant::now();
        // Sort the solvables by comparing the highest version of the shared
        // dependencies in alphabetic order.
        solvables.sort_by(|a, b| {
            for &name in sorted_unique_names.iter() {
                let a_version = id_and_deps
                    .get(&(*a, name))
                    .and_then(&mut find_best_selectable_version);
                let b_version = id_and_deps
                    .get(&(*b, name))
                    .and_then(&mut find_best_selectable_version);

                // Deal with the case where resolving the version set doesn't actually select a
                // version
                let (a_version, b_version) = match (a_version, b_version) {
                    // If we have a version for either solvable, but not the other, the one with the
                    // version is better.
                    (Some(_), None) => return Ordering::Less,
                    (None, Some(_)) => return Ordering::Greater,

                    // If for neither solvable the version set doesn't select a version for the
                    // dependency we skip it.
                    (None, None) => continue,

                    (Some(a), Some(b)) => (a, b),
                };

                // Compare the versions
                match a_version.compare_with_strategy(&b_version, self.dependency_strategy) {
                    Ordering::Equal => {
                        // If this version is equal, we continue with the next
                        // dependency
                    }
                    ordering => return ordering,
                }
            }

            // Otherwise sort by timestamp (in reverse, we want the highest timestamp first)
            let a_record = self.solvable_record(*a);
            let b_record = self.solvable_record(*b);
            b_record.timestamp().cmp(&a_record.timestamp())
        });
        st::add(&st::FINAL_SORT_NANOS, t_final.elapsed().as_nanos() as u64);

        // Candidate matching reports cancellation as no matching version. Do not
        // retain the resulting partial/timestamp-biased ordering in that case.
        !self
            .solver
            .provider()
            .cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}

/// Couples the version with the tracked features, for easier ordering
#[derive(PartialEq, Eq, Clone, Debug)]
struct TrackedFeatureVersion {
    version: Version,
    tracked_features: bool,
}

impl TrackedFeatureVersion {
    fn new(version: Version, tracked_features: bool) -> Self {
        Self {
            version,
            tracked_features,
        }
    }

    fn compare_with_strategy(&self, other: &Self, compare_strategy: CompareStrategy) -> Ordering {
        // First compare by "tracked_features". If one of the packages has a tracked
        // feature it is sorted below the one that doesn't have the tracked feature.
        match (self.tracked_features, other.tracked_features) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ if compare_strategy == CompareStrategy::Default => other.version.cmp(&self.version),
            _ => self.version.cmp(&other.version),
        }
    }
}

pub(super) fn find_highest_version(
    match_spec_id: VersionSetId,
    solver: &SolverCache<CondaDependencyProvider<'_>>,
    highest_version_cache: &mut HashMap<VersionSetId, Option<(rattler_conda_types::Version, bool)>>,
) -> Option<(Version, bool)> {
    use super::sort_stats as st;
    st::add(&st::HIGHEST_VERSION_CALLS, 1);
    highest_version_cache
        .entry(match_spec_id)
        .or_insert_with(|| {
            st::add(&st::HIGHEST_VERSION_MISSES, 1);
            let t_miss = std::time::Instant::now();
            let candidates = solver
                .get_or_cache_matching_candidates(match_spec_id)
                .now_or_never()
                .expect("get_or_cache_matching_candidates failed");

            // Err only happens on cancellation, so we will not continue anyways
            let candidates = if let Ok(candidates) = candidates {
                candidates
            } else {
                return None;
            };

            let pool = &solver.provider().pool;

            let mut highest_version = None;
            for record in candidates
                .iter()
                .map(|id| &pool.resolve_solvable(*id).record)
            {
                let (version, has_tracked_features) = match record {
                    SolverPackageRecord::Record(record) => (
                        record.package_record.version.version(),
                        !record.package_record.track_features.is_empty(),
                    ),
                    SolverPackageRecord::VirtualPackage(record) => (&record.version, false),
                    SolverPackageRecord::Extra { .. } => continue,
                };
                highest_version = highest_version.map_or_else(
                    || Some((version.clone(), has_tracked_features)),
                    |(highest_version, current_has_tracked_features)| {
                        if version > &highest_version {
                            Some((version.clone(), has_tracked_features))
                        } else {
                            Some((highest_version, current_has_tracked_features))
                        }
                    },
                );
            }

            st::add(&st::HIGHEST_VERSION_MISS_NANOS, t_miss.elapsed().as_nanos() as u64);
            highest_version
        })
        .clone()
}
