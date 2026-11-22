-- What a run was launched from (DESIGN.md §4.3, §7).

-- Nullable: the queue's own tests enqueue
-- runs without a pipeline.
ALTER TABLE runs
    ADD COLUMN project_id UUID,
    ADD COLUMN pipeline_version_id UUID REFERENCES pipeline_versions (id);

-- Which dataset fed each run input, for provenance (§7: provenance is
-- foreign keys).
CREATE TABLE run_inputs (
    run_id     UUID NOT NULL REFERENCES runs (id),
    name       TEXT NOT NULL,                     -- the data name, `fastqc_1__reads`
    dataset_id UUID NOT NULL REFERENCES datasets (id),
    PRIMARY KEY (run_id, name)
);
