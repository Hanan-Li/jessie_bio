# Jessie's Bioinformatics Platform — Design Document

**Status:** Draft v3 · **Date:** 2026-11-11 (v2: 2026-09-27, v1: 2026-09-14) · **Author:** David (with design assistance from Claude)

**Implementation:** P0a (compiler) and P0b (agent, local mode) are built and merged; P0c (control plane) is being built: the protocol, the API skeleton, the queue with the agent endpoints, and the UI endpoints with launch are done (§4.3, build order). See §10.

---

## 1. Vision

A **no-code, scientist-focused bioinformatics platform**: bench scientists visually compose analysis pipelines from curated, typed building blocks, run them on managed cloud or institutional hardware, and get interactive, provenance-tracked figures and tables — without writing code, editing config files, or knowing what a BAM index is.

The contrast with existing tools is deliberate:

- **Seqera Platform** is powerful but developer-focused — it assumes you can write or at least read Nextflow. We target the scientist who cannot and shouldn't have to.
- **Galaxy** proved visual workflows in bioinformatics but sits at tool-level granularity, so building a workflow still requires bioinformatician-level knowledge. We compose at the level scientists think in.

Three product pillars:

1. **Visual pipeline builder** — drag-drop canvas of typed nodes; invalid pipelines are impossible to construct.
2. **Data platform** — projects, datasets, samples, files, and figures as first-class objects with end-to-end provenance.
3. **Hybrid execution** — one hosted control plane; pipelines run on managed cloud compute or on institutional hardware via a lightweight agent, chosen per-run.

**Target users:** institutions and core facilities — facility bioinformaticians (who curate), bench scientists (who run), PIs (who oversee), and external collaborators (who view).

**Strategic wedge:** the platform turns an institution's bioinformatics core from a ticket queue into a *node/template publisher* serving its bench scientists.

---

## 2. Personas

| Persona | Needs | Platform role |
|---|---|---|
| Bench scientist | Run standard analyses on their data, get figures, no code | Project Editor |
| PI | Oversight, membership control, provenance for papers | Project Maintainer |
| Facility bioinformatician | Cross-project support, curate nodes/templates, manage compute | Workspace Admin |
| External collaborator | View one project's results, nothing else | Member + Viewer on one project |
| IT / facility director | Member management, compute environments, audit | Workspace Admin/Owner |

---

## 3. Product Surface

### 3.1 Pipeline Builder (the canvas)

A React Flow canvas where users drag **nodes** from a palette, connect typed ports, and configure parameters via generated forms.

**Node granularity — three levels:**

- **Stage-level (default palette):** one node = one coherent scientific step — "Align & quantify (RNA-seq)", "QC report", "Differential expression", "Volcano plot". Roughly nf-core subworkflow granularity. This is what scientists think in.
- **Tool-level (advanced palette):** one node = one tool (STAR, samtools sort). For power users; hidden by default. Deferred past v1.
- **Pipeline-level presets = templates:** a template is a pre-wired canvas of stage nodes (e.g. "Standard RNA-seq") that the user opens and may modify. Templates solve cold-start and teach composition by example.

**Execution semantics:** canvases are **static DAGs** in v1. No conditionals, no loops. Per-sample scatter/gather is expressed through the type system (below), not control flow.

### 3.2 Type system

Every node port carries a **semantic type**, not "file":

