# Project context and discussion notes

Background from the design conversations (2026-09-14 and 2026-09-27) that isn't a design decision, so it isn't in `DESIGN.md`. Read alongside `DESIGN.md` and `CLAUDE.md`.

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
- **Segments**: the compiler emits one Nextflow run per maximal connected group of batch nodes, not one per canvas or one per node. This keeps per-sample pipelining inside Nextflow while the per-node cache still works (cached nodes are pruned before codegen).
- **Multi-machine execution**, in order: native Nextflow executors, then whole-run placement across environments, then segment placement under the "data flows downhill" rule.
- **P0 is built inside-out** (compiler → agent → API) so the Rust learning curve rises gradually.

## Honest market assessment (2026-09-27)

These are for awareness, not blockers; the project is learning-first.

- **The problem is real**: labs generate far more sequencing data than bioinformatics cores can analyse.
- **The field is crowded**: Galaxy (free, widely installed), Partek Flow, CLC Genomics Workbench, Basepair, Latch Bio, Illumina's platforms, Seqera. Success would come from execution and a niche, not the concept.
- **Biggest product risk**: bench scientists may not want to *compose* pipelines at all, only pick validated templates, tweak parameters and get figures. Freeform canvas building mainly suits bioinformaticians, who can code. The suggestion (not yet decided) was to emphasise templates, the samplesheet builder, results and provenance in P1, and use the canvas mainly to view and tweak pipelines.
- **Scientific correctness**: no-code DESeq2 makes invalid designs easy (confounding, no replicates). Design validation and plain-language warnings should be core features.
- **Market**: academic institutions are slow buyers with a free incumbent. Small biotechs with no in-house bioinformatician may be a better first market.
- **AI opportunity (not yet decided)**: have an AI assemble the canvas from a plain-English description. The typed canvas makes AI-built pipelines checkable, and the scientist approves the pipeline visually before it runs.

## Development environment

- Development happens in a **Linux VM** (Ubuntu/Debian assumed), with the repo on the VM's own filesystem (not a Windows shared folder).
- Java 17+, Nextflow and Docker aren't needed until P0b.
- The web version of the design doc is published as a Claude artifact: https://claude.ai/code/artifact/86c03945-aca6-4d35-a366-3d69b303d6e3. Its source, `design-doc.html`, stays in the Windows design folder.
