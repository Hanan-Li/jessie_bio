//! Data formats shared between the compiler, the agent and the API.
//!
//! Everything here crosses a process boundary (a file, an HTTP body, a
//! database column), so it is plain serde data with no behaviour beyond
//! reading, writing and checking itself.

pub mod digest;
pub mod job;
pub mod manifest;
pub mod result;
pub mod types;
