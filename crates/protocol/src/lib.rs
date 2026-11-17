//! Data formats shared between the compiler, the agent and the API.
//!
//! Everything here crosses a process boundary (a file, an HTTP body, a
//! database column), so it is plain serde data with no behaviour beyond
//! reading, writing and checking itself. Documents read by separately
//! deployed programs follow the versioning rules in DESIGN.md §4.1: unknown
//! fields are ignored, and each top-level document carries a
//! `format_version` (see `version`).

pub mod blob;
pub mod digest;
pub mod job;
pub mod lease;
pub mod manifest;
pub mod result;
pub mod types;
pub mod version;
