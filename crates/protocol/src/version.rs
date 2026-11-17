//! Format versions for documents exchanged between separately deployed
//! programs (DESIGN.md §4.1, versioning).
//!
//! Agents and the control plane are upgraded at different times, so each
//! top-level document (`job.json`, `result.json`, the agent messages)
//! carries a `format_version`. It changes only for breaking changes; adding
//! a field with a default is not one. Reading a document from a *newer*
//! format fails with a clear message instead of misreading it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The newest format this build reads and the one it writes.
pub const CURRENT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FormatVersion(u32);

impl FormatVersion {
    /// The version this build writes.
    pub fn current() -> FormatVersion {
        FormatVersion(CURRENT)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// A document without `format_version` predates the field, so it is
/// version 1, not "whatever is current": once CURRENT is 2, an old file
/// must still read as 1.
impl Default for FormatVersion {
    fn default() -> FormatVersion {
        FormatVersion(1)
    }
}

impl fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for FormatVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.0)
    }
}

/// The check lives in deserialization, so every reader of every document
/// gets it without having to remember to call anything.
impl<'de> Deserialize<'de> for FormatVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<FormatVersion, D::Error> {
        let version = u32::deserialize(deserializer)?;
        if version == 0 {
            return Err(serde::de::Error::custom(
                "format_version starts at 1, got 0",
            ));
        }
        if version > CURRENT {
            return Err(serde::de::Error::custom(format!(
                "this document is format_version {version}, but this program only \
                 understands up to {CURRENT}: upgrade it"
            )));
        }
        Ok(FormatVersion(version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, Serialize)]
    struct Doc {
        #[serde(default)]
        format_version: FormatVersion,
    }

    #[test]
    fn writes_the_current_version() {
        let doc = Doc {
            format_version: FormatVersion::current(),
        };
        assert_eq!(
            serde_json::to_string(&doc).unwrap(),
            r#"{"format_version":1}"#
        );
    }

    #[test]
    fn a_missing_version_means_version_1() {
        let doc: Doc = serde_json::from_str("{}").unwrap();
        assert_eq!(doc.format_version.get(), 1);
    }

    #[test]
    fn refuses_a_newer_version_and_says_to_upgrade() {
        let err = serde_json::from_str::<Doc>(r#"{"format_version": 2}"#).unwrap_err();
        assert!(
            err.to_string()
                .contains("only understands up to 1: upgrade it"),
            "{err}"
        );
    }

    #[test]
    fn refuses_version_0() {
        assert!(serde_json::from_str::<Doc>(r#"{"format_version": 0}"#).is_err());
    }
}
