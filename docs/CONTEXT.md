# Project context and discussion notes

Background from the design and build conversations (2026-09-14 to 2026-11-11) that isn't itself the design, so it isn't in `DESIGN.md`: why decisions were made, progress, and the development environment. Read alongside `DESIGN.md` and `CLAUDE.md`.

## Why this project exists

- David's primary goal is **learning Rust and system design**. A real product is a bonus.
- The product idea came from **a friend**, who is the domain expert. Involve them as first user, source of trusted test data (e.g. a small public RNA-seq dataset), and the person to run validation conversations with scientists.
- If it ever looks commercially real, agree early on how ownership is split between David and the friend.

## How decisions were reached (short log)

- **Nextflow** was chosen by David up front. The canvas compiles to generated DSL2; export-as-Nextflow is an escape hatch for bioinformaticians.
- **Hybrid "agent" model** (cloud control plane + outbound-polling runner agents) was chosen to reconcile a seamless managed product with institutions that won't put sensitive data on a third-party site. Only metadata goes to the control plane; bytes stay in the workspace data plane.
- **Static DAGs only**, a **dedicated samplesheet builder**, and **lightweight analysis nodes instead of notebooks** were all chosen by David.
- **RBAC**: David chose that any Member can create projects ("open to noobs") and that there are no public share links in v1.
- **Postgres job queue** (SKIP LOCKED) instead of Redis/Celery, because it means one fewer system to run and enqueueing is transactional.
- **Language**: the stack was originally Python/FastAPI. David switched to an **all-Rust backend** because he wants to learn Rust. The compiler must live in the control plane, so a Python/Rust split would put a language seam through the most important code.
- **Segments**: the compiler emits one Nextflow run per connected group of batch nodes, not one per canvas or one per node. This keeps per-sample pipelining inside Nextflow while the per-node cache still works (cached nodes are pruned before codegen). Building it (2026-11-07) showed the original "maximal connected subgraph" definition was wrong: segments must also be convex, enforced with the analysis-depth rule (DESIGN.md §3.3).
- **Multi-machine execution**, in order: native Nextflow executors, then whole-run placement across environments, then segment placement under the "data flows downhill" rule.
- **P0 is built inside-out** (compiler → agent → API) so the Rust learning curve rises gradually.
- **Codegen** (2026-11-07): process code is curated, not generated; the compiler emits only wiring. nf-core modules are vendored by hand into `nodes/` at a pinned commit (no nf-core Python tooling). Templates via minijinja with all logic in Rust.
- **Names**: David asked for self-explanatory names, so the compiler's IR is `PipelineGraph` (`pipeline_graph.rs`), not `Ir`. Types that are only valid after a check (`PipelineGraph`, `ExecutionPlan`, `Digest`) can only be built by that check.
- **Shared `protocol` crate** (2026-11-07): formats that cross a process boundary (job interface, manifests, digests, job results, port types) live in one crate that the compiler, agent and API depend on. Intermediate compiler types stay in the compiler. The agent never depends on the compiler.
- **Node specs in JSON, not YAML** (2026-11-07): `serde_yaml` is unmaintained, and the database will store specs as JSON.
- **Local executor first** (2026-11-07): David proposed Kubernetes for the first runner agent. Decided against for now: k8s is just a Nextflow executor setting, it needs shared work storage and pod permissions, and target users mostly run Slurm or AWS Batch. Possible later milestone (kind/k3s locally), or for hosting the control plane.
- **Hashing** (2026-11-10): SHA-256, self-describing (`sha256:<hex>`). David asked that the algorithm be easy to switch, so it is an enum (`HashAlgorithm`) and `agent run --hash` selects it; BLAKE3 is the likely addition for very large files.
- **Containers run as the calling user** (2026-11-10): found on the first real run, when root-owned files in `work/` couldn't be deleted.
- **Stopping** (2026-11-11, from the review): SIGTERM to the agent is a stop like Ctrl-C, and the job becomes `interrupted` ("retry it"), not `cancelled`. Before the fix, stopping the agent orphaned Nextflow and lost the result.
- **Parameters rejected until designed** (2026-11-11, from the review): the test canvas set a MultiQC title that was silently ignored. Rejecting is better than ignoring a scientist's setting.
- **P0c agent protocol** (2026-11-13, design walkthrough): leases with fencing rather than ownership; one lease response carrying the whole job description, metadata only. David chose project files **by digest** over inline (inline would have been fine at 7 KB) to build the scaling path from the start, modelled on Bazel's CAS. Outputs stay on the agent's machine for now; uploads come later. Versioning: the protobuf-style rules proposed in the walkthrough.
- **P0c queue** (2026-11-16, design walkthrough): retry only what the infrastructure broke, never deterministic failures; `interrupted` counts against the 3 attempts (David's call, so a constantly restarted agent can't hold a job forever); analysis jobs rejected at launch; run state stored. David asked whether Postgres scales or a distributed queue is needed: staying on Postgres, because jobs are whole segments and the queue sees under a job per second even at large scale; the scaling steps and the signals for revisiting are in DESIGN.md §6.
- **P0c API** (2026-11-16, design walkthrough): David chose a development token instead of user accounts for P0c, stub runs in the CI end-to-end test (real Docker runs stay local), and compiling synchronously inside the launch request for ease of development. He asked how a scientist would actually provide a dataset from the UI; the answer, the agent-side inventory flow with the samplesheet builder, is designed for P1 (DESIGN.md §4.3), and P0c uses a script that computes digests with `sha256sum`.
- **P0c queue, as built** (2026-11-20, PR 3): building it turned up a race the design hadn't named. When two jobs finish at once, each transaction can see the other as unfinished, so the job that needs both is never queued and the run hangs. The fix is to lock the run's row before changing its jobs (DESIGN.md §6, "One run, one writer at a time"). A test runs the race and fails without the lock. Also decided while building: an expired lease that hasn't been reaped yet can still be renewed, since the attempt number is the fence; repeating a result is answered with 200 rather than 409; blobs are served `private`; agent tokens are stored only as digests.

