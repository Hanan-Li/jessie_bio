# Upstream: nf-core/modules `multiqc`

- Source: https://github.com/nf-core/modules/tree/97915125c4707e959ec797a321a6b2584bfe8b29/modules/nf-core/multiqc
- Commit: `97915125c4707e959ec797a321a6b2584bfe8b29` (2026-11-07)
- Tool: MultiQC 1.35
- License: MIT, copyright the nf-core community. Full text: [`../../LICENSE-nf-core-modules`](../../LICENSE-nf-core-modules)

## Local changes

- `container`: pinned to the image digest
  (`sha256:ae074e961979ea85c72c6c67a8ed432fa3368c736b8fe37799f7223901ac84ab`,
  linux/amd64) instead of the tag alone. Removed the Singularity/Apptainer
  alternative and the `conda` directive: the platform runs Docker only.
- Removed the `when:` section (`task.ext.when`), deprecated in Nextflow 26
  (`nextflow lint` warns). The platform does not switch processes off through
  config.
- Input: upstream takes `multiqc_files` plus optional `multiqc_config`,
  `multiqc_logo`, `replace_names` and `sample_names` files in one tuple. Only
  `multiqc_files` is kept, with the matching lines of the `script:` block
  removed, so the process input maps to the node's single input port.

## Ports

- Input `multiqc_files` → `tuple val(meta), path(multiqc_files)`. It is
  per-dataset, so codegen must supply a dataset-level `meta` alongside the
  collected files.
- Output `report` → `emit: report`. The `data`, `plots` and `versions` emits
  are not ports.
