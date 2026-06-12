#!/bin/bash
# A/B benchmark for the package download+extraction pipeline.
#
# Builds `rattler-bin` from two git revisions and measures cold installs of a
# real environment with both, interleaved to cancel network drift. Each run
# starts from an empty package cache and a fresh prefix while sharing a warm
# repodata cache and a pinned --exclude-newer timestamp, so every run solves
# the exact same set of packages.
#
# Wall time alone is too noisy on fast networks; user and sys CPU time are
# reported as well. The extraction pipeline change mainly shows up as a drop
# in sys time (fewer blocking-pool dispatches) and in wall time on fast
# networks (extraction overlapped with the download).
#
# Usage:
#   scripts/bench-install.sh [options] [BASELINE_REV] [CANDIDATE_REV]
#
#   BASELINE_REV   defaults to origin/main
#   CANDIDATE_REV  defaults to HEAD (committed state only)
#
# Options (environment variables):
#   SPECS      package specs to install   (default: "pytorch tensorflow torchvision")
#   CHANNEL    channel to install from    (default: conda-forge; point this at a
#              local mirror, e.g. http://127.0.0.1:8000/mirror, for network-free runs)
#   REPS       interleaved repetitions    (default: 3)
#   COOLDOWN   seconds between runs       (default: 60; be polite to the CDN)
#   WORKDIR    scratch directory          (default: mktemp -d)
#
# Failed runs (e.g. HTTP 503 when the CDN rate-limits a burst of fetches) are
# retried once after an extended cooldown and excluded from the summary if
# they fail again; the summary reports how many runs each side completed.
#
# Suggested spec sets:
#   many small files:  SPECS="python jupyterlab scipy pandas matplotlib"
#   mixed ML stack:    SPECS="pytorch tensorflow torchvision"   (~150 pkgs)
#   large env:         SPECS="rubin-env"                        (~800 pkgs, ~10x runtime)

set -Eeuo pipefail

BASELINE_REV=${1:-origin/main}
CANDIDATE_REV=${2:-HEAD}
SPECS=${SPECS:-"pytorch tensorflow torchvision"}
CHANNEL=${CHANNEL:-conda-forge}
REPS=${REPS:-3}
COOLDOWN=${COOLDOWN:-60}
WORKDIR=${WORKDIR:-$(mktemp -d -t rattler-bench-XXXXXX)}
EXCLUDE_NEWER=$(date -u +%Y-%m-%dT00:00:00Z)

REPO_ROOT=$(git rev-parse --show-toplevel)
RESULTS="$WORKDIR/results.csv"
export RATTLER_CACHE_DIR="$WORKDIR/cache"
mkdir -p "$RATTLER_CACHE_DIR"

build_rev() { # rev name -> prints binary path
    local rev=$1 name=$2
    local tree="$WORKDIR/src-$name"
    git -C "$REPO_ROOT" worktree add --force --detach "$tree" "$rev" >&2
    git -C "$tree" submodule update --init >&2
    cargo build --release -p rattler-bin --manifest-path "$tree/Cargo.toml" >&2
    echo "$tree/target/release/rattler"
}

run_once() { # binary label -> appends csv line, returns the run's exit status
    local bin=$1 label=$2
    rm -rf "$RATTLER_CACHE_DIR/pkgs" "$WORKDIR/prefix"
    local timing status=0
    # shellcheck disable=SC2086
    { TIMEFORMAT='%R %U %S'; timing=$( { time "$bin" create -c "$CHANNEL" \
        --prefix "$WORKDIR/prefix" --exclude-newer "$EXCLUDE_NEWER" $SPECS \
        > "$WORKDIR/log-$label.txt" 2>&1; } 2>&1 ) || status=$?; }
    read -r wall user sys <<< "$timing"
    echo "$label,$status,$wall,$user,$sys" >> "$RESULTS"
    echo "  $label: status=$status wall=${wall}s user=${user}s sys=${sys}s"
    return "$status"
}

run_with_retry() { # binary label
    if ! run_once "$1" "$2"; then
        echo "  $2 failed (see $WORKDIR/log-$2.txt), retrying after cooldown"
        sleep $((COOLDOWN * 2))
        run_once "$1" "$2-retry" || true
    fi
    sleep "$COOLDOWN"
}

summarize() { # side
    awk -F, -v side="$1" '
        $1 ~ "^"side"-" && $2 == 0 { n++; wall+=$3; user+=$4; sys+=$5 }
        END {
            if (n) printf "  %-9s n=%d  wall=%.1fs  user=%.1fs  sys=%.1fs  cpu=%.1fs\n",
                side, n, wall/n, user/n, sys/n, (user+sys)/n
            else printf "  %-9s no successful runs\n", side
        }' "$RESULTS"
}

echo "== rattler install benchmark =="
echo "machine:        $(uname -srm), $(nproc 2>/dev/null || sysctl -n hw.ncpu) cores"
echo "baseline:       $(git -C "$REPO_ROOT" rev-parse --short "$BASELINE_REV") ($BASELINE_REV)"
echo "candidate:      $(git -C "$REPO_ROOT" rev-parse --short "$CANDIDATE_REV") ($CANDIDATE_REV)"
echo "specs:          $SPECS (channel: $CHANNEL, exclude-newer: $EXCLUDE_NEWER)"
echo "workdir:        $WORKDIR"
echo

echo "building binaries..."
BASELINE_BIN=$(build_rev "$BASELINE_REV" baseline)
CANDIDATE_BIN=$(build_rev "$CANDIDATE_REV" candidate)

echo "warming the repodata cache and verifying the solve..."
# shellcheck disable=SC2086
"$CANDIDATE_BIN" create --dry-run -c "$CHANNEL" --prefix "$WORKDIR/prefix" \
    --exclude-newer "$EXCLUDE_NEWER" $SPECS > "$WORKDIR/log-solve.txt" 2>&1
echo "  $(grep -c '^+ ' "$WORKDIR/log-solve.txt") packages"
echo

echo "label,status,wall,user,sys" > "$RESULTS"
for rep in $(seq 1 "$REPS"); do
    echo "rep $rep/$REPS:"
    # Alternate which side goes first to cancel slow time-of-day drift.
    if [ $((rep % 2)) -eq 1 ]; then
        run_with_retry "$BASELINE_BIN" "baseline-$rep"
        run_with_retry "$CANDIDATE_BIN" "candidate-$rep"
    else
        run_with_retry "$CANDIDATE_BIN" "candidate-$rep"
        run_with_retry "$BASELINE_BIN" "baseline-$rep"
    fi
done

echo
echo "== summary (means over successful runs) =="
summarize baseline
summarize candidate
echo
echo "raw results: $RESULTS"
git -C "$REPO_ROOT" worktree remove --force "$WORKDIR/src-baseline" || true
git -C "$REPO_ROOT" worktree remove --force "$WORKDIR/src-candidate" || true