- **P0c UI endpoints, as built** (2026-11-22, PR 4): David first asked to build `agent serve` (PR 5) ahead of the design order, then changed his mind and kept the order. He then asked for the large PR 4 to be split into four smaller ones. Building it led to more checks at launch than the design listed (datasets must be on an agent of the run's compute environment, paired reads need two files per sample, a job's datasets must share an agent), to limiting sample names to path-safe characters, since they become directory names on agents' machines, and to reading the compiler's registry back from the database rather than from `nodes/`. A shell lesson from `register-dataset.sh`: an apostrophe inside `"${VAR:?message}"` opens a single-quoted string that runs on to the next `'` in the file, and bash's error points to that later line.

- **P0c `agent serve`, as built** (2026-11-26, PR 5): split into three PRs to keep each small. Decided while building: anything that will fail the same way on every agent (a lease that contradicts itself, input files missing on this machine) is reported `failed`, anything the agent itself broke is `interrupted` so the queue retries it; heartbeats continue after a cancel and while the result is posted, so the lease can't lapse while Nextflow shuts down; Ctrl-C and SIGTERM both mean "shut down" in `serve`; the HTTP client uses rustls, so agents need no OpenSSL (building needs `cc` and `cmake`). Checked end to end in a cloud session with real Nextflow 26.10.5: a run launched through the API, stub-run by `agent serve`, reached `succeeded`.
- **Architecture review** (2026-11-26): David led a review of everything built so far, section by section: the overall architecture, how the components fit, how the agent is installed, then each container with most attention on the control plane. It found the queue and agent internals solid and the operational shell thin. The install story it produced is in DESIGN.md §4.1 (*Installing an agent*, including Apptainer for HPC and systemd's cgroup cleanup for the orphan problem); its findings are in §12, and the trust discussion is the next entry.
- **Trust and threat model** (2026-11-26, architecture review): today a compromised control plane could run any code at every agent's site, which David accepted for the proof of concept. David proposed compiling on the agent too and comparing digests with the control plane's project; rejected because the modules are inputs to compilation (both sides would agree on a malicious module) and because it needs byte-identical compilers everywhere. Chosen instead, in four layers (DESIGN.md §8.4): agent-side data roots, signed node releases with a key the control plane never holds, the agent rendering the wiring from a plan sent as data (David's idea, generating instead of comparing), and sandboxing.

## Progress log

Dates are when the work reached `main`.

- 2026-10-18: Cargo workspace and canvas types (first commit; drafted 2026-09-27). CI on GitHub Actions with Rust pinned (PR #1). Validation and `PipelineGraph` (#2).
- 2026-11-07: segmentation (#3); node library with vendored FastQC/MultiQC (#4); job interfaces, manifests and the `protocol` crate (#5); codegen and the `compiler` CLI with stub runs (#6, #7, landed via #8).
- 2026-11-10: **first real run**: the compiled FastQC → MultiQC project on real reads with Docker.
- 2026-11-11: the agent: preflight and staging (#9), `agent run` (#10), trace, hashes, output manifests and `result.json` (#11). Full review of P0a/P0b; fixes for stopping (#12) and parameters plus the README (#13). Remaining gaps: DESIGN.md §12.
- 2026-11-13 to 2026-11-16: the P0c design: agent protocol (#15), queue and control-plane API (#16). P0c PR 1, the `protocol` messages (#17), and PR 2, the `api` skeleton (#18).
- 2026-11-22 to 2026-11-24: P0c PR 3, the queue, agent endpoints and reaper (#19). PR 4, the UI endpoints, was split into four smaller PRs at David's request (one large PR was too much to review): part 1, the development token and validate (#20); part 2, saving pipelines and registering datasets (#21); part 3, launch (#22); part 4, run status and cancel (#23).
- 2026-11-26: P0c PR 5, `agent serve`, in three parts: the blob cache and writing a lease to disk (#24), the HTTP client (#25), and the serve loop with heartbeats and results (#26). DESIGN.md §8.4, the trust and threat model, from the architecture review (#27).

## Honest market assessment (2026-09-27)

These are for awareness, not blockers; the project is learning-first.

- **The problem is real**: labs generate far more sequencing data than bioinformatics cores can analyse.
- **The field is crowded**: Galaxy (free, widely installed), Partek Flow, CLC Genomics Workbench, Basepair, Latch Bio, Illumina's platforms, Seqera. Success would come from execution and a niche, not the concept.
- **Biggest product risk**: bench scientists may not want to *compose* pipelines at all, only pick validated templates, tweak parameters and get figures. Freeform canvas building mainly suits bioinformaticians, who can code. The suggestion (not yet decided) was to emphasise templates, the samplesheet builder, results and provenance in P1, and use the canvas mainly to view and tweak pipelines.
- **Scientific correctness**: no-code DESeq2 makes invalid designs easy (confounding, no replicates). Design validation and plain-language warnings should be core features.
- **Market**: academic institutions are slow buyers with a free incumbent. Small biotechs with no in-house bioinformatician may be a better first market.
- **AI opportunity (not yet decided)**: have an AI assemble the canvas from a plain-English description. The typed canvas makes AI-built pipelines checkable, and the scientist approves the pipeline visually before it runs.

## Development environment

- Development happens in a **Linux VM** (Ubuntu), with the repo on the VM's own filesystem (not a Windows shared folder).
- Installed: Rust via rustup (the repo pins 1.98), Java 17, Nextflow 26.10.5 (in `~/.local/bin`), Docker 29. David's user is in the `docker` group, which real runs need.
- Cloud sessions (Claude Code on the web) have no Docker daemon, so `scripts/dev-db.sh` can't run there; they use the system Postgres 16 on the same port, 5433, with the same `jessie` role and database.
- Development Postgres: the `jessie-postgres` container (`postgres:17`, 671 MB image) from `scripts/dev-db.sh`, on **127.0.0.1:5433** with the `jessie-pgdata` volume. Port 5432 is taken by a system-wide Postgres service on the VM, left untouched.
- Port 8080 is taken by David's `freqtrade` container (FreqUI), which answers 200 to every path; the API defaults to **127.0.0.1:8470**. Lesson from setting this up: a health check must recognise the service's own response, not just accept any 200.
- Test data: the nextflow-io/rnaseq-nf demo reads `ggal_gut_1.fq` / `ggal_gut_2.fq` (pinned commit `53377cf`), downloaded to `data/ggal/`, which is git-ignored. `out/` (compiled projects and run directories) is git-ignored too.
- GitHub: `Hanan-Li/jessie_bio`, SSH remote. The `gh` CLI isn't installed, so pull requests are opened through prefilled compare links.
- **Pull requests target `main` directly.** Stacked PRs (a PR whose base is another PR's branch) merged into their base branches instead of `main` (#6, #7, fixed by #8), because "Automatically delete head branches" was off. Don't stack PRs unless that setting is on.
- The web version of the design doc is published as a Claude artifact: https://claude.ai/code/artifact/86c03945-aca6-4d35-a366-3d69b303d6e3. It shows draft v2 (2026-09-27) and its source, `design-doc.html`, stays in the Windows design folder; `docs/DESIGN.md` in this repo is the up-to-date version.