- Format + qualifiers: `FASTQ(paired)`, `BAM(sorted, indexed)`, `VCF`, `QcReport` (raw per-tool QC output that MultiQC aggregates), `HtmlReport` (a tool's standalone HTML report), `CountMatrix`, `SampleSheet`, `Genome`, `ReportArtifact`.
- **Cardinality:** `per-sample` vs `per-dataset`. This encodes Nextflow's scatter/gather without exposing channel operators: a per-sample → per-dataset edge is a gather (compiler emits `.collect()`), e.g. MultiQC collecting all samples' QC.

Consequences:

- The canvas **refuses incompatible connections** — scientists cannot build an invalid pipeline. Errors happen at design time, not four hours into a run.
- Nodes may declare **implicit adapters** (unsorted BAM → node needing sorted BAM: the compiler inserts a sort, or the UI offers it).
- The visual graph is a dataflow graph over typed collections — which is exactly Nextflow's channel model, making compilation natural.

### 3.3 The compiler

The canvas is never executed directly. It **compiles to real Nextflow DSL2**: generated workflow code importing curated modules, wired per the canvas, with pinned node versions and container digests.

- **Export-as-Nextflow** is a first-class feature: inspectable, runnable-elsewhere code. This is the trust-building escape hatch for institutional bioinformaticians.
- Compilation validates the full graph before any compute is touched: every spec and port exists, connected types are compatible, each input is fed at most once, node ids are valid identifiers, and there are no cycles. All errors are reported at once. Unconnected inputs are allowed: they become run inputs. Parameters are rejected until specs can declare them (§3.4).
- Generated code targets Nextflow's **strict syntax**, pinned to Nextflow 26.10.5, so output is linted by Nextflow itself in CI.

**Segmentation.** A canvas is not compiled into one Nextflow run, nor one run per node. The compiler partitions it into **segments** — connected groups of batch nodes — and each segment becomes one Nextflow run. Boundaries fall only where they are needed anyway: at analysis nodes (which bypass Nextflow) and at cache hits.

- Segments must be **convex**: no path may leave a segment and re-enter it. Without this, `A → B` plus `A → X(analysis) → B` would put A and B in one segment that must run both before and after X.
- Rule: each node's *analysis depth* is the largest number of analysis nodes on any path into it. Batch nodes joined by an edge share a segment only when their depths are equal. This is provably convex but conservative: it can split a segment that could safely stay whole (an independent analysis node feeding the middle of a batch chain), costing an extra barrier, never correctness.
- Within a segment, Nextflow does what it is best at: scatter/gather and per-sample pipelining (sample 1 aligns while sample 24 is still trimming).
- Across segments, the control-plane scheduler orchestrates. Every boundary is a barrier and a materialization point, so boundaries are kept to a minimum.
- **Cache pruning before codegen:** cached nodes are removed from the graph and their stored outputs are injected as ordinary segment inputs. The cache works per node while execution stays per segment.
- Every node's outputs are published and hashed, not just segment-final outputs, so the per-node cache has something to store.
- Segments are also the unit of placement for multi-environment runs (§5.4).

**Code generation.** The compiler generates only the *wiring*; process code is curated.

- Each node's Nextflow `process` is a hand-curated module in the node library (§3.4), written once and reviewed by people. The compiler emits the `include` lines, channel construction, process calls and output publishing.
- Each segment has an **interface**: its inputs are run inputs (unconnected input ports, e.g. reads from the samplesheet) or outputs of earlier jobs; its outputs are every node's outputs (all are published and hashed, per above).
- **Every boundary crossing is a manifest file**, a CSV the generated code reads with `splitCsv`. Per-sample: columns `sample,path`, one row per file (a paired sample has two rows, R1 then R2), so the sample's `meta` survives the boundary and provenance stays intact. Per-dataset: a single `path` column. Samplesheets, job inputs and agent-produced output manifests share this one format.
- **Every piece of data in a run has one name**, `<node>__<port>` (e.g. `fastqc_1__zip`): the job output that produces it, the job input that receives it, and the Nextflow `params.<name>` pointing at its manifest. A job input's cardinality is the *producer's*; any gather happens inside the consuming job, after the manifest is read.
- The job interface and manifest types live in the shared `protocol` crate, with `SemanticType`/`Cardinality`, because the compiler, agent and API all read or write them.
- The compiler is pure: it returns a `NextflowProject` (a map of path → file text: `main.nf`, the vendored modules, `nextflow.config`, and `job.json` describing the job's interface). Callers write it out: a CLI in P0a, the control plane (stored with the run, fetched by the agent at lease time) from P0c.
- `main.nf` is rendered with minijinja templates. All decisions (aliases, wiring, `.collect()` placement, call order) are made in Rust and passed to the template as plain view structs, so Rust's exhaustive `match` covers them and templates only format.
- Outputs are published with Nextflow workflow outputs in copy mode, one directory per data name: per-sample data under `<name>/<sample>/`, per-dataset data under `<name>/`. The agent builds output manifests from that layout. `nextflow.config` also enables the trace file the agent parses.
- Tests compare generated projects byte-for-byte against expected files in the repo (regenerated with `UPDATE_EXPECTED=1`). Generated projects are also checked with `nextflow lint` and a **stub run** (`-stub-run`, Docker off): the modules' `stub:` blocks create placeholder outputs, which exercises manifests, wiring, gathers and publishing without containers or real data.
- Analysis-job code generation is deferred until the first analysis node exists.

### 3.4 Node specification

A node is a curated artifact comprising:

- Container image (pinned by digest)
- Wrapped nf-core module/subworkflow or custom script
- Typed port declarations (inputs/outputs with semantic types + cardinality)
- Parameter schema → auto-rendered form (defaults, units, ranges, scientist-grade help text) — not implemented yet; until specs declare parameters and codegen passes them on, validation rejects any `params` on a canvas node rather than silently ignoring them
- Resource hints (CPU/memory/disk classes)
- **Execution class:** `batch` or `analysis` (see §5)

**Node library.** The core library lives in the platform repo under `nodes/<id>/<version>/`:

```
nodes/fastqc/0.1.0/
  spec.json     NodeSpec: ports, execution class (parameters later)
  main.nf       Nextflow process (batch nodes)
  UPSTREAM.md   provenance of vendored code: repo, commit, path, license, local changes
```

- A version directory is immutable: any change (module code, container) is a new version.
- Where nf-core has a well-tested module, it is **vendored**: copied from `github.com/nf-core/modules` at a pinned commit (MIT-licensed; notice kept in `UPSTREAM.md`). Local changes are expected and recorded: containers pinned by digest instead of tag, strict-syntax fixes, unused inputs removed.
- Convention linking spec and module: input ports in declared order are the process's inputs; output port names are its `emit:` names. Every port's process input and output is a `tuple val(meta), path(...)` (the nf-core convention), per-dataset ones included.
- Node ids on a canvas must be valid Nextflow identifiers, since codegen uses them in process aliases, and may not contain `__`, which separates node from port in data names.

Specs are JSON, not YAML: `serde_yaml`, the usual Rust YAML crate, is no longer maintained, the database will store specs as JSON, and the same serde types read both. The library loader checks every node on load: the spec parses, its id and version match its directory, and a batch node's module file exists inside the node directory and declares the named process. Today's library: FastQC 0.1.0 and MultiQC 0.1.0, vendored from nf-core/modules at `9791512`.

**Authorship:** v1 ships a platform-curated core library defined as spec files + containers in the platform repo. The spec format is designed from day one for third-party authorship; a node-authoring UI/registry for institutional cores is a later phase. Curation quality *is* the product — ~12 excellent nodes beat 40 mediocre ones.

### 3.5 Samplesheet builder

Mapping uploaded files to samples/conditions/replicates is the most error-prone step in real genomics, so it gets a dedicated, opinionated UI (arguably the most valuable screen in the product):

- Guided flow: upload/point at files → automatic R1/R2 pair detection → filename-pattern extraction of sample names → editable grid for conditions/replicates/covariates
- Validation before launch: missing pairs, duplicate samples, conditions with n=1, inconsistent lanes
- Output is a first-class `SampleSheet` object bound to a Dataset, reusable across runs

### 3.6 Runs & monitoring

- Launch = pick pipeline version + input dataset(s) + parameter overrides + compute environment
- Run monitor: DAG view with per-node state, logs, and resource metrics streamed from the agent
- Pre-launch cache preview: "this change re-executes 2 of 14 nodes, est. 40 seconds" (§5.2)

### 3.7 Results experience — reports are first-class

Scientists want figures and tables, not a file browser. Visualization nodes emit **structured report artifacts: a Vega-Lite plot spec + underlying data**, not just a PNG. The platform renders them natively — interactive volcano plots, sortable DE tables, embedded MultiQC — attached to the run and surfaced in the project.

Every figure is traceable to the exact run, parameters, node versions, and input samples that produced it. *"Click any figure from your lab and see exactly how it was made"* is the provenance story, and it falls out of the data model (§7) rather than a bolted-on lineage system. PNG/SVG export for papers comes free with Vega-Lite.

### 3.8 Interactive analysis (v1 approach)

No hosted notebooks in v1. Instead, **analysis nodes** are cheap to re-run: a scientist tweaks a p-value threshold on a plot node and only the affected downstream sub-DAG re-executes (§5), targeting sub-30-second re-render. Hosted notebooks are a possible later phase; "export and analyze elsewhere" always works via export.

---

## 4. Architecture Overview

```
┌────────────────── Control Plane (hosted, cloud) ──────────────────┐
│  Web UI (Next.js + React Flow)                                    │
│  API (Rust/axum) · Auth (Clerk; SSO via WorkOS later)             │
│  Postgres: projects, samples, runs, file records, provenance,     │
│            node registry, job queue (SKIP LOCKED)                 │
│  Compiler (canvas → Nextflow DSL2) · DAG scheduler · Cache index  │
└──────────────────────────────┬────────────────────────────────────┘
                               │  outbound-only HTTPS (poll / stream)
            ┌──────────────────┼──────────────────────┐
       ┌────┴─────────┐  ┌─────┴─────────┐  ┌─────────┴──────────┐
       │ Runner agent │  │ Runner agent  │  │ Runner agent       │
       │ Managed EC2  │  │ Slurm / HPC   │  │ Customer AWS       │
       │ + platform S3│  │ + on-prem     │  │ (BYO account)      │
       │              │  │   storage     │  │ + customer S3      │
       └──────────────┘  └───────────────┘  └────────────────────┘
                    Workspace data planes (bytes live here)
```

**Core principle: the control plane stores metadata only; bytes live in the workspace data plane.** This single rule resolves the tension between "seamless managed product" and "institutions won't put sensitive data on a third-party site."

### 4.1 Runner agent — one execution abstraction

Every compute backend is an **agent** that registers with the control plane and **polls outbound-only over HTTPS** (no inbound firewall holes into institutional networks — the politically viable option for on-prem). A managed cloud environment is simply an agent the platform operates itself; an on-prem environment is the same agent binary installed by a sysadmin. GitLab-Runner-shaped.

Agent responsibilities:

- Lease jobs for its compute environment (lease + heartbeat + expiry semantics, §6)
- **Batch jobs:** materialize the generated Nextflow project, execute (local Docker executor in v1; Slurm/AWS Batch via Nextflow config later), stream task state transitions, logs, and metrics back
- **Analysis jobs:** run the node's container directly — no Nextflow session, no queue — for the fast interactive path
- Upload output artifacts + content hashes to the workspace data plane; register FileRecords via the API
- **Data gateway (sovereign tier):** broker byte access for UI previews when storage is on-prem

Agents authenticate with compute-environment-scoped tokens against a separate `/api/agent/*` surface (§8.3). The agent is a **Rust static binary** sharing a protocol-types crate with the API. The binary itself has no dependencies (today it is an ordinary glibc build; the static musl build comes with agent packaging in P2), but hosts running batch nodes also need a JVM + Nextflow (Nextflow is Groovy/Java, Java 17+). The agent either uses the site install (e.g. `module load nextflow` on HPC) or bootstraps and caches a pinned JRE + Nextflow version on first run — pinning the version per compute environment also serves reproducibility. Analysis-class nodes never touch the JVM.

**Local mode (P0b).** Before it talks to any server, the agent runs one compiled job from disk: `agent run <project> --input <name>=<manifest> ... --run-dir <dir>`. P0c replaces only where jobs come from (a lease over HTTPS) and where results go (the API); the middle stays the same.

1. **Preflight**: Java 17+, Nextflow at the pinned version (enforced with `NXF_VER`), Docker reachable without `sudo` (skipped for stub runs).
2. **Stage**: read `job.json`; check every input has exactly one manifest of the right shape and every file it lists exists; write normalized copies (absolute paths) into the run directory. The project directory is never modified, so one compiled project can be run many times.
3. **Run**: `nextflow run` as a child process, in its own process group, from the run directory, both output streams drained concurrently into log files. A stop (Ctrl-C, a time limit, or SIGTERM to the agent itself) sends SIGTERM to Nextflow, which stops its containers; after a grace period, or at a second Ctrl-C/SIGTERM, the whole process group gets SIGKILL. SIGTERM to the agent (systemd, a sysadmin) is handled like any other stop, so Nextflow never outlives the agent and `result.json` is always written; the job's status is then `interrupted`, meaning "retry it", distinct from `cancelled`.
4. **Parse** `trace.tsv` into typed task records.
5. **Collect**: walk `results/<name>/<sample>/`, hash every file, write one output manifest per job output.
6. **Report**: `result.json` (a `protocol` type): status (`succeeded`, `failed`, `timed_out`, `cancelled`, `interrupted`), tasks, outputs with hashes and sizes, and warnings (e.g. an unparseable trace, which no longer prevents the report). In P0c this becomes the body of the agent's "job finished" call.

Run directory: `inputs/` (staged manifests), `work/` (Nextflow work dir), `results/`, `logs/`, `trace.tsv`, `outputs/` (output manifests), `result.json`.

- The trace is written with `raw = true` (times in milliseconds) and parsed by column name, never position.
- Outputs are collected only when the job succeeded, so a partial result is never mistaken for a full one. A failed job still records every task, which separates "a task failed on its data" (a failed task, with the tool's own error in its log) from "Nextflow or the pipeline failed" (no failed task).
- Output manifests use the same CSV format as input manifests, so one job's outputs feed the next job directly.
- Directory outputs (e.g. MultiQC's `multiqc_data/`) aren't collected yet; none of today's ports is a directory.

Generated configs run task containers as the calling user (`docker.runOptions = '-u $(id -u):$(id -g)'`); with Docker's default of root, every file in `work/` belongs to root and the agent's user can't clean up.

Known issue: when a run is stopped, Nextflow exits about a second before Docker has removed the task container it was stopping, so the agent reports "stopped" slightly early. Fix by labelling each run's containers (e.g. `--label` in the run's Docker options) and waiting until none with that label remain; untagged `docker ps` filtering would also catch other jobs' containers on a shared host.

Failures are typed, because each kind gets a different response: the environment is wrong (fix the host), the inputs are wrong (fix the run request), a task failed (tell the scientist), the run was cancelled or timed out, or the agent itself broke (alert us).

Hashes are **self-describing**: `sha256:<hex>`, like container digests. The algorithm is an enum with one variant today; adding another (e.g. BLAKE3 for very large files) leaves old records readable.

**Agent protocol (P0c).** Designed 2026-11-13. The messages are built (`protocol`, PR 1) and so is the control plane's side (`api`, PR 3); the agent's side, `agent serve`, is PR 5 (in progress: the blob cache, writing a lease to disk and the HTTP client are built).

*Leases.* An agent doesn't own a job; it holds a **lease**: a time-limited claim ("mine until T") that it renews with heartbeats. A lease isn't the job's run time but how long the agent may go silent, so a 6-hour alignment simply keeps renewing. If heartbeats stop (crash, `kill -9`, lost power, network partition), the lease expires and the reaper returns the job to the queue (§6). This is the only way to tell a dead agent from a slow one. Same idea as Pub/Sub ack deadlines or SQS visibility timeouts.

- **Fencing.** Every lease carries an `attempt` number, incremented on each acquire. Heartbeats and results must echo it, and the control plane accepts only the current one: an agent cut off by a partition, whose lease was reassigned, gets `409 Conflict`, stops its run, and discards its work.
- **At-least-once.** A job can therefore run twice (the cut-off agent and its replacement). That must be harmless: outputs are content-addressed, and each attempt works in its own directory.
- **One clock.** Expiry times come from Postgres's `now()`, never from an agent's clock.
- **An expired lease that hasn't been reaped yet can still be renewed** (decided while building PR 3): the attempt is the fence, not the clock. Nobody else can hold the job until the reaper has moved it, and after that the attempt check fails. Refusing it would only turn a late heartbeat into lost work.
- **Results are idempotent.** If the agent's first `result` call got through but the reply was lost, sending it again returns 200 and changes nothing; only a different agent or attempt gets `409`.
- **Starting values:** lease 60 s, heartbeat every 20 s (two heartbeats can fail and the lease still holds), 3 attempts before a job is marked failed instead of retried, so a job that crashes agents can't take them all down in turn.
- **Commands ride on heartbeats.** Agents only call out (no inbound connections), so "cancel" is the reply to the next heartbeat. In `supervise`, "cancel" and "lease lost" become two more branches of the stop future, alongside Ctrl-C, SIGTERM and the time limit.

*Messages.* Three calls on the `/api/agent/*` surface:

```
POST /api/agent/lease                  → a lease response, or 204 No Content (poll again)
POST /api/agent/jobs/{id}/heartbeat    { attempt, progress }  → { action: continue | cancel }, or 409 (lease lost)
POST /api/agent/jobs/{id}/result       { attempt, result: JobResult }  → 200, or 409 (stale attempt)
```

Short polling with backoff first; long polling (the server holds the request until a job is queued, via `LISTEN/NOTIFY`) later.

As built (PR 5, part 2, `client.rs`): each call returns what the agent must act on as an enum, so "no work" (204), "lease lost" (409 on a heartbeat) and "stale" (409 on a result) are outcomes the caller has to `match`, not errors; errors are for an unreachable server, a refused token (401, with a hint to create a new one) or an unexpected status, described with the API's own error message. Every request has a 15 s timeout, so a hung connection can't hold a heartbeat past the 60 s lease. A blob download stops as soon as the body exceeds the size its reference promised, before the extra bytes are held in memory. The HTTP library honours `HTTPS_PROXY`/`NO_PROXY`, since on-prem networks often allow outbound traffic only through a proxy.

*The lease response* is the agent's whole job description. It carries metadata only, never data bytes:

| Field | Contents |
|---|---|
| `format_version` | The message format version (see versioning below) |
| `lease` | `job_id`, `attempt` (the fencing token), `expires_at`, `heartbeat_every_secs` (server-tunable without upgrading agents) |
| `run` | `run_id` and the job's index in its plan, for logs and the agent's directories |
| `settings` | Nextflow version, hash algorithm, time limit: decided per compute environment by the control plane, no longer agent constants or CLI flags |
| `project` | The project's tree as `{digest, size}` (below). The agent fetches the tree like any blob, so everything it acts on was checked against a hash; an inline listing would have raised the question of whether it matched its digest. |
| `interface` | The `JobInterface` (`job.json`) |
| `inputs` | Each job input, **already resolved** by the scheduler (`from: run` → the dataset's files; `from: job` → the files recorded in that earlier job's result), as `{sample, uri, digest}` records. Digests let the agent verify inputs against what was registered; when to pay for that hashing is a policy for later. The agent writes the CSV manifests for Nextflow itself. |

The agent writes all of this to disk in the layout local mode reads today (`jobs/<job_id>/<attempt>/`), then runs the unchanged stage → run → collect code. Local mode becomes the special case where the lease came from a directory.

As built (PR 5, part 1, `job_dir.rs`): `jobs/<job_id>/<attempt>/` holds `lease.json` (the lease as received, for debugging), `project/` (the tree's files), `manifests/<name>.csv` (each resolved input, per-sample files grouped by sample in the order they arrive, so R1 stays before R2) and `run/`, the empty run directory that staging fills. Before writing anything the agent checks the lease against itself and reports every problem: each interface input has files of the right cardinality, per-sample files name a sample and per-dataset files don't, there are no extra inputs, and the job id and input names are safe as directory and file names. After fetching the tree it checks that the project's `job.json` equals the lease's `interface`, since staging reads the former and the inputs were checked against the latter. Problems are of two kinds, because `agent serve` answers them differently: a **bad lease** fails the same way on every agent, so retrying is pointless; a failed download or write might succeed next time. An attempt directory that already exists is refused rather than reused. Input digests aren't verified yet (only that the files exist, in staging). Trees carry no file modes, so a module's helper scripts (nf-core's `bin/`) would arrive without the executable bit; none of today's modules has one.

*Project files by digest, like Bazel's content-addressable store.* Each project file is a **blob** stored under its own digest, so a module shared by many projects (FastQC 0.1.0) is stored and downloaded once. A project is a **tree** listing path → `{digest, size}`; the tree is itself a blob, and its digest is the project's digest (a Merkle tree, like git trees, OCI manifests and Bazel's `Directory` messages). The lease carries the tree's reference; the agent fetches the tree and then only the blobs it lacks (`GET /api/agent/blobs/{digest}`). Tree paths are checked on both sides (relative, `/`-separated, no empty, `.` or `..` parts), because the agent writes files at those paths: a tree can never make it write outside its directory.

- Blobs never change, so they can be cached forever (HTTP `Cache-Control: private, max-age=31536000, immutable`, the digest as ETag). `private`, not `public`: a blob is served only to agents of environments whose jobs use it, so a shared proxy mustn't hand it to anyone else. Which blobs a tree lists is stored in `tree_entries`, so that check is one query.
- The agent hashes every blob it receives and rejects a mismatch; the size catches truncation early and lets it refuse oversized blobs.
- Agent cache: `~/.cache/jessie-agent/blobs/sha256/<2 hex>/<rest>`. Write to a temporary file, then rename into place, so a partial download is never visible; only verified bytes enter the cache. As built (`blobs.rs`): temporary files live in the cache's own `tmp/`, so the rename never crosses filesystems; a cached blob is verified again when read (cheap at these sizes) and fetched again if corrupt; a reference larger than 16 MiB is refused before downloading. The source is a trait (`BlobSource`), so tests use blobs in memory and `agent serve` the HTTP client.
- Storage: a Postgres `blobs` table in P0c (a project is a few KB of text); S3 later behind the same endpoint, which can redirect.
- The same "fetch by digest from a source, verify, cache" component will later serve input data (local disk now; S3 via presigned URLs, campus file servers) and reference genomes, where the real savings are: a reference index used by a thousand jobs on one agent is downloaded once. Data still never passes through the control plane.
- Batch lookups ("which of these do I lack?") can come later, like Bazel's `FindMissingBlobs` / `BatchReadBlobs`; one GET per blob is enough to start.
- `protocol` gains the blob and tree types and a shared "hash these bytes" function (so it depends on `sha2`); the API builds trees from the compiler's `NextflowProject`, the agent verifies them.

*Outputs stay on the agent's machine in P0c.* `result.json` records where each file is, including which agent or machine holds it; uploading to the workspace's storage (customer storage or S3) comes later. Until then, a job that reads another job's outputs must run where those outputs are: the scheduler places it on the same agent, or the same machine when agents share a filesystem. With one agent this happens by itself, but it is an explicit rule, so a second agent can never lease a job whose inputs it can't reach. The same "move work to data" idea as §5.4.

*Versioning.* Agents and the control plane are upgraded at different times, so an old agent will read messages from a newer control plane, and the reverse. For messages between separately deployed programs (`job.json`, the lease, heartbeat and result messages, `result.json`):

1. Readers **ignore unknown fields**.
2. Fields are only ever **added, each with a default** (`#[serde(default)]`).
3. Fields are **never renamed or removed**, and their meaning never changes; to change one, add a new field and retire the old.
4. **`format_version`** is an integer, bumped only for breaking changes; a reader that gets a newer version than it understands refuses the message with a clear "upgrade the agent" error instead of misreading it.
5. **New enum variants** break old readers even when unknown fields are ignored. Per enum: an `Unknown` fallback (`#[serde(other)]`) where a value is informational, such as task statuses; a `format_version` bump where the reader must act on it (job statuses, heartbeat actions, hash algorithms, cardinalities, input sources).

   Semantic types get a fallback too (`SemanticType::Unknown`): formats are added whenever a node with a new kind of data arrives, and making each one a breaking change would force an agent upgrade for every new node. `Unknown` only ever comes from reading a newer document: the node library rejects it in specs, and `can_connect` never connects it.

The canvas and node specs stay strict (`deny_unknown_fields`): the canvas is untrusted input, specs are curated in this repo, and a typo in either should fail loudly. Implemented in `protocol` (P0c PR 1): `FormatVersion` refuses a newer version during deserialization, so every reader gets the check without calling anything; a missing field means version 1, not "current", so old files keep reading as what they are. `job.json` and `result.json` carry `format_version`.

### 4.2 Deployment tiers (sovereignty model)

Same UI, same nodes, same everything — a workspace differs only in *where its data plane lives*:

| Tier | Data plane | Who it's for |
|---|---|---|
| **Managed** | Platform-provisioned S3 prefix + platform-operated compute, isolated per workspace | Trials, teaching, small labs, non-sensitive data. Zero-config onboarding funnel. |
| **BYO cloud** | Customer's AWS/GCP account via cross-account role; platform provisions compute + buckets there | Institutions fine with cloud but requiring data under their own agreements (most of them) |
| **Sovereign** | On-prem storage + HPC via the runner/data-gateway agent; only metadata leaves campus | Clinical/GDPR-strict environments, HPC-invested institutions |

Byte access from the browser:

- Cloud data plane → short-lived **presigned URLs**; bytes flow storage→browser directly.
- On-prem → the agent serves file ranges through its outbound connection (relay), or exposes a campus-internal endpoint so bytes never leave the network for on-campus users.

**Metadata sensitivity caveat:** sample names can be identifying in clinical contexts. The sovereign tier should eventually support a "no sample-level metadata leaves campus" mode / client-side field encryption; decided early because it constrains control-plane search. Deferred past v1 (managed-only), but the schema keeps sample-level metadata in clearly separable columns.

---

### 4.3 Control-plane API

Designed 2026-11-16 for P0c; being built (the build order is at the end of this section). The `api` crate is where everything meets: it calls the `compiler` crate as an in-process library (validation, planning, code generation), uses the `protocol` types for agent messages, and owns Postgres. The compiler stays pure; the agent still never depends on it.

**Three kinds of caller, kept apart** (§8.3, "agents are not users"):

| Caller | Credential | Endpoints |
|---|---|---|
| The UI (a scientist's browser) | A user token: in P0c one **development token**; Clerk sessions from P1 | `/api/*` except `/api/agent/*` |
| An agent | An **agent token**, scoped to one compute environment | `/api/agent/*` only |
| Setup and operations | Server-side commands | None: agent tokens are created by a setup command (later an admin screen); the node library is loaded when the API starts |

Neither credential works on the other's endpoints. Agent calls are also scoped per request: a heartbeat or result is accepted only for a job leased to *that* agent with the current `attempt`, and `GET /api/agent/blobs/{digest}` serves only blobs referenced by jobs in that agent's compute environment, so a valid token can't read other projects' code by guessing digests. This matters most for on-prem agents, which run on machines we don't control.

**P0c endpoints.** The minimum for an end-to-end run, shaped for the frontend so they need no redesign later. In P0c the UI endpoints are called by `curl`, scripts and the end-to-end test.

| Endpoint | Caller | Purpose |
|---|---|---|
| `POST /api/validate { canvas }` | UI | Validate without storing: autosave and live error highlighting |
| `POST /api/pipelines { name, canvas }` | UI | Validate and store an **immutable** pipeline version |
| `POST /api/datasets` | UI | Register samples and their files (below) |
| `POST /api/runs { pipeline_version_id, compute_env, inputs }` | UI | Launch a run; `inputs` maps each run input (`fastqc_1__reads`) to a dataset |
| `GET /api/runs/{id}` | UI | Run state, jobs, task summaries, outputs with digests |
| `POST /api/runs/{id}/cancel` | UI | Cancel (§6) |
| `POST /api/agent/lease` | Agent | Poll for work (§4.1) |
| `GET /api/agent/blobs/{digest}` | Agent | Fetch a project file it doesn't have cached |
| `POST /api/agent/jobs/{id}/heartbeat` | Agent | Renew the lease; the reply may say "cancel" |
| `POST /api/agent/jobs/{id}/result` | Agent | Report the `JobResult` |

The UI will also need read endpoints the walking skeleton doesn't: the node palette (`GET /api/node-specs`), lists of pipelines, datasets and runs, reopening a saved pipeline, run logs, result downloads. They belong to the frontend API design that follows P0c, together with an OpenAPI description and a generated TypeScript client (utoipa, §9); request and response types will already be Rust structs.

**Errors** use one JSON shape everywhere: `{ "error": { "code", "message", "details" } }`. Validation failures are `422` with every `ValidationError` in `details`, typed so the frontend can mark each broken node (`ValidationError` gains `Serialize`).

As built (PR 4), each entry of `details` is the error's own fields with its kind in `code` and a readable `message` beside them: `{ "code": "unknown_spec", "node": "fastqc_1", "spec": "fastqc", "version": "9.9.9", "message": "…" }`. Serde calls this an *internally tagged* enum; it needs every variant to have named fields, so `Cycle(String)` and the other one-value variants became `Cycle { node }`. Launch problems (an unknown compute environment, a missing input, a dataset on the wrong agent) use the same shape under `invalid_launch`. A body that doesn't parse (a misspelled canvas field) is `invalid_request` before validation starts.

**Launching a run** (`POST /api/runs`):

1. Load the pipeline version's canvas (immutable) and the specs it uses.
2. Validate (`validate(&canvas, &registry)`); on failure respond `422` with every error.
3. Plan (`ExecutionPlan::build`); refuse the launch if the plan contains an analysis job (§6).
4. For each job, generate its project (`nextflow_project`), store each file as a blob unless already stored (module blobs usually are), and store the tree (§4.1).
5. Resolve the first jobs' inputs: each `from: run` input maps to the dataset named in the request, giving file URIs, digests and `pinned_agent` from where the files live.
6. In one transaction: insert the run, all its jobs and their dependencies, and the resolved inputs; jobs without dependencies are `queued`, the rest `blocked`.
7. Respond `201 { run_id }`; an agent picks the run up on its next poll.

Compilation happens synchronously inside the request: the compiler takes milliseconds and does no I/O, and it keeps development simple. It can move to a background step if pipelines ever grow large enough to matter.

**The node library at startup.** The API loads `nodes/` (`load_library`) and writes each spec into `node_specs` and each module into `blobs`, in one transaction. Immutability is enforced here, not just by convention: each row records the digest of the spec as the API serializes it (so reformatting `spec.json` isn't a change) and of its module, and if `fastqc/0.1.0` already exists with different content, startup fails ("published versions never change, so publish the change as a new version") and nothing is written.

**Datasets and input digests.** A digest can only be computed by something that reads the bytes, and the control plane never does (§4).

- **P0c (a stand-in):** a script computes digests with `sha256sum` and calls `POST /api/datasets` with the development token, naming the agent whose disk holds the files. It exercises the same tables and launch path as the real flow.

  ```
  POST /api/datasets { name, location: { agent_id }, samples: [
    { sample: "gut", files: [ { uri: "/data/ggal/ggal_gut_1.fq", digest: "sha256:…", size: 691873 }, … ] } ] }
  ```

- **P1, with the samplesheet builder (§3.5): the inventory flow.** A browser can't read a lab server's disk, so the agent there does the reading:
  1. The scientist picks a compute environment and a folder on it.
  2. The API creates an **inventory request**, a second kind of agent work next to running jobs, delivered through the same lease (so a scan whose agent dies is retried like a job).
  3. The agent lists the folder, hashes each file with the same code it uses for outputs, reports progress in its heartbeats ("hashing 14 of 48 files"), and returns `{path, size, digest}` for each.
  4. The API stores each file as a FileRecord (§7) with its digest and the agent that holds it.
  5. The samplesheet builder shows the files: R1/R2 pairs detected, sample names extracted, an editable grid for conditions and replicates, and warnings (a missing mate, a condition with one replicate).
  6. "Create dataset" sends samples that point to **FileRecord IDs, never raw paths**: every file in a dataset was found and hashed by an agent, so paths can't be mistyped and digests can't be wrong.

  Safeguards: agents list only **data roots** configured by the sysadmin at install (e.g. `/data/sequencing`), with paths fully resolved before the check, so `..` or a symbolic link can't escape; without this, any UI user could browse the server through the agent. Hashing costs roughly 30–60 s per 30 GB file per core, so the agent hashes in parallel and remembers digests per (path, size, modification time, inode), like git's index or Bazel's file digest cache, re-checking a file's digest when a run actually uses it.

- **Managed tier: uploads.** A lab without a server uploads from the browser straight to the workspace's S3 bucket through short-lived upload URLs, so the bytes still bypass the API; an agent hashes each file on first use.

**As built (PR 4).** Decisions made while building the UI endpoints:

- *The development token* comes from `JESSIE_DEV_TOKEN` (or `--dev-token`) at startup, at least 16 characters. The API keeps only its SHA-256 and compares digests, so the time a comparison takes says nothing about how much of a guessed token was right. Without one, the UI endpoints refuse everyone and the server serves only agents. Agent tokens don't open UI endpoints and the development token doesn't open agent endpoints: each has its own extractor (`users::User`, `agents::Agent`), and a handler names the one it needs.
- *The compiler's registry is read back from the database* at startup (`node_library::load_registry`), right after the node library is published, rather than built from `nodes/` directly: a run always compiles against exactly what was published.
- *Pipelines are named*; saving a canvas under a name that exists adds the next version (1, 2, 3, …). Names are unique per project (`NULLS NOT DISTINCT`, so the single NULL project of P0c counts as one). Only valid canvases are stored. `POST /api/pipelines` answers `201 { pipeline_id, pipeline_version_id, version }`.
- *Datasets* are checked as a whole, every problem reported: a name, at least one sample, each with files, absolute paths (P0c files are on an agent's disk), an existing agent. Sample names are limited to letters, digits, `_`, `-` and `.`, starting with a letter or digit, because they become directory names on the agent's machine (`results/<output>/<sample>/`): `../../x` must never get that far. `scripts/register-dataset.sh` is the stand-in client.
- *Launch checks more than the design listed*, all before anything is written, and reports every problem at once: the compute environment exists; every run input has a dataset and nothing else does; each dataset exists and is on an agent of that compute environment; paired reads have two files per sample and single-end reads one (found at launch it is the request that is wrong, found on the agent it would be a failed job); and a job's datasets are on one agent, which becomes its `pinned_agent`. Storing the projects' blobs is part of the launch transaction, so a refused or failed launch leaves no orphaned rows. Which dataset fed which input is recorded in `run_inputs`, for provenance.
- *Run status* (`GET /api/runs/{id}`) returns the run's state and times, its pipeline version, compute environment and inputs, and per job: state, attempt, agent, `cancel_requested` (the job is stopping), progress, failure reason, tasks (name, status, exit code, duration), warnings, and output files with digests and the agent holding them. Timestamps are RFC 3339 in UTC, formatted by Postgres.
- *Cancel* answers `200 { run_id, state: "cancelled" }`: the run is cancelled at once, while running jobs stop through their next heartbeat. A run that already finished is `409 run_finished`; an unknown run `404`.

**Kept out of P0c on purpose:** user accounts, projects and roles (P1; tables carry `project_id` from the start so adding them later reshapes nothing), the UI read endpoints and OpenAPI client (frontend API design), the inventory flow and samplesheet builder (P1).

**End-to-end test in CI:** GitHub Actions starts Postgres as a service, the `api`, and an agent serving in a loop; the test launches the test canvas through the API, waits for `succeeded`, and checks the output digests. It uses **stub runs**: pulling the 1–3 GB images on every CI run would be slow. Real Docker runs remain a local check before merging.

**Build order** (one PR each, against `main`):

1. **Built.** `protocol`: lease, heartbeat and result messages (`lease.rs`); blob and tree types (`blob.rs`) with the hasher moved into `protocol` so the API and agent share it; the versioning rules (`version.rs`), with `format_version` on `job.json` and `result.json`.
2. **Built.** The `api` crate skeleton: axum, sqlx, migrations (embedded in the binary), configuration from flags or environment variables, `GET /healthz` (which also checks the database), graceful shutdown on Ctrl-C or SIGTERM, and loading the node library into Postgres with immutability enforced. Queries are runtime-checked (`sqlx::query`) for now: sqlx's compile-time checking (`query!`) needs a database or saved query metadata at build time, which can be added once there are enough queries to justify it. sqlx 0.9 accepts only `&'static str` SQL unless explicitly marked reviewed, so queries can't be assembled from runtime strings.
3. **Built.** The queue module (`queue.rs`: `enqueue_run`, `acquire`, `heartbeat`, `complete`, `reap`, `cancel_run`), the four agent endpoints (`agent_api.rs`), agent tokens (`agents.rs`), and the reaper, a tokio task every 10 s. Tested against real Postgres (`#[sqlx::test]`), including 30 agents racing for 20 jobs and two results arriving at once. Agent tokens: `api create-compute-env` and `api create-agent` (which prints the token once); the database stores only the token's SHA-256, and tokens start with `jat_` so a leaked one is easy to spot. Launch isn't built yet, so `enqueue_run` is called only by tests until PR 4.
4. **Built**, as four smaller PRs: (1) the development token (`users.rs`) and validate; (2) pipelines (`pipelines.rs`) and datasets (`datasets.rs`); (3) launch (`runs.rs`); (4) run status and cancel. Decisions made while building are above ("As built (PR 4)"). Tested against real Postgres, including launch → lease → result → status through `queue::acquire` and `queue::complete`; against a running server with `curl`, the project an agent fetches by digest was byte-identical to what the `compiler` CLI writes.
5. `agent serve`, as three PRs: (1) **built**: the blob cache (`blobs.rs`) and writing a lease to disk (`job_dir.rs`), tested with blobs in memory and the unchanged staging; (2) **built**: the HTTP client (`client.rs`, reqwest with rustls and the system proxy settings), tested against a fake control plane in axum and checked against the real API; (3) the polling loop with backoff, the heartbeat task, and posting results.
6. The end-to-end test in CI.

## 5. Execution Model

### 5.1 Two execution classes

| | Batch nodes | Analysis nodes |
|---|---|---|
| Examples | Alignment, quantification, variant calling | DE test, filtering, plots, tables |
| Execution | Compiled into generated Nextflow workflow; scheduled on cluster/cloud | Agent runs container directly on a warm executor |
| Latency | Minutes–hours (progress bar) | Target < 30 s from "Apply" to new plot |

Same node spec, same cache, same provenance — a `class` field routes execution. Scientists never see the distinction.

### 5.2 Content-addressed caching

Every node execution is memoized by **cache key = hash(node id + version, resolved parameters, input artifact hashes)**. At dispatch, the scheduler skips any node whose key has a cached artifact and wires cached outputs downstream. One mechanism, three features:

1. **Durable resume** — failed runs relaunch and skip prior successes; survives across runs, machines, and users (stronger than Nextflow `-resume`).
2. **Interactive analysis** — changing a plot threshold changes only that node's and descendants' keys; the 6-hour alignment is untouched. Enables the pre-launch "what will re-run" preview.
3. **Cross-user dedup** — same data + same params in a workspace = one execution. Real cost savings at facility scale.

**Required discipline: immutability.** Node versions pinned per pipeline version; containers pinned by digest; input files content-hashed at registration. This same discipline underwrites provenance.

### 5.3 Reference data

- Reference-consuming nodes expose a **`Genome` dropdown** ("Human GRCh38, Ensembl 110") — never a file input.
- Behind it: a platform-curated **reference registry** — versioned, content-hashed entries (FASTA, GTF, prebuilt indexes) in a platform bucket. In the compiler they resolve to ordinary typed inputs, so provenance and caching cover them automatically; index building is itself a memoized node (first user pays, everyone else hits cache).
- Per-workspace custom references (unusual organisms/annotations) use the same registry, project-scoped. v1 ships human + mouse.

### 5.4 Multi-machine and multi-environment execution

Three distinct levels, adopted in this order:

1. **Many machines, one environment (native Nextflow).** A single Nextflow head process submits each task as its own job to Slurm / AWS Batch / Kubernetes — 24 samples ⇒ up to 24 machines. A config change (executor + shared work dir: S3 on Batch, shared filesystem on HPC), not an architectural one. MVP uses the local executor (one EC2 box); AWS Batch is the P2 upgrade.
2. **Whole-run placement across environments (burst).** The scheduler sends an entire run to whichever eligible environment has capacity ("HPC queue is 3 days deep → run in cloud"). One placement decision per run; subject to the per-environment project allowlist.
3. **Segment placement across environments.** Different segments of one run execute in different environments, with the control plane coordinating artifact transfer at boundaries. Governed by the **"data flows downhill"** rule: a boundary may cross environments only when the crossing artifacts are small derived data (e.g. a count matrix — megabytes), never raw data (FASTQ/BAM — tens to hundreds of GB), and only when sovereignty policy permits. Canonical case: align on campus HPC, analyze in cloud; only the count matrix leaves campus.

---

## 6. Job Queue & Dispatch

**Postgres-backed queue** in the control-plane database — no Redis, no Celery, no separate broker. A `jobs` table claimed with `SELECT … FOR UPDATE SKIP LOCKED`, plus lease/heartbeat/expiry columns, retry-with-backoff via `run_at`, a reaper for expired leases, and optional `LISTEN/NOTIFY` for low-latency pickup. Lease semantics (fencing by `attempt`, the database clock, at-least-once delivery) are in §4.1, *Agent protocol*. The three core statements:

```sql
-- acquire: claim one queued job atomically; concurrent agents skip locked rows
UPDATE jobs SET state = 'leased', agent_id = $1, attempt = attempt + 1,
                lease_expires_at = now() + interval '60 seconds'
WHERE id = (SELECT id FROM jobs WHERE state = 'queued' AND compute_env = $2
            ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1)
RETURNING id, attempt;

-- heartbeat: renew only if this agent still holds this attempt (0 rows: lease lost)
UPDATE jobs SET lease_expires_at = now() + interval '60 seconds'
WHERE id = $1 AND attempt = $2 AND state = 'leased';

-- reaper: expired leases go back to the queue (or fail after the last attempt)
UPDATE jobs SET state = 'queued' WHERE state = 'leased' AND lease_expires_at < now();
```

The acquire query also filters on `run_at <= now()` (retry backoff) and on placement (below).

Why:

- **One fewer stateful system** for a solo operator; rides Postgres backups/HA.
- **Transactional enqueue:** creating a Run and its dispatch job commit atomically; marking a Task done and enqueuing downstream nodes is atomic. Eliminates the dual-write bug class entirely — significant correctness-for-free in a DAG scheduler.
- **Queryable:** the queue is SQL, joinable against runs/tasks for dashboards and debugging.
- **Scale headroom:** see *Why Postgres, and when to revisit* below.

Two consumer types:

- **Control-plane workers** (tokio tasks in the API process) consume directly: DAG scheduling steps, cache-key resolution, notifications, cleanup.
- **Runner agents never touch Postgres.** They call `POST /api/agent/lease`; that endpoint's implementation is the SKIP LOCKED query filtered to the agent's compute environment. Table lease semantics map 1:1 onto the agent protocol.

Rules: never hold a queue transaction open during work (claim → commit → work → update); a long Nextflow execution is **not** a held job — the job is *dispatch & monitor*; the agent owns the hours-long run and reports state transitions.

Implementation: hand-rolled with sqlx (~200 lines), so the queue and the agent API share one model and one set of types. Lease semantics are load-bearing enough that owning this code beats adopting a job-queue crate. All queue SQL lives in one module of the API crate (`acquire`, `heartbeat`, `complete`, `reap`, `cancel`); nothing else writes the `jobs` table.

**Job states** (decided 2026-11-16; built in P0c PR 3). A job is one entry of an `ExecutionPlan`: a segment or an analysis node.

| State | Meaning | Leaves by |
|---|---|---|
| `blocked` | Waiting for the jobs it depends on | → `queued` when they have all succeeded; → `skipped` if one failed |
| `queued` | Ready: inputs resolved, waiting for an agent | → `leased` when an agent acquires it; → `cancelled` |
| `leased` | An agent holds the lease and is running it | → a final state when its result arrives; → `queued` when the lease expires or the result is `interrupted` (attempts left); → `failed` (no attempts left) |
| `succeeded`, `failed`, `cancelled`, `skipped` | Final | — |

Every transition is a conditional update (`UPDATE … WHERE id = $1 AND state = 'leased' AND attempt = $2`): zero rows means another actor (an agent, the reaper, a cancel) moved the job first. Compare-and-set, the same idea as fencing, on every change.

**Who moves jobs.** There is no separate scheduler process: each transition happens inside the transaction that causes it.

1. **Launch** (one transaction): insert the run and all its jobs; jobs without dependencies start `queued` with inputs resolved from the dataset, the rest `blocked`.
2. **Acquire and heartbeat**: the agent protocol (§4.1).
3. **A result** (one transaction): record the job's final state and its outputs; for each dependent job whose dependencies have now all succeeded, resolve its inputs from the stored outputs and move it to `queued`. Either all of it happens or none, so "job done but nothing downstream started" can't occur.
4. **The reaper**: a tokio task in the API process, every ~10 s, handling expired leases. Safe with several API replicas, because it claims rows with `SKIP LOCKED` too.
5. **Cancel** (the scientist): `blocked` and `queued` jobs → `cancelled` at once; `leased` jobs get `cancel_requested`, the next heartbeat reply says "cancel", and the agent's result moves the job to `cancelled`. If that agent is dead, the reaper sees the flag on the expired lease and cancels rather than requeues.

Inputs are resolved when a job becomes `queued`, not when it is leased: the lease response is then a plain read, identical across retries. A blocked job's `resolved_inputs` already holds its run inputs (from launch); queueing adds its `from: job` inputs, read from `job_outputs` in the order the agent reported them (R1 before R2).

**One run, one writer at a time** (found while building PR 3). Every transaction that changes a run's jobs (a result, the reaper, cancel) first locks the run's row (`SELECT … FOR UPDATE`), always run before job, so they can't deadlock. Without it, when the last two jobs of a run finish at the same moment, each transaction sees the other job as unfinished (neither has committed yet), so neither queues the job that needs both, and neither marks the run succeeded: the run hangs. Postgres's default isolation, READ COMMITTED, gives each statement a fresh snapshot, so once the lock is held, the next statement sees what the previous holder committed. A test runs this race and fails if the lock is removed. Acquire and heartbeat change one job each and don't take the run lock, so the hot path never waits for it.

**Retry policy.** Retry what the infrastructure broke; never retry what will fail the same way again.

| Outcome | Retry? |
|---|---|
| Lease expired (agent crashed, network partition) | Yes, after a backoff |
| `interrupted` (agent shut down) | Yes, immediately |
| `failed` with a failed task (the tool rejected its input) | No: show the scientist the tool's error |
| `failed` with no failed task (Nextflow or the pipeline errored) | No, for now: retrying could hide pipeline bugs |
| `timed_out` | No: the limit was a decision |

At most 3 attempts (`max_attempts`); `interrupted` counts as an attempt, so an agent that is restarted over and over can't hold a job forever. Backoff via `run_at` (+30 s, then +2 min). After the last attempt the job is `failed` ("the agent was lost 3 times"), so a job that crashes agents can't take them all down in turn. An agent runs preflight before it leases anything, so a broken host never takes leases it can only fail.

**Run state** is stored on the run and updated in the same transaction as the job change behind it, not computed from jobs on every read (run lists are the most-viewed screen): `running` while any job is unfinished; `succeeded` when all jobs succeeded; `failed` as soon as one job fails, with every job downstream of it, directly or not, `skipped` while unrelated branches keep running; `cancelled` when the scientist cancels.

**Placement.** While outputs stay on agents' disks (§4.1), resolving a job's inputs also fixes where it can run: if any input lives on agent A, the job gets `pinned_agent = A`, and acquire adds `AND (pinned_agent IS NULL OR pinned_agent = $agent_id)`. If A is gone for good, its outputs went with it: a pinned job left `queued` too long fails ("the agent holding this job's inputs is gone"), and only rerunning the upstream job helps (not built yet: §12). If a job's inputs are on two different agents, no agent can run it, so it fails when it would be queued. Uploading outputs to shared storage removes both limits.

**Analysis jobs** are rejected when a run is launched ("analysis jobs aren't supported yet"), as parameters are rejected at validation, rather than queued forever with no agent able to run them.

**Tables** (as built in `migrations/0002_queue.sql`; this is the outline):

```
runs             (id, project_id, pipeline_version_id, state, created_at, finished_at)
jobs             (id, run_id, job_index, kind,
                  state, attempt, max_attempts, run_at,           -- queue
                  agent_id, lease_expires_at, cancel_requested,   -- lease
                  compute_env, pinned_agent,                      -- placement
                  project_tree, interface, resolved_inputs,       -- what the lease response sends
                  result, failure_reason)                         -- outcome
job_dependencies (job_id, depends_on_job_id)
job_outputs      (job_id, name, position, sample, uri, digest, size, agent_id)   -- becomes FileRecords (§7)
compute_envs     (id, nextflow_version, hash_algorithm, time_limit_secs)  -- the lease's settings
agents           (id, name, compute_env, token_digest, agent_version, last_seen_at)
tree_entries     (tree_digest, path, blob_digest)                       -- which blobs a project uses
```

Constraints make some bad states impossible to write: a `leased` job always has an agent and an expiry, and no other job has an expiry; an output file can be recorded only once (`job_outputs`' primary key is `(job_id, name, position)`); a compute environment can only name a hash algorithm the code knows. Partial indexes cover the two queries that run constantly: queued jobs per environment, and leased jobs by expiry.

**Why Postgres, and when to revisit.** The queue stays small because a job is a whole segment, minutes to hours long; per-sample tasks are scheduled by Nextflow inside it. Even at 10,000 runs a day × 5 jobs, that is about 0.6 jobs per second; the heaviest load is heartbeats, about 500 single-row updates per second with 10,000 jobs running at once. A well-provisioned Postgres commonly sustains thousands to tens of thousands of such updates per second. Airflow runs several schedulers on this same `SKIP LOCKED` pattern, and Temporal can run on Postgres: workflow state is a database problem first.

A distributed queue (SQS, Pub/Sub, Kafka) would cost more than it gains: enqueueing would no longer be transactional with the state change (needing an outbox table, i.e. a queue in Postgres anyway); leases and fencing would still live in Postgres (SQS's visibility timeout has no fencing token; Pub/Sub's ack deadline caps at 10 minutes, against jobs of hours); Kafka is a log, not a work queue; and it is one more system to run.

Scaling steps, in order:

1. **Tuning**: a partial index on `state = 'queued'`; cheaper heartbeats (every Postgres `UPDATE` writes a new row version for vacuum to clean up, so: one heartbeat per agent covering all its jobs, longer intervals for long-running jobs, lease columns in a narrow table of their own); long polling; moving finished jobs out (time partitions or an archive table).
2. **Bigger hardware and read replicas** for dashboards and run lists.
3. **Sharding** by workspace or compute environment, a boundary the design already has (a sovereign institution may run its own control plane).
4. **A dedicated queue system**, only on these signals: thousands of state changes per second sustained, a control plane active in several regions at once, or lock waits on the queue table in profiles. Keeping all queue SQL in one module keeps that replacement contained.

---

## 7. Data Model (control plane, Postgres)

```
Workspace ─┬─ Members (identity, workspace role)
           ├─ ComputeEnvironments (runner registrations, data-plane config, agent tokens)
           ├─ NodeRegistry (NodeSpec versions: ports, param schema, container digest, class)
           ├─ ReferenceRegistry (genomes, annotations, prebuilt indexes)
           └─ Projects
                ├─ ProjectMembers (user, project role)
                ├─ Datasets ── Samples ── FileRecords (uri, size, digest, semantic type)
                ├─ SampleSheets
                ├─ Pipelines ── PipelineVersions (canvas graph JSON; immutable once run)
                └─ Runs
                     ├─ binding: PipelineVersion + Dataset(s) + param overrides + ComputeEnv
                     ├─ Tasks (per node execution: state, logs ptr, metrics, cache key)
                     └─ Artifacts (output FileRecords + ReportArtifacts: Vega-Lite spec + data)
```

- **Provenance is foreign keys:** `Artifact → Task → Run → input FileRecords → Samples`. No separate lineage system.
- **FileRecords store a URI into the workspace data plane, never bytes** — the sovereignty boundary made concrete in the schema. The digest is self-describing (`sha256:<hex>`, §4.1), computed by the agent, the only component that touches bytes.
- Audit events: append-only table (who launched/deleted/changed membership, when).

---

## 8. AuthN / AuthZ

### 8.1 Roles — two levels, fixed

**Workspace roles:** Owner (everything incl. workspace deletion) · Admin (members, compute environments, node registry; sees all projects) · Member (sees only projects they're added to).

**Project roles:** Maintainer (membership, deletion, everything) · Editor (upload, build, launch) · Viewer (read-only, incl. reports & provenance).

Persona mapping: facility staff → Admin; PI → Maintainer; bench scientist → Editor; external collaborator → Member + Viewer on one project.

Decisions:

- **The project is the sole authorization boundary for data.** Everything inside inherits its ACL; no per-dataset/per-file permissions ("more sensitive" ⇒ its own project). This single rule avoids the per-object-ACL rabbit hole.
- **Any Member can create projects** (creator becomes Maintainer) — self-serve, noob-friendly by default. A workspace setting can restrict creation to Admins for facilities wanting gated intake.
- **No public share links in v1.**
- Pipelines are project-scoped; a workspace-level **published-templates shelf** (Admin-curated) is how facility bioinformaticians distribute validated pipelines — the adoption wedge, concretely.
- Fixed roles only; no custom role builder. Teams/IdP-group mapping deferred to SSO phase.

### 8.2 Enforcement — deliberately simple

No policy engine. (1) A static permission matrix in code (`{"run.launch": {Editor, Maintainer}, …}`) — unit-testable, greppable, self-documenting. (2) An axum extractor / `require(project_id, permission)` guard resolving effective role (Admin/Owner short-circuit). (3) **List endpoints always filter via membership joins** — object-level checks on by-id GETs *and* joins on lists; IDOR via guessed run IDs is the classic bug class here.

**Presigned URLs** are bearer tokens: authorize at minting, ~5-minute TTL, never logged.

### 8.3 Agents are not users

Compute-environment-scoped tokens; separate `/api/agent/*` surface limited to: lease jobs for own environment, report task state, upload artifacts for assigned runs. Agents can never browse project data — essential once agents run on-prem outside the trust boundary. Schema includes an optional per-environment project allowlist ("clinical projects only run on-prem"); UI for it deferred. Concrete per-request scoping (heartbeats and results only for the agent's own lease and attempt, blobs only for its environment's jobs) is in §4.3.

---

## 9. Technology Stack

| Layer | Choice | Rationale |
|---|---|---|
| Frontend | Next.js, React Flow, shadcn/ui + Tailwind, TanStack Query | React Flow = canvas mechanics solved |
| Plots | Vega-Lite specs rendered client-side | Interactive + paper-grade export, mature spec format |
| Backend language | **Rust 1.98** (pinned in `rust-toolchain.toml`), one Cargo workspace: `compiler`, `agent`, `protocol` (built); `api` (P0c) | Compiler must live in the control plane; one language means no seam through it. Node science is containerized, so platform language is independent of the bioinformatics ecosystem. |
| API | axum + sqlx (compile-time-checked SQL, sqlx migrations), utoipa → OpenAPI → generated TS client | Mainstream Rust web stack; typed end to end |
| Compiler | Rust crate: serde (canvas JSON), petgraph (DAG, segmentation, descendants), minijinja (DSL2 codegen), clap (`compiler` CLI) | Exhaustive `match` over the semantic type system |
| Protocol | Rust crate shared by compiler, agent and API: serde, csv, sha2. Job interfaces (`job.json`), manifests, digests and the shared hasher, job results (`result.json`), port types, the agent messages (lease, heartbeat, result), blobs and trees, format versions | Formats that cross process boundaries are defined once, so both sides agree at compile time; versioned per §4.1 |
| DB | Managed Postgres (Neon or RDS) | Single stateful system: app data + queue + provenance |
| Queue | Postgres SKIP LOCKED (§6), workers as tokio tasks | No broker to operate |
| Auth | Clerk (or Cognito); WorkOS for SSO when demanded | Buy, don't build |
| Agent | Rust binary: tokio (process supervision), nix (signals), sha2 (hashing), clap; containers via Docker; JVM + Nextflow on batch hosts (§4.1). Depends on `protocol`, never on `compiler` | Drop-in install on HPC head nodes; shares protocol types with the API |
| Testing / CI | Built: cargo test (unit + CLI integration tests), expected-output files for codegen, `nextflow lint` on the node library and on a generated project, and `scripts/stub-run.sh` (compile → lint → `agent run --stub`) in GitHub Actions, with Rust and Nextflow versions pinned. P0c adds testcontainers (real Postgres) and an E2E run | |
| Workflow engine | Nextflow DSL2 (generated) | Ecosystem, executor portability, nf-core modules |
| MVP compute | One EC2 instance: agent + Nextflow local executor + Docker | Exercises the full architecture with near-zero infra; AWS Batch is a config upgrade later |
| Object storage | S3, per-workspace prefixes, presigned URLs | |

Guiding principle: **buy everything that isn't the product.** The product is the canvas-compiler, node library, samplesheet UX, and results experience.

---

## 10. Roadmap

**P0 — walking skeleton, built inside-out.** Goal: a hardcoded 2-node canvas runs end-to-end through the real spine (API → queue → agent lease → Nextflow → artifact upload → hash registration → viewable result). Ordered so the Rust learning curve rises gradually (easiest crate first):

- **P0a — compiler core. Done.** Pure Rust, no async/DB: canvas JSON → validated `PipelineGraph` → `ExecutionPlan` (convex segments) → job interfaces → Nextflow projects, plus the `compiler` CLI and the node library. Expected-output tests; generated code linted and stub-run by Nextflow in CI.
- **P0b — agent, local mode. Done for batch jobs.** `agent check` / `stage` / `run`: preflight, staging, supervised Nextflow run with clean stopping, trace parsing, SHA-256 output hashes, output manifests and `result.json`. Verified with a real Docker run on the nextflow-io/rnaseq-nf test reads. A plain Linux process that runs a compiled segment with Nextflow's **local executor** + Docker, parses the trace file, hashes and stores outputs. No server. Kubernetes is deliberately not used here: where tasks run is a Nextflow executor setting (§5.4 level 1), so `k8s`, like Slurm and AWS Batch, is a later config option, not part of the agent's architecture.
- **P0c — control plane. In progress** (PRs 1–4 of the build order in §4.3 are done). Designed: the agent protocol (leases, messages, project files by digest, local outputs, versioning) is decided in §4.1, the queue (job and run states, transitions, retries, placement, why Postgres) in §6, and the control-plane API in §4.3. The design walkthrough is complete; building starts with the `protocol` messages (§4.3, build order). axum + sqlx + SKIP LOCKED queue; agent switches from local mode to leasing over HTTPS. One GitHub Actions E2E test: Postgres service container + real tiny Nextflow run.

**P1 — MVP: RNA-seq vertical slice, managed tier only.**

- Canvas with typed ports; stage-level palette of ~12 nodes: FastQC, fastp, Salmon quant, MultiQC (batch); DESeq2, filter, PCA, volcano, heatmap, results table, GSEA (analysis)
- "Standard RNA-seq" template; samplesheet builder; compiler → DSL2; export-as-Nextflow
- Run monitor; content-addressed cache with pre-launch re-run preview
- Projects/datasets/samples/files; report view with provenance links
- RBAC per §8; audit log; reference registry with human + mouse

**P2 — hybrid & sovereignty.** Installable agent packaging, BYO-cloud provisioning, sovereign-tier data gateway, AWS Batch/Slurm executors (multi-machine level 1), whole-run placement across environments (level 2), SSO (WorkOS), metadata-sensitivity mode design. Segment placement across environments (level 3, "data flows downhill") at the end of P2 or in P3.

**P3 — ecosystem.** Node-authoring tooling + registry for institutional cores, tool-level palette, second domain (variant calling — proves the type system generalizes), template sharing.

**Explicitly deferred:** conditionals/loops, hosted notebooks, public share links, custom roles, per-dataset ACLs, quotas/billing/cost controls, node-authoring UI.

---

## 11. Risks

| Risk | Mitigation |
|---|---|
| Learning Rust from zero while building solo | Inside-out P0 (compiler first: pure logic, no async/DB); clone freely, owned types, `anyhow`, avoid lifetimes/generics early; Rust Book ch. 1–10 before starting |
| Node library is a grind for a solo builder (containers, schemas, scientist-grade help text, validation on real data) | 12 excellent nodes > 40 mediocre; one domain first; lean on nf-core modules |
| Compiler correctness (canvas → DSL2 edge cases) | Static DAGs only; golden-file tests compiling canvases to expected DSL2; export feature doubles as inspection |
| Cache poisoning / staleness | Strict immutability (digests, content hashes); cache keyed on everything that affects output |
| Bioinformaticians reject a no-code tool | Export-as-Nextflow escape hatch; make them node authors, not gatekeepers |
| Sovereignty promises vs. metadata leakage | Metadata-only control plane rule enforced in schema; sample-metadata sensitivity mode on P2 roadmap |
| Interactive latency misses the <30 s bar | Analysis class bypasses Nextflow entirely; warm executor pool; small data artifacts |
| Large container images slow first runs (FastQC 1.1 GB, MultiQC 1.8 GB; most of a first run's minute was the pull) | Pin by digest so cached images stay valid; pre-pull library images on agents; consider slimmer images per node |

---

## 12. Open Questions (parked)

- Cost controls / quotas / billing model
- Hosted notebooks (post-P3 candidate)
- Metadata field-level encryption design for sovereign tier
- Multi-cloud (GCP) support order
- Node marketplace / cross-workspace template sharing
- Parameter design: a schema per node spec, how codegen passes values to modules, and how they enter the cache key. Until then validation rejects parameters.
- The frontend API design after P0c: read endpoints for the UI, an OpenAPI description and a generated TypeScript client (§4.3).
- The inventory flow and samplesheet builder back end (P1, designed in §4.3).
- Upload of outputs to workspace storage (customer storage or S3), which lifts the rule that dependent jobs run where their inputs are.
- (Decided 2026-11-13: protocol versioning, now in §4.1.)

**Known gaps** (from the 2026-11-11 review of P0a/P0b; none blocks P0c):

- Analysis jobs: planned and segmented, but neither compiled nor executed yet.
- Directory outputs (e.g. `multiqc_data/`) aren't collected; no current port is a directory.
- Per-sample `meta` carries only `id`; nf-core modules such as fastp and Salmon also need `single_end`, which the compiler knows from `Fastq { paired }`. Needed before those P1 nodes.
- The library loader doesn't check that process and port names are valid identifiers, or that a node's input and output port names differ (a shared name would give two pieces of data one name). Needed before third-party nodes (P3).
- Staging treats every manifest path as a local path, so `s3://` URIs would be mangled. Needed with S3 storage.
- When a run is stopped, the agent reports it about a second before Docker has removed the task container (§4.1).
- A job pinned to an agent that is gone for good stays `queued` forever; it should fail after a while (§6, Placement).
- Datasets carry no semantic type, so launch can't check that a dataset fits the input it feeds beyond the file count of FASTQ inputs. FileRecords with types come with the inventory flow (P1).
- At launch a dataset's samples are ordered by name, not in the order they were registered.
- The control plane polls: no `LISTEN/NOTIFY` long polling yet (§4.1). Agents' `last_seen_at` is recorded on each lease poll but nothing reads it yet (a "which agents are alive" view belongs to the UI).
