# Jessie's Bioinformatics Platform

A no-code, scientist-focused bioinformatics platform: a visual pipeline builder whose canvas compiles to Nextflow DSL2, a data platform with provenance, and hybrid cloud/on-prem execution through runner agents. The full design is in `docs/DESIGN.md`. Read it before making architectural changes, and update it when a design decision changes.

## Goals and working style

- **This is primarily a project for learning Rust and system design.** David started with no Rust experience. A product that becomes real is a bonus. The idea came from a friend, who is the domain expert.
- When writing Rust, explain the language concepts involved (ownership, borrowing, enums and `match`, `Result` and `?`, traits, async) as they come up. Treat compiler errors as teaching moments, not something to fix silently.
- Prefer beginner-friendly idioms early on: owned types (`String` over `&str` in structs), clone freely, `anyhow` for errors, no custom lifetimes or heavy generics until they're needed.
- Build the systems-interesting parts properly (compiler, queue, agent protocol, cache, failure handling). Use existing services for undifferentiated parts (auth, UI components).

## Stack

- Backend: Rust, one Cargo workspace (`crates/`). Parts: compiler (serde, petgraph, minijinja), API (axum + sqlx + Postgres), agent (tokio static binary), shared protocol/types crates.
- Frontend (later): Next.js + React Flow.
- Workflow engine: Nextflow DSL2. Generated code targets strict syntax.
- Job queue: Postgres `FOR UPDATE SKIP LOCKED`, hand-rolled.

## Roadmap position

P0 is built inside-out:
1. **P0a — compiler core** (current): canvas JSON → IR → segments → DSL2 text, with golden-file tests.
2. **P0b — agent in local mode**: runs a segment with Nextflow, parses the trace file, hashes outputs.
3. **P0c — control plane**: axum + sqlx + queue; GitHub Actions end-to-end test with Postgres and a real Nextflow run.

## Conventions

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before calling work done.
- Compiler output is tested with golden files: expected `.nf` output is checked into the repo and diffed in tests.
