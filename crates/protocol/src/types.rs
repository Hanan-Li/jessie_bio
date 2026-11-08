//! The semantic type system carried by every node port (DESIGN.md §3.2).
//!
//! Shared vocabulary: the compiler checks edges with it, job interfaces and
//! (later) file records in the database are labelled with it.

use serde::{Deserialize, Serialize};

/// What kind of data flows through a port: a format plus its qualifiers.
///
/// In JSON the `format` field picks the variant and any qualifiers sit
/// beside it: `{"format": "fastq", "paired": true}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticType {
    Fastq {
        paired: bool,
    },
    Bam {
        sorted: bool,
        indexed: bool,
    },
    Vcf,
    /// Raw per-tool QC output (FastQC zip, fastp JSON) that MultiQC aggregates.
    QcReport,
    CountMatrix,
    SampleSheet,
    Genome,
    /// A standalone HTML report (FastQC, MultiQC), shown embedded in results.
    HtmlReport,
    /// A rendered result: Vega-Lite spec plus its data (DESIGN.md §3.7).
    ReportArtifact,
}

/// Whether a port carries one item per sample or one item for the whole dataset.
///
/// A `PerSample` output feeding a `PerDataset` input is a gather: the
/// compiler emits `.collect()` on that edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    PerSample,
    PerDataset,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_format_with_qualifiers() {
        let ty: SemanticType =
            serde_json::from_str(r#"{"format": "fastq", "paired": true}"#).unwrap();
        assert_eq!(ty, SemanticType::Fastq { paired: true });
    }

    #[test]
    fn parses_format_without_qualifiers() {
        let ty: SemanticType = serde_json::from_str(r#"{"format": "count_matrix"}"#).unwrap();
        assert_eq!(ty, SemanticType::CountMatrix);
    }

    #[test]
    fn rejects_unknown_format() {
        let result = serde_json::from_str::<SemanticType>(r#"{"format": "cram"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_missing_qualifier() {
        // A BAM must say whether it is sorted and indexed; there is no default.
        let result = serde_json::from_str::<SemanticType>(r#"{"format": "bam", "sorted": true}"#);
        assert!(result.is_err());
    }

    #[test]
    fn serializes_to_the_same_json_it_parses() {
        let ty = SemanticType::Bam {
            sorted: true,
            indexed: false,
        };
        let json = serde_json::to_string(&ty).unwrap();
        assert_eq!(json, r#"{"format":"bam","sorted":true,"indexed":false}"#);
        assert_eq!(serde_json::from_str::<SemanticType>(&json).unwrap(), ty);
    }
}
