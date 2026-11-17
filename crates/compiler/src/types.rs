//! Edge compatibility over the semantic type system.
//!
//! The types themselves live in the shared `protocol` crate; they are
//! re-exported here so the rest of the compiler can keep using `crate::types`.

pub use protocol::types::{Cardinality, SemanticType};

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
    // A format from a newer document never connects, not even to itself.
    if *output == SemanticType::Unknown || *input == SemanticType::Unknown {
        return false;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- can_connect ----

    fn bam(sorted: bool, indexed: bool) -> SemanticType {
        SemanticType::Bam { sorted, indexed }
    }

    #[test]
    fn unknown_formats_never_connect() {
        let unknown = SemanticType::Unknown;
        assert!(!can_connect(&unknown, &unknown));
        assert!(!can_connect(&unknown, &SemanticType::QcReport));
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
}
