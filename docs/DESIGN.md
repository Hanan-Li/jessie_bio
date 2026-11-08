# Jessie's Bioinformatics Platform — Design Document

**Status:** Draft v2 · **Date:** 2026-09-27 (v1: 2026-09-14) · **Author:** David (with design assistance from Claude)

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
- Compilation validates the full graph (types, required inputs, parameter constraints) before any compute is touched.
- Generated code targets Nextflow's **strict syntax** (25.04+) so output can be linted by Nextflow itself in CI.

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
- Tests compare generated projects byte-for-byte against expected files in the repo (regenerated with `UPDATE_EXPECTED=1`), and CI runs `nextflow lint` on them.
- Analysis-job code generation is deferred until the first analysis node exists.

### 3.4 Node specification

A node is a curated artifact comprising:

- Container image (pinned by digest)
- Wrapped nf-core module/subworkflow or custom script
- Typed port declarations (inputs/outputs with semantic types + cardinality)
- Parameter schema → auto-rendered form (defaults, units, ranges, scientist-grade help text)
- Resource hints (CPU/memory/disk classes)
- **Execution class:** `batch` or `analysis` (see §5)

**Node library.** The core library lives in the platform repo under `nodes/<id>/<version>/`:

```
nodes/fastqc/0.1.0/
  spec.json     NodeSpec: ports, execution class, parameters
  main.nf       Nextflow process (batch nodes)
  UPSTREAM.md   provenance of vendored code: repo, commit, path, license, local changes
```

- A version directory is immutable: any change (module code, container) is a new version.
- Where nf-core has a well-tested module, it is **vendored**: copied from `github.com/nf-core/modules` at a pinned commit (MIT-licensed; notice kept in `UPSTREAM.md`). Local changes are expected and recorded: containers pinned by digest instead of tag, strict-syntax fixes, unused inputs removed.
- Convention linking spec and module: input ports in declared order are the process's inputs; output port names are its `emit:` names.
- Node ids on a canvas must be valid Nextflow identifiers, since codegen uses them in process aliases, and may not contain `__`, which separates node from port in data names.

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

Agents authenticate with compute-environment-scoped tokens against a separate `/api/agent/*` surface (§8.3). The agent is a **Rust static binary** sharing a protocol-types crate with the API. The binary itself has no dependencies, but hosts running batch nodes also need a JVM + Nextflow (Nextflow is Groovy/Java, Java 17+). The agent either uses the site install (e.g. `module load nextflow` on HPC) or bootstraps and caches a pinned JRE + Nextflow version on first run — pinning the version per compute environment also serves reproducibility. Analysis-class nodes never touch the JVM.

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

**Postgres-backed queue** in the control-plane database — no Redis, no Celery, no separate broker. A `jobs` table claimed with `SELECT … FOR UPDATE SKIP LOCKED`, plus lease/heartbeat/expiry columns, retry-with-backoff via `run_at`, a reaper for expired leases, and optional `LISTEN/NOTIFY` for low-latency pickup.

Why:

- **One fewer stateful system** for a solo operator; rides Postgres backups/HA.
- **Transactional enqueue:** creating a Run and its dispatch job commit atomically; marking a Task done and enqueuing downstream nodes is atomic. Eliminates the dual-write bug class entirely — significant correctness-for-free in a DAG scheduler.
- **Queryable:** the queue is SQL, joinable against runs/tasks for dashboards and debugging.
- **Scale headroom:** pattern is comfortable at hundreds of jobs/sec; facility load is orders of magnitude below.

Two consumer types:

- **Control-plane workers** (tokio tasks in the API process) consume directly: DAG scheduling steps, cache-key resolution, notifications, cleanup.
- **Runner agents never touch Postgres.** They call `POST /api/agent/lease`; that endpoint's implementation is the SKIP LOCKED query filtered to the agent's compute environment. Table lease semantics map 1:1 onto the agent protocol.

Rules: never hold a queue transaction open during work (claim → commit → work → update); a long Nextflow execution is **not** a held job — the job is *dispatch & monitor*; the agent owns the hours-long run and reports state transitions.

Implementation: hand-rolled with sqlx (~200 lines), so the queue and the agent API share one model and one set of types. Lease semantics are load-bearing enough that owning this code beats adopting a job-queue crate.

---

## 7. Data Model (control plane, Postgres)

