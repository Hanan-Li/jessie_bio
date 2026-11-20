-- The job queue (DESIGN.md §6) and the agents that lease from it (§4.1, §8.3).

-- Where jobs run, and the settings every lease for it carries. One row per
-- compute environment: a laptop, an HPC cluster, a cloud account.
CREATE TABLE compute_envs (
    id               TEXT PRIMARY KEY,
    nextflow_version TEXT NOT NULL,
    hash_algorithm   TEXT NOT NULL DEFAULT 'sha256',
    time_limit_secs  BIGINT CHECK (time_limit_secs > 0),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- An agent and its token. Only the token's digest is stored: a leaked
-- database (or backup) doesn't hand out working agent tokens.
CREATE TABLE agents (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name          TEXT NOT NULL,
    compute_env   TEXT NOT NULL REFERENCES compute_envs (id),
    token_digest  TEXT NOT NULL UNIQUE,
    agent_version TEXT,
    last_seen_at  TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A run of a pipeline. `project_id` and `pipeline_version_id` arrive with
-- the UI endpoints (P0c PR 4).
CREATE TABLE runs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    state       TEXT NOT NULL DEFAULT 'running'
                CHECK (state IN ('running', 'succeeded', 'failed', 'cancelled')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ
);

-- One row per job of a run's execution plan. Only the queue module
-- (src/queue.rs) writes this table.
CREATE TABLE jobs (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id           UUID NOT NULL REFERENCES runs (id),
    job_index        INT NOT NULL CHECK (job_index >= 0),
    kind             TEXT NOT NULL CHECK (kind IN ('batch', 'analysis')),

    -- The queue.
    state            TEXT NOT NULL CHECK (state IN
                     ('blocked', 'queued', 'leased', 'succeeded', 'failed', 'cancelled', 'skipped')),
    attempt          INT NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    max_attempts     INT NOT NULL DEFAULT 3 CHECK (max_attempts > 0),
    run_at           TIMESTAMPTZ NOT NULL DEFAULT now(),   -- retry backoff

    -- The lease. `agent_id` stays set after the job finishes: who ran it.
    agent_id         UUID REFERENCES agents (id),
    lease_expires_at TIMESTAMPTZ,
    cancel_requested BOOLEAN NOT NULL DEFAULT false,
    progress         TEXT,                                 -- from heartbeats

    -- Placement.
    compute_env      TEXT NOT NULL REFERENCES compute_envs (id),
    pinned_agent     UUID REFERENCES agents (id),

    -- What the lease response sends. `resolved_inputs` holds the inputs
    -- known so far: a blocked job has its run inputs, and gains the
    -- outputs of earlier jobs when it is queued.
    project_tree     TEXT NOT NULL REFERENCES blobs (digest),
    interface        JSONB NOT NULL,
    resolved_inputs  JSONB NOT NULL DEFAULT '{}',

    -- The outcome.
    result           JSONB,
    failure_reason   TEXT,

    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at      TIMESTAMPTZ,

    UNIQUE (run_id, job_index),
    -- A leased job always has a holder and an expiry; no other job has an
    -- expiry. The database refuses a half-made lease.
    CHECK ((state = 'leased') = (lease_expires_at IS NOT NULL)),
    CHECK (state <> 'leased' OR agent_id IS NOT NULL)
);

-- The two queries that run constantly: acquire (queued jobs of one
-- environment, oldest first) and the reaper (expired leases). Partial
-- indexes stay small because finished jobs aren't in them.
CREATE INDEX jobs_queued ON jobs (compute_env, created_at) WHERE state = 'queued';
CREATE INDEX jobs_lease_expiry ON jobs (lease_expires_at) WHERE state = 'leased';
CREATE INDEX jobs_run ON jobs (run_id);

CREATE TABLE job_dependencies (
    job_id            UUID NOT NULL REFERENCES jobs (id),
    depends_on_job_id UUID NOT NULL REFERENCES jobs (id),
    PRIMARY KEY (job_id, depends_on_job_id),
    CHECK (job_id <> depends_on_job_id)
);
CREATE INDEX job_dependencies_upstream ON job_dependencies (depends_on_job_id);

-- Every file a succeeded job produced, and the agent whose disk holds it
-- (outputs stay on the agent's machine in P0c). Becomes FileRecords (§7).
CREATE TABLE job_outputs (
    job_id   UUID NOT NULL REFERENCES jobs (id),
    name     TEXT NOT NULL,                 -- the data name, `fastqc_1__zip`
    position INT NOT NULL,                  -- order within the output (R1 before R2)
    sample   TEXT,
    uri      TEXT NOT NULL,
    digest   TEXT NOT NULL,
    size     BIGINT NOT NULL CHECK (size >= 0),
    agent_id UUID NOT NULL REFERENCES agents (id)
);
CREATE INDEX job_outputs_job ON job_outputs (job_id, name, position);

-- Which blobs each project tree lists, so the blob endpoint can serve an
-- agent only the files of jobs in its own compute environment.
CREATE TABLE tree_entries (
    tree_digest TEXT NOT NULL REFERENCES blobs (digest),
    path        TEXT NOT NULL,
    blob_digest TEXT NOT NULL REFERENCES blobs (digest),
    PRIMARY KEY (tree_digest, path)
);
CREATE INDEX tree_entries_blob ON tree_entries (blob_digest);
