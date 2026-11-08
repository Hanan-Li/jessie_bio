# Upstream: nf-core/modules `fastqc`

- Source: https://github.com/nf-core/modules/tree/97915125c4707e959ec797a321a6b2584bfe8b29/modules/nf-core/fastqc
- Commit: `97915125c4707e959ec797a321a6b2584bfe8b29` (2026-11-07)
- Tool: FastQC 0.13.0
- License: MIT, copyright the nf-core community. Full text: [`../../LICENSE-nf-core-modules`](../../LICENSE-nf-core-modules)

## Local changes

- `container`: pinned to the image digest
  (`sha256:5dbe9d091f1cbd37fa044ce34bd01da53ad1254b83c00946a107a53e9166f1be`,
  linux/amd64) instead of the tag alone. Removed the Singularity/Apptainer
  alternative and the `conda` directive: the platform runs Docker only.
- Removed the `when:` section (`task.ext.when`), deprecated in Nextflow 26
  (`nextflow lint` warns). The platform does not switch processes off through
  config.

## Ports

- Input `reads` → the process's only input, `tuple val(meta), path(reads)`.
- Outputs `html`, `zip` → the `emit:` names of the same name. The
  `versions_fastqc` emit is not a port.
