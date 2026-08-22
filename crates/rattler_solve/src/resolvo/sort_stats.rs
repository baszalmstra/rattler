//! TEMPORARY instrumentation for profiling candidate sorting. Not for merge.
#![allow(missing_docs)]

use std::sync::atomic::{AtomicU64, Ordering};

pub static STAGE1_NANOS: AtomicU64 = AtomicU64::new(0);
pub static STAGE2_NANOS: AtomicU64 = AtomicU64::new(0);
pub static DEP_GATHER_NANOS: AtomicU64 = AtomicU64::new(0);
pub static FINAL_SORT_NANOS: AtomicU64 = AtomicU64::new(0);
pub static HIGHEST_VERSION_MISS_NANOS: AtomicU64 = AtomicU64::new(0);
pub static HIGHEST_VERSION_CALLS: AtomicU64 = AtomicU64::new(0);
pub static HIGHEST_VERSION_MISSES: AtomicU64 = AtomicU64::new(0);
pub static SORT_CALLS: AtomicU64 = AtomicU64::new(0);
pub static SORTED_SOLVABLES: AtomicU64 = AtomicU64::new(0);
pub static TIEBREAK_RUNS: AtomicU64 = AtomicU64::new(0);
pub static TIEBREAK_RUN_SOLVABLES: AtomicU64 = AtomicU64::new(0);
pub static TIEBREAK_MAX_RUN: AtomicU64 = AtomicU64::new(0);

pub fn add(counter: &AtomicU64, v: u64) {
    counter.fetch_add(v, Ordering::Relaxed);
}

pub fn max(counter: &AtomicU64, v: u64) {
    counter.fetch_max(v, Ordering::Relaxed);
}

pub fn reset() {
    for c in [
        &STAGE1_NANOS,
        &STAGE2_NANOS,
        &DEP_GATHER_NANOS,
        &FINAL_SORT_NANOS,
        &HIGHEST_VERSION_MISS_NANOS,
        &HIGHEST_VERSION_CALLS,
        &HIGHEST_VERSION_MISSES,
        &SORT_CALLS,
        &SORTED_SOLVABLES,
        &TIEBREAK_RUNS,
        &TIEBREAK_RUN_SOLVABLES,
        &TIEBREAK_MAX_RUN,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}

pub fn report() -> String {
    let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
    format!(
        "stage1(simple sort): {:.3} ms\n\
         stage2(tie-break total): {:.3} ms\n\
         .. dep gather: {:.3} ms\n\
         .. final sort_by: {:.3} ms\n\
         .. highest-version misses: {:.3} ms ({} misses / {} calls)\n\
         sort calls: {}, solvables sorted: {}\n\
         tie-break runs: {} (solvables in runs: {}, max run: {})",
        g(&STAGE1_NANOS) as f64 / 1e6,
        g(&STAGE2_NANOS) as f64 / 1e6,
        g(&DEP_GATHER_NANOS) as f64 / 1e6,
        g(&FINAL_SORT_NANOS) as f64 / 1e6,
        g(&HIGHEST_VERSION_MISS_NANOS) as f64 / 1e6,
        g(&HIGHEST_VERSION_MISSES),
        g(&HIGHEST_VERSION_CALLS),
        g(&SORT_CALLS),
        g(&SORTED_SOLVABLES),
        g(&TIEBREAK_RUNS),
        g(&TIEBREAK_RUN_SOLVABLES),
        g(&TIEBREAK_MAX_RUN),
    )
}
