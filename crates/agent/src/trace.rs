//! Parsing Nextflow's `trace.tsv` into typed task records
//! (DESIGN.md §4.1, local mode step 4).
//!
//! The generated `nextflow.config` asks for these columns, with `raw = true`
//! so times are plain milliseconds. Columns are looked up by their header
//! name, never by position, so reordering the config can't silently shift
//! one field into another. Nextflow writes `-` for a value it doesn't have
//! (an aborted task has no exit code).

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, bail};
use protocol::result::{TaskRecord, TaskStatus};

pub fn parse(text: &str) -> anyhow::Result<Vec<TaskRecord>> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines
        .next()
        .context("trace file is empty")?
        .split('\t')
        .collect();
    let column: BTreeMap<&str, usize> = header
        .iter()
        .enumerate()
        .map(|(index, name)| (*name, index))
        .collect();
    for required in ["task_id", "hash", "name", "process", "status"] {
        if !column.contains_key(required) {
            bail!("trace file has no `{required}` column (columns: {header:?})");
        }
    }

    let mut tasks = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let line_number = index + 2; // 1-based, after the header
        // A closure that looks a column up in *this* row. `-` and missing
        // columns both become None.
        let get = |name: &str| -> Option<&str> {
            let value = *fields.get(*column.get(name)?)?;
            (value != "-" && !value.is_empty()).then_some(value)
        };
        let task = parse_row(&get).with_context(|| format!("trace line {line_number}"))?;
        tasks.push(task);
    }
    // Nextflow writes tasks in completion order; sort by id for a stable result.
    tasks.sort_by_key(|task| task.task_id);
    Ok(tasks)
}

fn parse_row<'a>(get: &dyn Fn(&str) -> Option<&'a str>) -> anyhow::Result<TaskRecord> {
    let required = |name: &str| get(name).with_context(|| format!("missing `{name}`"));
    Ok(TaskRecord {
        task_id: number(required("task_id")?, "task_id")?,
        hash: required("hash")?.to_string(),
        name: required("name")?.to_string(),
        process: required("process")?.to_string(),
        tag: get("tag").map(str::to_string),
        status: task_status(required("status")?),
        exit_code: get("exit").map(|v| number(v, "exit")).transpose()?,
        duration_ms: get("duration").map(|v| number(v, "duration")).transpose()?,
        realtime_ms: get("realtime").map(|v| number(v, "realtime")).transpose()?,
    })
}

/// Parse any number type. `T: FromStr` is a generic parameter: one
/// function serves u64 and i32 alike.
fn number<T: FromStr>(value: &str, column: &str) -> anyhow::Result<T> {
    value
        .parse()
        .map_err(|_| anyhow::anyhow!("`{column}` is not a number: `{value}`"))
}

fn task_status(value: &str) -> TaskStatus {
    match value {
        "COMPLETED" => TaskStatus::Completed,
        "CACHED" => TaskStatus::Cached,
        "FAILED" => TaskStatus::Failed,
        "ABORTED" => TaskStatus::Aborted,
        _ => TaskStatus::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real trace from a stub run of the fastqc → multiqc project
    /// (workdir shortened), plus one aborted task.
    const TRACE: &str = "\
task_id\thash\tname\tprocess\ttag\tstatus\texit\tsubmit\tstart\tcomplete\tduration\trealtime\tworkdir
2\t62/e47e5c\tFASTQC_fastqc_1 (S2)\tFASTQC_fastqc_1\tS2\tCOMPLETED\t0\t1794384930916\t1794384930991\t1794384931252\t336\t80\t/run/work/62/e47e5c
1\t67/657381\tFASTQC_fastqc_1 (S1)\tFASTQC_fastqc_1\tS1\tCOMPLETED\t0\t1794384930883\t1794384930892\t1794384931643\t760\t92\t/run/work/67/657381
3\td2/40f110\tMULTIQC_multiqc_1 (multiqc_1)\tMULTIQC_multiqc_1\tmultiqc_1\tABORTED\t-\t1794384931711\t-\t-\t-\t-\t/run/work/d2/40f110
";

    #[test]
    fn parses_a_real_trace() {
        let tasks = parse(TRACE).unwrap();

        assert_eq!(tasks.len(), 3);
        assert_eq!(
            tasks[0],
            TaskRecord {
                task_id: 1,
                hash: "67/657381".to_string(),
                name: "FASTQC_fastqc_1 (S1)".to_string(),
                process: "FASTQC_fastqc_1".to_string(),
                tag: Some("S1".to_string()),
                status: TaskStatus::Completed,
                exit_code: Some(0),
                duration_ms: Some(760),
                realtime_ms: Some(92),
            }
        );
    }

    #[test]
    fn dashes_become_missing_values() {
        let tasks = parse(TRACE).unwrap();
        let aborted = &tasks[2];
        assert_eq!(aborted.status, TaskStatus::Aborted);
        assert_eq!(aborted.exit_code, None);
        assert_eq!(aborted.duration_ms, None);
    }

    #[test]
    fn columns_are_found_by_name_not_position() {
        let reordered = "status\tname\ttask_id\tprocess\thash\nFAILED\tX (a)\t7\tX\tab/cdef01\n";
        let tasks = parse(reordered).unwrap();
        assert_eq!(tasks[0].task_id, 7);
        assert_eq!(tasks[0].status, TaskStatus::Failed);
        assert_eq!(tasks[0].exit_code, None, "no exit column at all");
    }

    #[test]
    fn rejects_traces_missing_required_columns() {
        let err = parse("task_id\tname\n1\tX\n").unwrap_err();
        assert!(err.to_string().contains("no `hash` column"), "{err}");
    }

    #[test]
    fn says_which_line_is_bad() {
        let bad = "task_id\thash\tname\tprocess\tstatus\nnot-a-number\tab/cd\tX\tX\tCOMPLETED\n";
        let err = format!("{:#}", parse(bad).unwrap_err());
        assert!(err.contains("trace line 2"), "{err}");
        assert!(err.contains("`task_id` is not a number"), "{err}");
    }
}
