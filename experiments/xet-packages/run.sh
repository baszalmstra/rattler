#!/usr/bin/env bash
# Reproduce the measurements in README.md.
# Downloads ~6.5 GiB of packages from conda-forge into ./work and needs ~10 GiB of scratch space.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release
BIN="$PWD/target/release/xet-packages-lab"
mkdir -p work/pkgs work/tmp results
export XET_LAB_TMP="$PWD/work/tmp"

while read -r name subdir files; do
  for f in $files; do
    [ -f "work/pkgs/$f" ] || curl -sSf -o "work/pkgs/$f" "https://conda.anaconda.org/conda-forge/$subdir/$f"
  done
  pkgs=$(for f in $files; do echo "work/pkgs/$f"; done)
  # shellcheck disable=SC2086
  "$BIN" analyze --name "$name" --json "results/$name.analyze.json" $pkgs | tee "results/$name.analyze.txt"
done < series.txt

# Real Xet pipeline (upload into a local CAS, download + verify) for two series.
for name in spacy_md pyspark; do
  read -r _ _ files < <(grep "^$name " series.txt)
  pkgs=$(for f in $files; do echo "work/pkgs/$f"; done)
  # shellcheck disable=SC2086
  "$BIN" roundtrip --name "$name" --cas "work/cas-$name" --json "results/$name.roundtrip.json" $pkgs
done
