-- The node library and content-addressed blobs (DESIGN.md §3.4, §4.1).

-- Files stored under their digest. A blob never changes: rows are only
-- inserted, never updated.
CREATE TABLE blobs (
    digest     TEXT PRIMARY KEY,                 -- "sha256:<hex>"
    size       BIGINT NOT NULL CHECK (size >= 0),
    bytes      BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (octet_length(bytes) = size)
);

-- One row per published node version. A row never changes once written:
-- the API refuses to start if nodes/ has different content for an
-- existing (id, version).
CREATE TABLE node_specs (
    id            TEXT NOT NULL,
    version       TEXT NOT NULL,
    spec          JSONB NOT NULL,
    -- Digest of the spec as the API serialized it, so formatting changes
    -- in spec.json don't count as content changes.
    spec_digest   TEXT NOT NULL,
    -- The Nextflow module's blob; NULL for analysis nodes.
    module_digest TEXT REFERENCES blobs (digest),
    published_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, version)
);
