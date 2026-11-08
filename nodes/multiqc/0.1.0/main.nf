// Copyright (c) the nf-core community under an open-source MIT license.
// See https://github.com/nf-core/modules for full license, file patching instructions and upstream contributing.

process MULTIQC {
    tag "${meta.id}"
    label 'process_single'

    // Pinned by digest (upstream uses the tag only, and supports Singularity/conda).
    container "community.wave.seqera.io/library/multiqc:1.35--c17fb751507e9dfc@sha256:ae074e961979ea85c72c6c67a8ed432fa3368c736b8fe37799f7223901ac84ab"

    input:
    // Upstream also takes optional config, logo, replace_names and sample_names
    // files. Removed: each input here must map to one port on the node.
    tuple val(meta), path(multiqc_files, stageAs: "?/*")

    output:
    tuple val(meta), path("*.html"), emit: report
    tuple val(meta), path("*_data"), emit: data
    tuple val(meta), path("*_plots"), emit: plots, optional: true
    // MultiQC should not push its versions to the `versions` topic. Its input depends on the versions topic to be resolved thus outputting to the topic will let the pipeline hang forever
    tuple val("${task.process}"), val('multiqc'), eval('multiqc --version | sed "s/.* //g"'), emit: versions

    script:
    def args = task.ext.args ?: ''
    def prefix = task.ext.prefix ? "--filename ${task.ext.prefix}.html" : ''
    """
    multiqc \\
        --force \\
        ${args} \\
        ${prefix} \\
        .
    """

    stub:
    """
    mkdir multiqc_data
    touch multiqc_data/.stub
    mkdir multiqc_plots
    touch multiqc_plots/.stub
    touch multiqc_report.html
    """
}
