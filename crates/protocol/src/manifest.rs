//! Manifests: how data crosses a job boundary (DESIGN.md §3.3).
//!
//! A manifest lists the files behind one job input or output. It is a CSV
//! file, because the generated Nextflow code reads it with `splitCsv`:
//!
//! ```text
//! per-sample                    per-dataset
//! sample,path                   path
//! S1,/data/S1_R1.fastq.gz       /results/S1_fastqc.zip
//! S1,/data/S1_R2.fastq.gz       /results/S2_fastqc.zip
//! S2,/data/S2_R1.fastq.gz
//! S2,/data/S2_R2.fastq.gz
//! ```
//!
//! Per-sample manifests carry the sample name on every row. That is what
//! lets each sample's identity survive the boundary: the next job rebuilds
//! Nextflow's `[meta, files]` tuples from it, so provenance stays intact.
//! A sample with several files (paired reads: R1 then R2) has one row per
//! file, in order.

use anyhow::{Context, bail, ensure};

use crate::types::Cardinality;

/// Column holding the sample name in a per-sample manifest.
pub const SAMPLE_COLUMN: &str = "sample";
/// Column holding a file's location: a local path, or a URI Nextflow
/// understands (such as `s3://...`).
pub const PATH_COLUMN: &str = "path";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manifest {
    /// One entry per sample, in manifest order.
    PerSample(Vec<SampleFiles>),
    /// Files belonging to the dataset as a whole.
    PerDataset(Vec<String>),
}

/// One sample's files, in order (e.g. R1 before R2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleFiles {
    pub sample: String,
    pub paths: Vec<String>,
}

impl Manifest {
    pub fn cardinality(&self) -> Cardinality {
        match self {
            Manifest::PerSample(_) => Cardinality::PerSample,
            Manifest::PerDataset(_) => Cardinality::PerDataset,
        }
    }

    pub fn to_csv(&self) -> anyhow::Result<String> {
        let mut writer = csv::WriterBuilder::new()
            // Plain `\n` line endings, so output is identical on every OS.
            .terminator(csv::Terminator::Any(b'\n'))
            .from_writer(Vec::new());
        match self {
            Manifest::PerSample(samples) => {
                writer.write_record([SAMPLE_COLUMN, PATH_COLUMN])?;
                for sample in samples {
                    for path in &sample.paths {
                        writer.write_record([sample.sample.as_str(), path.as_str()])?;
                    }
                }
            }
            Manifest::PerDataset(paths) => {
                writer.write_record([PATH_COLUMN])?;
                for path in paths {
                    writer.write_record([path.as_str()])?;
                }
            }
        }
        let bytes = writer.into_inner().context("finishing manifest CSV")?;
        Ok(String::from_utf8(bytes)?)
    }

