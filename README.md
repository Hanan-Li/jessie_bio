# Jessie's Bioinformatics Platform

A no-code bioinformatics platform: scientists build pipelines on a visual
canvas, which compiles to Nextflow DSL2. Design: [`docs/DESIGN.md`](docs/DESIGN.md).
Background and decisions: [`docs/CONTEXT.md`](docs/CONTEXT.md).

Current stage: **P0a, the compiler core** (canvas JSON → Nextflow projects).

## Layout

```
crates/compiler/   canvas → validated graph → jobs → Nextflow projects (+ the `compiler` CLI)
crates/protocol/   formats shared by compiler, agent and API: job interfaces, manifests, types
nodes/             the node library: one directory per node version (spec, module, provenance)
scripts/           stub-run.sh: compile the test canvas and check it with Nextflow
```

## Requirements

- Rust (version pinned in `rust-toolchain.toml`; rustup installs it automatically)
- Java 17+ and Nextflow 26.10.5, for anything that runs Nextflow
- Docker, for real runs. Your user must be able to use it without `sudo`:

  ```bash
  sudo usermod -aG docker "$USER"   # then log out and back in
  docker run --rm hello-world       # check
  ```

## Checks

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
nextflow lint nodes/
scripts/stub-run.sh      # compile + lint + stub run, no containers or real data
```

## Compile a canvas

```bash
cargo run -p compiler -- crates/compiler/testdata/fastqc_multiqc.canvas.json --out out/
```

This writes one Nextflow project per segment job (`out/job-0/`): `main.nf`,
`nextflow.config`, the node modules, and `job.json`, which lists the job's
inputs and outputs.

## Run a compiled project for real

Each job input is a **manifest**, a CSV of files. For the test canvas, the
only input is `fastqc_1__reads`, per sample, one row per file (R1, then R2):

```csv
sample,path
S1,/abs/path/S1_R1.fastq.gz
S1,/abs/path/S1_R2.fastq.gz
S2,/abs/path/S2_R1.fastq.gz
S2,/abs/path/S2_R2.fastq.gz
```

Then:

```bash
cd out/job-0
nextflow run main.nf --fastqc_1__reads /abs/path/reads.csv -output-dir results
```

The first run pulls the two containers pinned in the modules. Outputs land in
`results/<data name>/<sample>/` (per sample) or `results/<data name>/` (per
dataset), and `trace.tsv` records every task.