```
Workspace ─┬─ Members (identity, workspace role)
           ├─ ComputeEnvironments (runner registrations, data-plane config, agent tokens)
           ├─ NodeRegistry (NodeSpec versions: ports, param schema, container digest, class)
           ├─ ReferenceRegistry (genomes, annotations, prebuilt indexes)
           └─ Projects
                ├─ ProjectMembers (user, project role)
                ├─ Datasets ── Samples ── FileRecords (uri, size, checksum, semantic type)
                ├─ SampleSheets
                ├─ Pipelines ── PipelineVersions (canvas graph JSON; immutable once run)
                └─ Runs
                     ├─ binding: PipelineVersion + Dataset(s) + param overrides + ComputeEnv
                     ├─ Tasks (per node execution: state, logs ptr, metrics, cache key)
                     └─ Artifacts (output FileRecords + ReportArtifacts: Vega-Lite spec + data)
```

- **Provenance is foreign keys:** `Artifact → Task → Run → input FileRecords → Samples`. No separate lineage system.
- **FileRecords store a URI into the workspace data plane, never bytes** — the sovereignty boundary made concrete in the schema.
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

Compute-environment-scoped tokens; separate `/api/agent/*` surface limited to: lease jobs for own environment, report task state, upload artifacts for assigned runs. Agents can never browse project data — essential once agents run on-prem outside the trust boundary. Schema includes an optional per-environment project allowlist ("clinical projects only run on-prem"); UI for it deferred.

---

## 9. Technology Stack

| Layer | Choice | Rationale |
|---|---|---|
| Frontend | Next.js, React Flow, shadcn/ui + Tailwind, TanStack Query | React Flow = canvas mechanics solved |
| Plots | Vega-Lite specs rendered client-side | Interactive + paper-grade export, mature spec format |
| Backend language | **Rust**, one Cargo workspace (api, compiler, agent, shared protocol/types crates) | Compiler must live in the control plane; one language means no seam through it. Node science is containerized, so platform language is independent of the bioinformatics ecosystem. |
| API | axum + sqlx (compile-time-checked SQL, sqlx migrations), utoipa → OpenAPI → generated TS client | Mainstream Rust web stack; typed end to end |
| Compiler | Rust crate: serde (canvas JSON), petgraph (DAG, segmentation, descendants), minijinja (DSL2 codegen) | Exhaustive `match` over the semantic type system |
| DB | Managed Postgres (Neon or RDS) | Single stateful system: app data + queue + provenance |
| Queue | Postgres SKIP LOCKED (§6), workers as tokio tasks | No broker to operate |
| Auth | Clerk (or Cognito); WorkOS for SSO when demanded | Buy, don't build |
| Agent | Rust static binary (tokio); containers via Docker; JVM + Nextflow on batch hosts (§4.1) | Drop-in install on HPC head nodes; shares protocol types with the API |
| Testing / CI | cargo test, golden-file compiler tests, testcontainers (real Postgres), `nextflow lint` on generated code, GitHub Actions E2E with a real Nextflow run | |
| Workflow engine | Nextflow DSL2 (generated) | Ecosystem, executor portability, nf-core modules |
| MVP compute | One EC2 instance: agent + Nextflow local executor + Docker | Exercises the full architecture with near-zero infra; AWS Batch is a config upgrade later |
| Object storage | S3, per-workspace prefixes, presigned URLs | |

Guiding principle: **buy everything that isn't the product.** The product is the canvas-compiler, node library, samplesheet UX, and results experience.

---

## 10. Roadmap

**P0 — walking skeleton, built inside-out.** Goal: a hardcoded 2-node canvas runs end-to-end through the real spine (API → queue → agent lease → Nextflow → artifact upload → hash registration → viewable result). Ordered so the Rust learning curve rises gradually (easiest crate first):

- **P0a — compiler core.** Pure Rust, no async/DB: hardcoded canvas JSON → IR → segments → DSL2 text. Golden-file tests; generated code linted by Nextflow in CI.
- **P0b — agent, local mode.** A plain Linux process that runs a compiled segment with Nextflow's **local executor** + Docker, parses the trace file, hashes and stores outputs. No server. Kubernetes is deliberately not used here: where tasks run is a Nextflow executor setting (§5.4 level 1), so `k8s`, like Slurm and AWS Batch, is a later config option, not part of the agent's architecture.
- **P0c — control plane.** axum + sqlx + SKIP LOCKED queue; agent switches from local mode to leasing over HTTPS. One GitHub Actions E2E test: Postgres service container + real tiny Nextflow run.

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

---

## 12. Open Questions (parked)

- Cost controls / quotas / billing model
- Hosted notebooks (post-P3 candidate)
- Metadata field-level encryption design for sovereign tier
- Multi-cloud (GCP) support order
- Node marketplace / cross-workspace template sharing