    /// Parse a manifest, checking it has the shape `expected` calls for.
    pub fn from_csv(text: &str, expected: Cardinality) -> anyhow::Result<Manifest> {
        let mut reader = csv::Reader::from_reader(text.as_bytes());
        let header: Vec<String> = reader.headers()?.iter().map(str::to_string).collect();
        let wanted: &[&str] = match expected {
            Cardinality::PerSample => &[SAMPLE_COLUMN, PATH_COLUMN],
            Cardinality::PerDataset => &[PATH_COLUMN],
        };
        ensure!(
            header == wanted,
            "manifest header is {header:?}, expected {wanted:?}"
        );

        let mut rows = Vec::new();
        for (index, record) in reader.records().enumerate() {
            let record = record?;
            let line = index + 2; // 1-based, after the header line
            let fields: Vec<&str> = record.iter().collect();
            if fields.iter().any(|field| field.is_empty()) {
                bail!("manifest line {line} has an empty field");
            }
            rows.push(
                fields
                    .iter()
                    .map(|field| field.to_string())
                    .collect::<Vec<_>>(),
            );
        }

        let manifest = match expected {
            Cardinality::PerDataset => {
                Manifest::PerDataset(rows.into_iter().map(|mut row| row.remove(0)).collect())
            }
            Cardinality::PerSample => {
                // Group rows by sample, keeping samples in first-seen order
                // and each sample's files in row order.
                let mut samples: Vec<SampleFiles> = Vec::new();
                for row in rows {
                    let [sample, path]: [String; 2] =
                        row.try_into().expect("header check guarantees two columns");
                    match samples.iter_mut().find(|entry| entry.sample == sample) {
                        Some(entry) => entry.paths.push(path),
                        None => samples.push(SampleFiles {
                            sample,
                            paths: vec![path],
                        }),
                    }
                }
                Manifest::PerSample(samples)
            }
        };
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired_reads() -> Manifest {
        Manifest::PerSample(vec![
            SampleFiles {
                sample: "S1".to_string(),
                paths: vec![
                    "/data/S1_R1.fq.gz".to_string(),
                    "/data/S1_R2.fq.gz".to_string(),
                ],
            },
            SampleFiles {
                sample: "S2".to_string(),
                paths: vec![
                    "/data/S2_R1.fq.gz".to_string(),
                    "/data/S2_R2.fq.gz".to_string(),
                ],
            },
        ])
    }

    #[test]
    fn writes_per_sample_csv() {
        assert_eq!(
            paired_reads().to_csv().unwrap(),
            "sample,path\n\
             S1,/data/S1_R1.fq.gz\n\
             S1,/data/S1_R2.fq.gz\n\
             S2,/data/S2_R1.fq.gz\n\
             S2,/data/S2_R2.fq.gz\n"
        );
    }

    #[test]
    fn per_sample_round_trips() {
        let manifest = paired_reads();
        let csv = manifest.to_csv().unwrap();
        assert_eq!(
            Manifest::from_csv(&csv, Cardinality::PerSample).unwrap(),
            manifest
        );
    }

    #[test]
    fn per_dataset_round_trips() {
        let manifest = Manifest::PerDataset(vec!["/r/a.zip".to_string(), "/r/b.zip".to_string()]);
        let csv = manifest.to_csv().unwrap();
        assert_eq!(csv, "path\n/r/a.zip\n/r/b.zip\n");
        assert_eq!(
            Manifest::from_csv(&csv, Cardinality::PerDataset).unwrap(),
            manifest
        );
    }

    #[test]
    fn paths_with_commas_are_quoted() {
        let manifest = Manifest::PerDataset(vec!["/data/run 1, final.zip".to_string()]);
        let csv = manifest.to_csv().unwrap();
        assert_eq!(csv, "path\n\"/data/run 1, final.zip\"\n");
        assert_eq!(
            Manifest::from_csv(&csv, Cardinality::PerDataset).unwrap(),
            manifest
        );
    }

    #[test]
    fn groups_rows_by_sample_in_first_seen_order() {
        let csv = "sample,path\nS2,b1\nS1,a1\nS2,b2\n";
        let manifest = Manifest::from_csv(csv, Cardinality::PerSample).unwrap();
        let Manifest::PerSample(samples) = manifest else {
            panic!("expected a per-sample manifest");
        };
        assert_eq!(samples[0].sample, "S2");
        assert_eq!(samples[0].paths, vec!["b1", "b2"]);
        assert_eq!(samples[1].sample, "S1");
    }

    #[test]
    fn rejects_the_wrong_shape() {
        let per_dataset = "path\n/r/a.zip\n";
        let err = Manifest::from_csv(per_dataset, Cardinality::PerSample).unwrap_err();
        assert!(
            err.to_string().contains("expected [\"sample\", \"path\"]"),
            "{err}"
        );
    }

    #[test]
    fn rejects_empty_fields() {
        let err =
            Manifest::from_csv("sample,path\n,/data/x.fq\n", Cardinality::PerSample).unwrap_err();
        assert!(
            err.to_string().contains("line 2 has an empty field"),
            "{err}"
        );
    }
}
