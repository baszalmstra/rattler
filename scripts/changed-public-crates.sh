#!/usr/bin/env bash

# Print the publishable library crates changed relative to a git revision.
# Crates that do not exist in the baseline are new and therefore have no API to
# compare. Workspace-wide Cargo and toolchain changes select every public crate.

set -euo pipefail

base_ref="${1:?Usage: changed-public-crates.sh <base-ref> <baseline-root>}"
baseline_root="${2:?Usage: changed-public-crates.sh <base-ref> <baseline-root>}"
baseline_manifest="${baseline_root%/}/Cargo.toml"

if [[ ! -f "$baseline_manifest" ]]; then
  echo "Baseline manifest not found: $baseline_manifest" >&2
  exit 1
fi

scratch_dir=$(mktemp -d)
trap 'rm -rf "$scratch_dir"' EXIT

changed_files_path="$scratch_dir/changed-files"
current_metadata_path="$scratch_dir/current-metadata.json"
baseline_metadata_path="$scratch_dir/baseline-metadata.json"

git diff --name-only -z "${base_ref}...HEAD" > "$changed_files_path"
mapfile -d '' -t changed_files < "$changed_files_path"

cargo metadata --no-deps --format-version 1 > "$current_metadata_path"
cargo metadata \
  --manifest-path "$baseline_manifest" \
  --no-deps \
  --format-version 1 > "$baseline_metadata_path"

check_all=false
for file in "${changed_files[@]}"; do
  case "$file" in
    Cargo.toml | Cargo.lock | rust-toolchain | rust-toolchain.toml | .cargo/*)
      check_all=true
      break
      ;;
  esac
done

selected_packages=()
while IFS=$'\t' read -r package directory; do
  if [[ "$check_all" == true ]]; then
    selected_packages+=("$package")
    continue
  fi

  for file in "${changed_files[@]}"; do
    if [[ "$file" == "$directory/"* ]]; then
      selected_packages+=("$package")
      break
    fi
  done
done < <(
  jq -r --slurpfile baseline "$baseline_metadata_path" '
    .workspace_root as $root
    | .workspace_members as $members
    | ($baseline[0].packages | map(.name)) as $baseline_packages
    | .packages[]
    | .id as $id
    | .name as $name
    | select($members | index($id))
    | select(.publish != [])
    | select($baseline_packages | index($name))
    | select(any(.targets[].kind[];
        . == "lib"
        or . == "rlib"
        or . == "dylib"
        or . == "cdylib"
        or . == "staticlib"
        or . == "proc-macro"))
    | [
        $name,
        (.manifest_path
          | ltrimstr($root)
          | ltrimstr("/")
          | ltrimstr("\\")
          | rtrimstr("/Cargo.toml")
          | rtrimstr("\\Cargo.toml")
          | gsub("\\\\"; "/"))
      ]
    | @tsv
  ' "$current_metadata_path"
)

if (( ${#selected_packages[@]} > 0 )); then
  printf '%s\n' "${selected_packages[@]}" | sort -u | paste -sd ' ' -
fi
