//! TEMPORARY: validates the assumptions behind a "per-name version index":
//! what fraction of dependency specs are pure (indexable) version constraints,
//! what candidate lists look like, what Version cmp/clone cost, and how a
//! binary-searched sorted-version index compares to the linear matching scan.
//! Not for merge.

use std::{
    collections::HashMap,
    hint::black_box,
    ops::Bound,
    path::Path,
    time::Instant,
};

use rattler_conda_types::{
    Channel, ChannelConfig, MatchSpec, Matches, NamelessMatchSpec, RepoDataRecord, Version,
    VersionSpec,
    version_spec::{EqualityOperator, LogicalOperator, RangeOperator},
};
use rattler_repodata_gateway::sparse::{PackageFormatSelection, SparseRepoData};

fn read_sparse_repodata(path: &str) -> SparseRepoData {
    SparseRepoData::from_file(
        Channel::from_str(
            "dummy",
            &ChannelConfig::default_with_root_dir(std::env::current_dir().unwrap()),
        )
        .unwrap(),
        "dummy".to_string(),
        path,
        None,
    )
    .unwrap()
}

/// Is this spec expressible as one contiguous version interval?
fn to_interval(spec: &VersionSpec) -> Option<(Bound<&Version>, Bound<&Version>)> {
    match spec {
        VersionSpec::Any => Some((Bound::Unbounded, Bound::Unbounded)),
        VersionSpec::Range(RangeOperator::GreaterEquals, v) => {
            Some((Bound::Included(v), Bound::Unbounded))
        }
        VersionSpec::Range(RangeOperator::Greater, v) => {
            Some((Bound::Excluded(v), Bound::Unbounded))
        }
        VersionSpec::Range(RangeOperator::Less, v) => {
            Some((Bound::Unbounded, Bound::Excluded(v)))
        }
        VersionSpec::Range(RangeOperator::LessEquals, v) => {
            Some((Bound::Unbounded, Bound::Included(v)))
        }
        VersionSpec::Exact(EqualityOperator::Equals, v) => {
            Some((Bound::Included(v), Bound::Included(v)))
        }
        VersionSpec::Group(LogicalOperator::And, children) => {
            let mut lo: Bound<&Version> = Bound::Unbounded;
            let mut hi: Bound<&Version> = Bound::Unbounded;
            for child in children {
                let (clo, chi) = to_interval(child)?;
                lo = tighter_lo(lo, clo);
                hi = tighter_hi(hi, chi);
            }
            Some((lo, hi))
        }
        _ => None,
    }
}

fn tighter_lo<'v>(a: Bound<&'v Version>, b: Bound<&'v Version>) -> Bound<&'v Version> {
    use Bound::*;
    match (a, b) {
        (Unbounded, x) | (x, Unbounded) => x,
        (Included(x), Included(y)) => Included(x.max(y)),
        (Excluded(x), Excluded(y)) => Excluded(x.max(y)),
        (Included(i), Excluded(e)) | (Excluded(e), Included(i)) => {
            if e >= i { Excluded(e) } else { Included(i) }
        }
    }
}

fn tighter_hi<'v>(a: Bound<&'v Version>, b: Bound<&'v Version>) -> Bound<&'v Version> {
    use Bound::*;
    match (a, b) {
        (Unbounded, x) | (x, Unbounded) => x,
        (Included(x), Included(y)) => Included(x.min(y)),
        (Excluded(x), Excluded(y)) => Excluded(x.min(y)),
        (Included(i), Excluded(e)) | (Excluded(e), Included(i)) => {
            if e <= i { Excluded(e) } else { Included(i) }
        }
    }
}

