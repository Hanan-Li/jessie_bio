-- Pipelines and datasets (DESIGN.md §4.3, §7).
--
-- `project_id` columns are here from the start, so adding projects (P1)
-- reshapes nothing: NULL until projects exist.

-- A pipeline is a name; what runs is one of its versions.
CREATE TABLE pipelines (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID,
    name       TEXT NOT NULL CHECK (name <> ''),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- One pipeline per name within a project. NULLS NOT DISTINCT (Postgres 15+)
-- makes two NULL project ids count as the same project.
CREATE UNIQUE INDEX pipelines_name ON pipelines (project_id, name) NULLS NOT DISTINCT;

-- A validated canvas. A row never changes once written: a run records the
-- exact version it ran, so editing a pipeline makes a new version.
CREATE TABLE pipeline_versions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pipeline_id UUID NOT NULL REFERENCES pipelines (id),
    version     INT NOT NULL CHECK (version > 0),   -- 1, 2, 3… per pipeline
    canvas      JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (pipeline_id, version)
);

-- Samples and their files, on one agent's disk (outputs and inputs stay on
-- agents' machines in P0c, DESIGN.md §4.1). The control plane never reads
-- the bytes: the digests come from whoever registered the dataset.
CREATE TABLE datasets (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID,
    name       TEXT NOT NULL CHECK (name <> ''),
    agent_id   UUID NOT NULL REFERENCES agents (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE dataset_files (
    dataset_id UUID NOT NULL REFERENCES datasets (id),
    sample     TEXT NOT NULL CHECK (sample <> ''),
    position   INT NOT NULL CHECK (position >= 0),  -- order within the sample (R1 before R2)
    uri        TEXT NOT NULL,
    digest     TEXT NOT NULL,
    size       BIGINT NOT NULL CHECK (size >= 0),
    PRIMARY KEY (dataset_id, sample, position)
);
