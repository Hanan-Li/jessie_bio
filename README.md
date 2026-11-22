# Jessie's Bioinformatics Platform

A no-code bioinformatics platform: scientists build pipelines on a visual
canvas, which compiles to Nextflow DSL2. Design: [`docs/DESIGN.md`](docs/DESIGN.md).
Background and decisions: [`docs/CONTEXT.md`](docs/CONTEXT.md).

Current stage: P0a (the compiler) and P0b (the agent in local mode) are done;
P0c, the control plane, is being built (DESIGN.md §4.3 lists the order).

## Layout

```
crates/compiler/   canvas → validated graph → jobs → Nextflow projects (+ the `compiler` CLI)
crates/agent/      runs a compiled job with Nextflow and records the result (the `agent` CLI)
crates/api/        the control plane: HTTP API on Postgres (the `api` server); migrations/
crates/protocol/   formats shared by compiler, agent and API: job interfaces, manifests,
                   digests, job results, port types
nodes/             the node library: one directory per node version (spec, module, provenance)
scripts/           stub-run.sh: compile the test canvas, lint it, stub-run it with the agent
                   dev-db.sh: start the development Postgres in Docker
```

## Requirements

- Rust (version pinned in `rust-toolchain.toml`; rustup installs it automatically)
- Java 17+ and Nextflow 26.10.5, for anything that runs Nextflow
- Docker, for real runs and the development database. Your user must be able
  to use it without `sudo`:

  ```bash
  sudo usermod -aG docker "$USER"   # then log out and back in
  docker run --rm hello-world       # check
  ```

## Development database

The `api` crate's tests and server need Postgres. `scripts/dev-db.sh` runs it
in Docker on `127.0.0.1:5433` (not 5432, which a system Postgres often holds),
with its data in the `jessie-pgdata` volume:

```bash
scripts/dev-db.sh
export DATABASE_URL=postgres://jessie:jessie@localhost:5433/jessie
```

## Checks

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked       # needs DATABASE_URL (see above) for the api tests
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

## Run the API server

```bash
export JESSIE_DEV_TOKEN=$(openssl rand -hex 16)   # opens the UI endpoints (below)
cargo run -p api          # needs DATABASE_URL; listens on http://127.0.0.1:8470
curl localhost:8470/healthz
```

On start it applies the database migrations, publishes the node library
(refusing to start if `nodes/` changed a version that was already published),
and starts the reaper, which every 10 seconds takes back the leases of agents
that went silent.

Agents need a compute environment and a token:

```bash
cargo run -p api -- create-compute-env laptop --nextflow-version 26.10.5
cargo run -p api -- create-agent laptop --name my-laptop   # prints the token, only once
```

The agent endpoints (DESIGN.md §4.1) take the token as `Authorization: Bearer <token>`:

```bash
curl -X POST localhost:8470/api/agent/lease \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"agent_version": "0.1.0"}'      # 204: nothing queued yet
```

| Endpoint | |
|---|---|
| `POST /api/agent/lease` | Lease the oldest queued job (200 with the job), or 204 |
| `POST /api/agent/jobs/{id}/heartbeat` | Renew the lease; the reply says `continue` or `cancel`, 409 if the lease was lost |
| `POST /api/agent/jobs/{id}/result` | Report the job's `result.json`; 409 if this attempt is no longer current |
| `GET /api/agent/blobs/{digest}` | A project file, only for jobs in the agent's own environment |

The UI endpoints (DESIGN.md §4.3) take the development token instead: in
P0c there are no user accounts, just the one token the server was started
with. So far there is one, checking a canvas without storing it:

```bash
curl -s -X POST localhost:8470/api/validate -H "Authorization: Bearer $JESSIE_DEV_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"canvas\": $(cat crates/compiler/testdata/fastqc_multiqc.canvas.json)}"
# 200 {"valid": true}, or 422 listing every problem, each typed by `code`
```

Nothing queues jobs yet: launching runs (`POST /api/runs`) is coming in the
next parts of P0c PR 4.
