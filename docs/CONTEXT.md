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

## Progress log

Dates are when the work reached `main`.

- 2026-10-18: Cargo workspace and canvas types (first commit; drafted 2026-09-27). CI on GitHub Actions with Rust pinned (PR #1). Validation and `PipelineGraph` (#2).
- 2026-11-07: segmentation (#3); node library with vendored FastQC/MultiQC (#4); job interfaces, manifests and the `protocol` crate (#5); codegen and the `compiler` CLI with stub runs (#6, #7, landed via #8).
- 2026-11-10: **first real run**: the compiled FastQC → MultiQC project on real reads with Docker.
- 2026-11-11: the agent: preflight and staging (#9), `agent run` (#10), trace, hashes, output manifests and `result.json` (#11). Full review of P0a/P0b; fixes for stopping (#12) and parameters plus the README (#13). Remaining gaps: DESIGN.md §12.

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
- Test data: the nextflow-io/rnaseq-nf demo reads `ggal_gut_1.fq` / `ggal_gut_2.fq` (pinned commit `53377cf`), downloaded to `data/ggal/`, which is git-ignored. `out/` (compiled projects and run directories) is git-ignored too.
- GitHub: `Hanan-Li/jessie_bio`, SSH remote. The `gh` CLI isn't installed, so pull requests are opened through prefilled compare links.
- **Pull requests target `main` directly.** Stacked PRs (a PR whose base is another PR's branch) merged into their base branches instead of `main` (#6, #7, fixed by #8), because "Automatically delete head branches" was off. Don't stack PRs unless that setting is on.
- The web version of the design doc is published as a Claude artifact: https://claude.ai/code/artifact/86c03945-aca6-4d35-a366-3d69b303d6e3. It shows draft v2 (2026-09-27) and its source, `design-doc.html`, stays in the Windows design folder; `docs/DESIGN.md` in this repo is the up-to-date version.
