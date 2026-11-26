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
                   register-dataset.sh: hash files and register them as a dataset with the API
```

## Requirements

- Rust (version pinned in `rust-toolchain.toml`; rustup installs it automatically)
- Java 17+ and Nextflow 26.10.5, for anything that runs Nextflow
- `curl` and `jq`, for `scripts/register-dataset.sh` (`sudo apt install jq`)
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

### Launch a run

The UI endpoints (DESIGN.md §4.3) take the development token instead: in
P0c there are no user accounts, just the one token the server was started
with. Until the frontend exists, `curl` and scripts call them. With
`$AGENT_ID` from `create-agent` above:

```bash
H="Authorization: Bearer $JESSIE_DEV_TOKEN"
CANVAS=$(cat crates/compiler/testdata/fastqc_multiqc.canvas.json)

# Check a canvas (stores nothing): 200 {"valid": true}, or 422 listing every problem.
curl -s -X POST localhost:8470/api/validate -H "$H" -H 'Content-Type: application/json' \
  -d "{\"canvas\": $CANVAS}"

# Save it as a pipeline version (saving under the same name again makes version 2).
PIPELINE_VERSION=$(curl -s -X POST localhost:8470/api/pipelines -H "$H" -H 'Content-Type: application/json' \
  -d "{\"name\": \"QC\", \"canvas\": $CANVAS}" | jq -r .pipeline_version_id)

# Register the reads on the agent's disk: the script hashes them with sha256sum
# and sends only paths, sizes and digests. One <sample>=<R1>,<R2> per sample.
DATASET=$(scripts/register-dataset.sh --name "ggal reads" --agent "$AGENT_ID" \
  gut=data/ggal/ggal_gut_1.fq,data/ggal/ggal_gut_2.fq)

# Launch: compiles the pipeline, stores each job's project, queues the jobs.
RUN=$(curl -s -X POST localhost:8470/api/runs -H "$H" -H 'Content-Type: application/json' \
  -d "{\"pipeline_version_id\": \"$PIPELINE_VERSION\", \"compute_env\": \"laptop\",
       \"inputs\": {\"fastqc_1__reads\": \"$DATASET\"}}" | jq -r .run_id)

curl -s localhost:8470/api/runs/$RUN -H "$H" | jq .         # state, jobs, tasks, outputs
curl -s -X POST localhost:8470/api/runs/$RUN/cancel -H "$H"  # cancel it
```

| Endpoint | |
|---|---|
| `POST /api/validate` | Check a canvas without storing it; 422 lists every problem, typed by `code` |
| `POST /api/pipelines` | Validate and store a canvas as the next version of a named pipeline |
| `POST /api/datasets` | Register samples and their files (paths on one agent's disk, with digests) |
| `POST /api/runs` | Launch a pipeline version on a compute environment; 422 lists every problem |
| `GET /api/runs/{id}` | The run: state, jobs, their tasks and output files |
| `POST /api/runs/{id}/cancel` | Cancel; running jobs stop at their next heartbeat. 409 if already finished |

After launch, the job waits in the queue for an agent.

### Serve jobs with an agent

`agent serve` leases jobs, runs them with Nextflow and reports each result,
until Ctrl-C or SIGTERM (a running job is then stopped and reported
`interrupted`, so the queue retries it; a second Ctrl-C kills it at once):

```bash
export JESSIE_AGENT_TOKEN=jat_...         # from create-agent; an env var keeps it out of `ps`
cargo run -p agent -- serve --server http://127.0.0.1:8470 --work-dir out/agent --stub
```

Each attempt runs in `out/agent/jobs/<job_id>/<attempt>/` (the lease, the
project, the input manifests and the run directory); project files are cached
by digest in `~/.cache/jessie-agent/blobs` (`--cache-dir` to change). Leave out
`--stub` for real runs with Docker.
