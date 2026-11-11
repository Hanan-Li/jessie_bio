# Jessie's Bioinformatics Platform

A no-code bioinformatics platform: scientists build pipelines on a visual
canvas, which compiles to Nextflow DSL2. Design: [`docs/DESIGN.md`](docs/DESIGN.md).
Background and decisions: [`docs/CONTEXT.md`](docs/CONTEXT.md).

Current stage: P0a (the compiler) and P0b (the agent in local mode) are done;
next is P0c, the control plane.

## Layout

```
crates/compiler/   canvas → validated graph → jobs → Nextflow projects (+ the `compiler` CLI)
crates/agent/      runs a compiled job with Nextflow and records the result (the `agent` CLI)
crates/protocol/   formats shared by compiler, agent and API: job interfaces, manifests,
                   digests, job results, port types
nodes/             the node library: one directory per node version (spec, module, provenance)
scripts/           stub-run.sh: compile the test canvas, lint it, stub-run it with the agent
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

## Run a compiled job with the agent

Each job input is a **manifest**, a CSV of files. For the test canvas, the
only input is `fastqc_1__reads`, per sample, one row per file (R1, then R2).
Relative paths are relative to the manifest:

```csv
sample,path
S1,data/S1_R1.fastq.gz
S1,data/S1_R2.fastq.gz
S2,data/S2_R1.fastq.gz
S2,data/S2_R2.fastq.gz
```

Then:

```bash
cargo run -p agent -- run out/job-0 --input fastqc_1__reads=reads.csv --run-dir out/run-1
```

`agent run` checks the machine (`agent check` does only that), checks the
inputs, runs Nextflow, and records what happened. Add `--stub` to run the
modules' `stub:` blocks without Docker or real data, `--timeout-secs N` to
limit the run. The first real run pulls the containers pinned in the modules.

The run directory ends up with:

```
inputs/        the input manifests, paths made absolute
work/          Nextflow's work directory
results/       outputs, under <data name>/<sample>/ (per sample) or <data name>/
outputs/       one output manifest per output, ready to be the next job's input
logs/          Nextflow's stdout and stderr
trace.tsv      Nextflow's record of every task
result.json    status, tasks, and every output file with its size and sha256 digest
```

Exit codes: 0 success, 1 the agent itself failed, 2 this machine can't run
jobs, 3 the inputs are wrong, 4 the job failed, 5 time limit, 130 cancelled
(Ctrl-C), 143 the agent was shut down (SIGTERM; retry the job). A second
Ctrl-C or SIGTERM while stopping skips Nextflow's grace period.

To run a compiled project by hand instead, without the agent:

```bash
cd out/job-0
nextflow run main.nf --fastqc_1__reads /abs/path/reads.csv -output-dir results
```
