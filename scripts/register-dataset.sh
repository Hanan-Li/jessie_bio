#!/usr/bin/env bash
# Register a dataset with the API: P0c's stand-in for the agent-side
# inventory flow (DESIGN.md §4.3, "Datasets and input digests").
#
# The control plane never reads data, so this script does what an agent
# will do in P1: find each file, take its size and SHA-256 digest, and
# send only that metadata to POST /api/datasets.
#
#   JESSIE_DEV_TOKEN=... scripts/register-dataset.sh --name "ggal reads" --agent <agent id> \
#       gut=data/ggal/ggal_gut_1.fq,data/ggal/ggal_gut_2.fq
#
# One <sample>=<file>[,<file>...] argument per sample, files in order (R1
# before R2). Paths are made absolute, since the agent reads them from its
# own working directory. Run it on the machine whose disk holds the files,
# the one running that agent. Prints the dataset's id.
#
# Needs curl, jq, sha256sum and realpath. JESSIE_API defaults to
# http://127.0.0.1:8470.
set -euo pipefail

api="${JESSIE_API:-http://127.0.0.1:8470}"
name=""
agent=""
samples=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) name="$2"; shift 2 ;;
    --agent) agent="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *=*) samples+=("$1"); shift ;;
    *) echo "unexpected argument: $1 (expected <sample>=<file>[,<file>...])" >&2; exit 2 ;;
  esac
done
if [[ -z "$name" || -z "$agent" || ${#samples[@]} -eq 0 ]]; then
  echo "usage: $0 --name <name> --agent <agent id> <sample>=<file>[,<file>...] ..." >&2
  exit 2
fi
: "${JESSIE_DEV_TOKEN:?set JESSIE_DEV_TOKEN to the development token the API was started with}"

# Build the samples array one file at a time. jq does the JSON quoting, so
# a path with a quote or a space can't break the request.
samples_json='[]'
for arg in "${samples[@]}"; do
  sample="${arg%%=*}"
  IFS=',' read -r -a files <<< "${arg#*=}"
  files_json='[]'
  for file in "${files[@]}"; do
    path="$(realpath -e "$file")"   # -e: fail if it doesn't exist
    size="$(stat -c %s "$path")"
    digest="sha256:$(sha256sum "$path" | cut -d' ' -f1)"
    echo "  $sample: $path ($size bytes, $digest)" >&2
    files_json="$(jq --arg uri "$path" --arg digest "$digest" --argjson size "$size" \
      '. + [{uri: $uri, digest: $digest, size: $size}]' <<< "$files_json")"
  done
  samples_json="$(jq --arg sample "$sample" --argjson files "$files_json" \
    '. + [{sample: $sample, files: $files}]' <<< "$samples_json")"
done

body="$(jq -n --arg name "$name" --arg agent "$agent" --argjson samples "$samples_json" \
  '{name: $name, location: {agent_id: $agent}, samples: $samples}')"

# --fail-with-body: exit non-zero on an HTTP error, but still print the
# API's explanation of what was wrong.
reply="$(curl -sS --fail-with-body -X POST "$api/api/datasets" \
  -H "Authorization: Bearer $JESSIE_DEV_TOKEN" -H 'Content-Type: application/json' \
  -d "$body")" || { echo "$reply" | jq . >&2 2>/dev/null || echo "$reply" >&2; exit 1; }
jq -r .dataset_id <<< "$reply"
