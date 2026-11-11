#!/usr/bin/env bash
# Compile the test canvas, lint the result, and stub-run it with the agent.
#
# A stub run (`-stub-run`) runs each module's `stub:` block, which creates
# placeholder output files instead of running the real tool. With Docker
# off, it checks the generated wiring end to end (manifests, gathers,
# publishing) without containers or real data.
#
# Needs: cargo, Java 17+, Nextflow. Usage: scripts/stub-run.sh
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "== compile"
(cd "$repo" && cargo run --quiet --locked -p compiler -- \
    crates/compiler/testdata/fastqc_multiqc.canvas.json --out "$work/projects")
project="$work/projects/job-0"

echo "== lint"
nextflow lint "$project"

echo "== agent run --stub (2 samples, paired reads)"
mkdir "$work/data"
manifest="$work/reads.csv"
echo "sample,path" > "$manifest"
for sample in S1 S2; do
    for read in R1 R2; do
        file="$work/data/${sample}_${read}.fastq.gz"
        : > "$file"
        echo "$sample,$file" >> "$manifest"
    done
done
(cd "$repo" && cargo run --quiet --locked -p agent -- run "$project" --stub \
    --input fastqc_1__reads="$manifest" \
    --run-dir "$work/run")

echo "== check published outputs"
for file in \
    fastqc_1__html/S1/S1.html fastqc_1__html/S2/S2.html \
    fastqc_1__zip/S1/S1.zip fastqc_1__zip/S2/S2.zip \
    multiqc_1__report/multiqc_report.html; do
    if [ ! -f "$work/run/results/$file" ]; then
        echo "missing output: $file" >&2
        exit 1
    fi
done
for file in trace.tsv logs/stdout.log logs/stderr.log result.json outputs/fastqc_1__zip.csv; do
    if [ ! -f "$work/run/$file" ]; then
        echo "missing run file: $file" >&2
        exit 1
    fi
done

echo "== check result.json"
result="$work/run/result.json"
grep -q '"status": "succeeded"' "$result" || { echo "result.json: job did not succeed" >&2; exit 1; }
digests=$(grep -c '"digest": "sha256:' "$result")
if [ "$digests" -ne 5 ]; then
    echo "result.json: expected 5 hashed output files, found $digests" >&2
    exit 1
fi
echo "stub run OK"
