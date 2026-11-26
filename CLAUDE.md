# Jessie's Bioinformatics Platform

A no-code, scientist-focused bioinformatics platform: a visual pipeline builder whose canvas compiles to Nextflow DSL2, a data platform with provenance, and hybrid cloud/on-prem execution through runner agents.

- `docs/DESIGN.md`: the design. Read it before making architectural changes, and update it when a design decision changes. Known gaps are listed in §12.
- `docs/CONTEXT.md`: why decisions were made, the progress log, and the development environment.
- `README.md`: how to build, check and run things.

## Goals and working style

- **This is primarily a project for learning Rust and system design.** David started with no Rust experience. A product that becomes real is a bonus. The idea came from a friend, who is the domain expert.
- When writing Rust, explain the language concepts involved (ownership, borrowing, enums and `match`, `Result` and `?`, traits, async) as they come up. Treat compiler errors as teaching moments, not something to fix silently.
- Prefer beginner-friendly idioms early on: owned types (`String` over `&str` in structs), clone freely, `anyhow` for errors, no custom lifetimes or heavy generics until they're needed.
- Build the systems-interesting parts properly (compiler, queue, agent protocol, cache, failure handling). Use existing services for undifferentiated parts (auth, UI components).

## Stack

- Backend: Rust 1.98 (pinned in `rust-toolchain.toml`), one Cargo workspace:
  - `crates/compiler`: canvas → validated graph → jobs → Nextflow projects, plus the `compiler` CLI (serde, petgraph, minijinja, clap).
  - `crates/agent`: runs a compiled job and records the result, the `agent` CLI (tokio, nix, sha2, clap, reqwest with rustls). Depends on `protocol`, never on `compiler`.
  - `crates/protocol`: formats that cross process boundaries: `job.json`, manifests, digests and the shared hasher, `result.json`, port types, the agent messages, blobs and trees, format versions (serde, csv, sha2). Versioning rules in `docs/DESIGN.md` §4.1: no `deny_unknown_fields` here, `format_version` on top-level documents.
  - `crates/api` (P0c, built per `docs/DESIGN.md` §4.3): the `api` server, axum + sqlx + Postgres, migrations in `crates/api/migrations/`; depends on `compiler` and `protocol`.
- Node library: `nodes/<id>/<version>/` (spec.json, vendored nf-core module, UPSTREAM.md).
- Frontend (later): Next.js + React Flow.
- Workflow engine: Nextflow DSL2. Generated code targets strict syntax.
- Job queue: Postgres `FOR UPDATE SKIP LOCKED`, hand-rolled.

## Roadmap position

P0 is built inside-out:
1. **P0a — compiler core** (done): canvas JSON → `PipelineGraph` (the IR) → segments → DSL2 text, with golden-file tests; `compiler` CLI.
2. **P0b — agent in local mode** (done for batch jobs): `agent run` stages, runs and supervises Nextflow (clean stop on Ctrl-C, SIGTERM or a time limit), parses the trace, hashes outputs, writes output manifests and `result.json`.
3. **P0c — control plane** (in progress; designed in `docs/DESIGN.md`: agent protocol §4.1, control-plane API §4.3, queue §6; building in the order listed in §4.3; PRs 1 (`protocol` messages), 2 (`api` skeleton), 3 (queue, agent endpoints, reaper), 4 (UI endpoints, launch) and 5 (`agent serve`) done; PR 6, the end-to-end test, remains): axum + sqlx + queue; the agent leases jobs over HTTPS; GitHub Actions end-to-end test with Postgres, the API and `agent serve`, using stub runs.

## Conventions

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before calling work done. The api tests need Postgres: `scripts/dev-db.sh`, then `export DATABASE_URL=postgres://jessie:jessie@localhost:5433/jessie`.
- Run `nextflow lint nodes/` after changing any Nextflow module (Nextflow 26.10.5, Java 17; CI pins the same version).
- Run `scripts/stub-run.sh` after changing codegen or a module: it compiles the test canvas, lints the generated project and stub-runs it (no containers).
- Compiler output is tested with golden files: expected output is checked into `crates/compiler/testdata/expected/` and diffed in tests. After an intended change, regenerate with `UPDATE_EXPECTED=1 cargo test` and review the diff.
- Check that a new test can fail: break the code it guards, confirm the test fails, restore.
- Verify behaviour against real tools (Nextflow, Docker, `sha256sum`) when a claim depends on them, not just unit tests.
- Open each pull request against `main`. Don't stack PRs (see `docs/CONTEXT.md`).
