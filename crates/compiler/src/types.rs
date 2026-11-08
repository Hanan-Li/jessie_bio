//! The semantic type system carried by every node port (DESIGN.md §3.2).

use serde::Deserialize;

/// What kind of data flows through a port: a format plus its qualifiers.
///
/// In JSON the `format` field picks the variant and any qualifiers sit
/// beside it: `{"format": "fastq", "paired": true}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

/// Can data of type `output` flow into a port that accepts `input`?
///
/// The rules, as the tests below spell out:
/// - Formats must match: a FASTQ never feeds a BAM port.
/// - FASTQ: `paired` must match exactly. Paired-end and single-end reads
///   are processed differently, so neither can stand in for the other.
/// - BAM: the output must have every qualifier the input requires. A sorted,
///   indexed BAM can feed a port that only needs "sorted"; an unsorted BAM
///   cannot feed a port that needs "sorted".
/// - Formats without qualifiers connect when the formats are the same.
pub fn can_connect(output: &SemanticType, input: &SemanticType) -> bool {
    match (output, input) {
        (
            &SemanticType::Fastq { paired: out_paired },
            &SemanticType::Fastq { paired: in_paired },
        ) => out_paired == in_paired,
        (
            &SemanticType::Bam {
                sorted: is_sorted,
                indexed: is_indexed,
            },
            &SemanticType::Bam {
                sorted: needs_sorted,
                indexed: needs_indexed,
            },
        ) => (is_sorted || !needs_sorted) && (is_indexed || !needs_indexed),
        // Everything else: formats without qualifiers, or two different
        // formats. Exact equality is the strict, safe default for any
        // qualified format added later without its own rule here.
        _ => output == input,
    }
}

/// Whether a port carries one item per sample or one item for the whole dataset.
///
/// A `PerSample` output feeding a `PerDataset` input is a gather: the
/// compiler emits `.collect()` on that edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

    // ---- can_connect ----

    fn bam(sorted: bool, indexed: bool) -> SemanticType {
        SemanticType::Bam { sorted, indexed }
    }

    #[test]
    fn same_simple_format_connects() {
        assert!(can_connect(
            &SemanticType::QcReport,
            &SemanticType::QcReport
        ));
    }

    #[test]
    fn different_formats_do_not_connect() {
        assert!(!can_connect(
            &SemanticType::QcReport,
            &SemanticType::CountMatrix
        ));
        assert!(!can_connect(
            &SemanticType::Fastq { paired: true },
            &bam(false, false)
        ));
    }

    #[test]
    fn fastq_pairedness_must_match() {
        let paired = SemanticType::Fastq { paired: true };
        let single = SemanticType::Fastq { paired: false };
        assert!(can_connect(&paired, &paired));
        assert!(can_connect(&single, &single));
        assert!(!can_connect(&paired, &single));
        assert!(!can_connect(&single, &paired));
    }

    #[test]
    fn bam_with_extra_qualifiers_connects() {
        assert!(can_connect(&bam(true, true), &bam(true, false)));
        assert!(can_connect(&bam(true, true), &bam(false, false)));
        assert!(can_connect(&bam(true, false), &bam(true, false)));
    }

    #[test]
    fn bam_missing_a_required_qualifier_does_not_connect() {
        assert!(!can_connect(&bam(false, false), &bam(true, false)));
        assert!(!can_connect(&bam(true, false), &bam(true, true)));
        assert!(!can_connect(&bam(false, true), &bam(true, true)));
    }

    // ---- parsing ----

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
}