fn main() {
    let base = format!(
        "{}/../../test-data/channels/conda-forge",
        env!("CARGO_MANIFEST_DIR")
    );
    let sparse = vec![
        read_sparse_repodata(&format!("{base}/linux-64/repodata.json")),
        read_sparse_repodata(&format!("{base}/noarch/repodata.json")),
    ];

    for root in ["tensorflow", "quetz"] {
        let name = rattler_conda_types::PackageName::try_from(root).unwrap();
        let repodata = SparseRepoData::load_records_recursive(
            &sparse,
            [name],
            None,
            PackageFormatSelection::default(),
        )
        .unwrap();
        let records: Vec<&RepoDataRecord> = repodata.iter().flatten().collect();

        let mut by_name: HashMap<&str, Vec<&RepoDataRecord>> = HashMap::new();
        for r in &records {
            by_name
                .entry(r.package_record.name.as_normalized())
                .or_default()
                .push(r);
        }

        let mut unique: Vec<&str> = records
            .iter()
            .flat_map(|r| {
                r.package_record
                    .depends
                    .iter()
                    .chain(r.package_record.constrains.iter())
            })
            .map(String::as_str)
            .collect();
        unique.sort_unstable();
        unique.dedup();

        // --- census ---
        let mut bare = 0usize;
        let mut version_only = 0usize;
        let mut version_only_interval = 0usize;
        let mut version_build = 0usize;
        let mut other = 0usize;
        let mut parsed: Vec<(MatchSpec, NamelessMatchSpec)> = Vec::new();
        for s in &unique {
            let Ok(spec) = MatchSpec::from_str(s, rattler_conda_types::ParseStrictness::Lenient)
            else {
                continue;
            };
            let nameless = spec.clone().into_nameless().1;
            let has_other = nameless.file_name.is_some()
                || nameless.extras.is_some()
                || nameless.channel.is_some()
                || nameless.subdir.is_some()
                || nameless.namespace.is_some()
                || nameless.md5.is_some()
                || nameless.sha256.is_some()
                || nameless.url.is_some()
                || nameless.license.is_some()
                || nameless.condition.is_some()
                || nameless.track_features.is_some();
            let has_build = nameless.build.is_some() || nameless.build_number.is_some();
            match (&nameless.version, has_build, has_other) {
                (_, _, true) => other += 1,
                (None, false, false) => bare += 1,
                (None, true, false) | (Some(_), true, false) => version_build += 1,
                (Some(vs), false, false) => {
                    version_only += 1;
                    if to_interval(vs).is_some() {
                        version_only_interval += 1;
                    }
                }
            }
            parsed.push((spec, nameless));
        }
        let total = unique.len();

        // --- candidate list distribution ---
        let mut sizes: Vec<usize> = by_name.values().map(Vec::len).collect();
        sizes.sort_unstable();
        let max = sizes.last().copied().unwrap_or(0);
        let med = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
        let p95 = sizes.get(sizes.len() * 95 / 100).copied().unwrap_or(0);

        println!("==== {root}: {} records, {} names, {} unique specs ====", records.len(), by_name.len(), total);
        println!(
            "spec shapes: bare {} ({:.1}%), version-only {} ({:.1}%) of which single-interval {} ({:.1}% of version-only), version+build {} ({:.1}%), other-fields {} ({:.1}%)",
            bare, pct(bare, total),
            version_only, pct(version_only, total),
            version_only_interval, pct(version_only_interval, version_only),
            version_build, pct(version_build, total),
            other, pct(other, total),
        );
        println!("candidates per name: median {med}, p95 {p95}, max {max}");

        // --- microbench: Version cmp + clone ---
        let versions: Vec<&Version> = records
            .iter()
            .map(|r| r.package_record.version.version())
            .collect();
        let n = versions.len();

        let t = Instant::now();
        let mut acc = 0usize;
        for i in 0..n {
            let a = versions[i];
            let b = versions[(i * 7919 + 13) % n];
            if a.cmp(b) == std::cmp::Ordering::Less {
                acc += 1;
            }
        }
        let cmp_ns = t.elapsed().as_nanos() as f64 / n as f64;
        black_box(acc);

        let t = Instant::now();
        for v in &versions {
            black_box((*v).clone());
        }
        let clone_ns = t.elapsed().as_nanos() as f64 / n as f64;
        println!("Version::cmp {cmp_ns:.0} ns/op, Version::clone {clone_ns:.0} ns/op over {n} records");

        // --- microbench: find-highest-matching, linear vs indexed ---
        // Build per-name sorted version arrays (the index build cost is timed).
        let t = Instant::now();
        let mut sorted_by_name: HashMap<&str, Vec<(&Version, usize)>> = HashMap::new();
        for (name, records) in &by_name {
            let mut vs: Vec<(&Version, usize)> = records
                .iter()
                .enumerate()
                .map(|(i, r)| (r.package_record.version.version(), i))
                .collect();
            vs.sort_by(|a, b| a.0.cmp(b.0));
            sorted_by_name.insert(name, vs);
        }
        let index_build = t.elapsed();

        // The workload: every (spec that names a package in the closure and is
        // a single interval) -> find the highest matching version.
        struct Case<'a> {
            name: &'a str,
            nameless: &'a NamelessMatchSpec,
            interval: (Bound<&'a Version>, Bound<&'a Version>),
        }
        let cases: Vec<Case> = parsed
            .iter()
            .filter_map(|(spec, nameless)| {
                let name = spec.name.as_exact()?.as_normalized();
                let (name_key, _) = by_name.get_key_value(name)?;
                if nameless.build.is_some()
                    || nameless.build_number.is_some()
                    || nameless.md5.is_some()
                    || nameless.sha256.is_some()
                {
                    return None;
                }
                let interval = match &nameless.version {
                    None => (Bound::Unbounded, Bound::Unbounded),
                    Some(vs) => to_interval(vs)?,
                };
                Some(Case {
                    name: name_key,
                    nameless,
                    interval,
                })
            })
            .collect();

        // Linear scan, as `find_highest_version` does today.
        let t = Instant::now();
        let linear: Vec<Option<&Version>> = cases
            .iter()
            .map(|case| {
                let mut best: Option<&Version> = None;
                for r in &by_name[case.name] {
                    if case.nameless.matches(*r) {
                        let v = r.package_record.version.version();
                        if best.is_none_or(|b| v > b) {
                            best = Some(v);
                        }
                    }
                }
                best
            })
            .collect();
        let linear_time = t.elapsed();

        // Indexed: two partition points on the sorted versions.
        let t = Instant::now();
        let indexed: Vec<Option<&Version>> = cases
            .iter()
            .map(|case| {
                let sorted = &sorted_by_name[case.name];
                let lo = match case.interval.0 {
                    Bound::Unbounded => 0,
                    Bound::Included(v) => sorted.partition_point(|(x, _)| *x < v),
                    Bound::Excluded(v) => sorted.partition_point(|(x, _)| *x <= v),
                };
                let hi = match case.interval.1 {
                    Bound::Unbounded => sorted.len(),
                    Bound::Included(v) => sorted.partition_point(|(x, _)| *x <= v),
                    Bound::Excluded(v) => sorted.partition_point(|(x, _)| *x < v),
                };
                (hi > lo).then(|| sorted[hi - 1].0)
            })
            .collect();
        let indexed_time = t.elapsed();

        let agree = linear
            .iter()
            .zip(&indexed)
            .filter(|(a, b)| a == b)
            .count();
        println!(
            "highest-version workload: {} indexable cases | linear {:.3} ms | indexed {:.3} ms ({:.1}x) | index build {:.3} ms | agreement {}/{}",
            cases.len(),
            linear_time.as_secs_f64() * 1e3,
            indexed_time.as_secs_f64() * 1e3,
            linear_time.as_secs_f64() / indexed_time.as_secs_f64(),
            index_build.as_secs_f64() * 1e3,
            agree,
            cases.len(),
        );
        println!();
    }
}

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 }
}
