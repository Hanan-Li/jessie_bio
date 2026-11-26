//! Process supervision: run Nextflow and see it through to the end
//! (DESIGN.md §4.1, local mode step 3).
//!
//! While the child runs, three things happen at once:
//! - its stdout is copied, line by line, to `logs/stdout.log` (and the terminal)
//! - its stderr is copied the same way to `logs/stderr.log`
//! - we wait for whichever comes first: the child exits, or `stop` fires
//!
//! Both streams must be read at the same time. A pipe holds only ~64 KB:
//! if we read stdout to the end before touching stderr and the child fills
//! the stderr pipe, it blocks writing while we block reading, forever.
//!
//! Stopping is graceful first: SIGTERM lets Nextflow run its own shutdown,
//! which stops the task containers it started. If it hasn't exited after
//! `grace`, or the caller insists (`force`, e.g. a second Ctrl-C), the
//! whole process group gets SIGKILL: Nextflow and every process it started.
//! That can still leave containers behind, which Docker, not the process
//! group, owns.

use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::stage::StagedRun;

/// Why a run was asked to stop before it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Someone asked for this job to stop: Ctrl-C in `agent run`, a
    /// cancelled run in `agent serve`.
    Cancelled,
    TimedOut,
    /// The agent itself is being shut down (SIGTERM, e.g. from systemd).
    /// The job didn't fail and wasn't cancelled: it should be retried.
    Terminated,
    /// `agent serve` only: the control plane answered a heartbeat with
    /// 409, so another attempt holds the job now (a network partition,
    /// say). The work is discarded and nothing is reported.
    LeaseLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The process ended on its own. `code` is `None` if a signal we didn't
    /// send killed it; `signal` then says which.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// We stopped it. `forced` means it ignored SIGTERM for the whole grace
    /// period and was killed.
    Stopped { reason: StopReason, forced: bool },
}

/// Where a stream's lines are echoed, besides its log file.
#[derive(Debug, Clone, Copy)]
pub enum Echo {
    Stdout,
    Stderr,
    Off,
}

/// Run `command` to completion, or until `stop` fires. Once stopping, the
/// child gets `grace` to exit, cut short if `force` fires.
///
/// `stop` is a future (a "thing that will happen later") supplied by the
/// caller: Ctrl-C or a timeout from the CLI, a plain timer in tests, and
/// in P0c a cancel message from the control plane. Supervision doesn't
/// need to know which. `impl Future<...>` means "any type that is a future
/// with this output"; the compiler generates one version of this function
/// per type used.
pub async fn supervise(
    mut command: Command,
    logs_dir: &Path,
    echo: bool,
    stop: impl Future<Output = StopReason>,
    force: impl Future<Output = ()>,
    grace: Duration,
) -> anyhow::Result<Outcome> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A process group of its own: Ctrl-C in the terminal then reaches
        // only the agent, which decides how to stop the child, instead of
        // hitting both at once.
        .process_group(0)
        // If the agent itself panics or this future is dropped, don't
        // leave the child running.
        .kill_on_drop(true);
    let mut child = command.spawn().context("starting the child process")?;

    let (out_echo, err_echo) = if echo {
        (Echo::Stdout, Echo::Stderr)
    } else {
        (Echo::Off, Echo::Off)
    };
    // `take()` moves each pipe out of `child`, so the tasks own them.
    // A spawned task may run on another thread and outlive this function,
    // so it must own everything it uses: hence the owned PathBufs.
    let stdout = child.stdout.take().context("stdout was not piped")?;
    let stderr = child.stderr.take().context("stderr was not piped")?;
    let stdout_task = tokio::spawn(copy_lines(stdout, logs_dir.join("stdout.log"), out_echo));
    let stderr_task = tokio::spawn(copy_lines(stderr, logs_dir.join("stderr.log"), err_echo));

    // `select!` polls both futures and runs the branch of whichever
    // finishes first; the other is dropped. `stop` is used by reference,
    // which requires it to stay at one memory address: `pin!` does that.
    tokio::pin!(stop);
    let outcome = tokio::select! {
        status = child.wait() => {
            let status = status.context("waiting for the child process")?;
            Outcome::Exited { code: status.code(), signal: status.signal() }
        }
        reason = &mut stop => {
            let forced = stop_gracefully(&mut child, grace, force).await?;
            Outcome::Stopped { reason, forced }
        }
    };

    // The pipes close when the child exits, which ends both copy tasks.
    // `.await` on a JoinHandle gives Result<task's result, JoinError>.
    stdout_task.await.context("stdout task panicked")??;
    stderr_task.await.context("stderr task panicked")??;
    Ok(outcome)
}

/// SIGTERM to the child, then SIGKILL to its whole process group if it's
/// still running after `grace` or when `force` fires. Returns whether
/// SIGKILL was needed.
async fn stop_gracefully(
    child: &mut Child,
    grace: Duration,
    force: impl Future<Output = ()>,
) -> anyhow::Result<bool> {
    let Some(pid) = child.id() else {
        return Ok(false); // already exited and reaped
    };
    let pid = Pid::from_raw(i32::try_from(pid)?);
    // Only Nextflow gets SIGTERM: it knows how to stop its own tasks.
    ignore_exited(kill(pid, Signal::SIGTERM)).context("sending SIGTERM")?;
    tokio::select! {
        status = child.wait() => {
            status.context("waiting for the child after SIGTERM")?;
            return Ok(false);
        }
        _ = tokio::time::sleep(grace) => {}
        _ = force => {}
    }
    // The child was started with `process_group(0)`, so its process group
    // id is its pid. Signalling the group reaches Nextflow *and* every
    // process it started (task shells, `docker run` clients), which would
    // otherwise survive it and keep our log pipes open.
    ignore_exited(killpg(pid, Signal::SIGKILL)).context("sending SIGKILL")?;
    child
        .wait()
        .await
        .context("waiting for the child after SIGKILL")?;
    Ok(true)
}

/// ESRCH ("no such process") means it exited just now: not an error.
fn ignore_exited(result: nix::Result<()>) -> nix::Result<()> {
    match result {
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        other => other,
    }
}

/// Copy a stream to a log file line by line, optionally echoing each line.
/// Works on raw bytes, so a non-UTF-8 line can't stop the copy.
async fn copy_lines(
    stream: impl AsyncRead + Unpin,
    log: PathBuf,
    echo: Echo,
) -> anyhow::Result<()> {
    let mut file = File::create(&log)
        .await
        .with_context(|| format!("creating {}", log.display()))?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            break; // end of stream: the child closed its end
        }
        file.write_all(&line).await?;
        let text = String::from_utf8_lossy(&line);
        match echo {
            Echo::Stdout => print!("{text}"),
            Echo::Stderr => eprint!("{text}"),
            Echo::Off => {}
        }
    }
    file.flush().await?;
    Ok(())
}

/// The `nextflow run` command for a staged job, started from its run
/// directory (where Nextflow writes `trace.tsv` and its `.nextflow/` state).
/// `program` is `nextflow` except in tests; `version` becomes `NXF_VER`
/// (the pinned version locally, the compute environment's in `serve`).
pub fn nextflow_command(staged: &StagedRun, stub: bool, program: &Path, version: &str) -> Command {
    let mut command = Command::new(program);
    command
        .args(staged.nextflow_args(stub))
        .current_dir(&staged.run_dir)
        .env("NXF_VER", version)
        // Plain line-by-line progress instead of a redrawn terminal UI,
        // which would fill the log with escape codes.
        .env("NXF_ANSI_LOG", "false");
    command
}

/// A future that never completes: the "no timeout" case.
pub async fn sleep_or_forever(limit: Option<Duration>) {
    match limit {
        Some(limit) => tokio::time::sleep(limit).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// A stop that never fires: the child decides when it's done.
    async fn never() -> StopReason {
        std::future::pending().await
    }

    /// A `force` that never fires: stopping waits out the grace period.
    async fn no_force() {
        std::future::pending().await
    }

    async fn after(millis: u64, reason: StopReason) -> StopReason {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        reason
    }

    const GRACE: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn logs_both_streams_and_the_exit_code() {
        let logs = tempfile::tempdir().unwrap();
        let script = "echo out1; echo err1 >&2; echo out2; exit 3";

        let outcome = supervise(
            shell(script),
            logs.path(),
            false,
            never(),
            no_force(),
            GRACE,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            Outcome::Exited {
                code: Some(3),
                signal: None
            }
        );
        let read = |name: &str| std::fs::read_to_string(logs.path().join(name)).unwrap();
        assert_eq!(read("stdout.log"), "out1\nout2\n");
        assert_eq!(read("stderr.log"), "err1\n");
    }

    #[tokio::test]
    async fn drains_both_pipes_without_deadlocking() {
        // 1 MB to stderr first, then 1 MB to stdout: far more than a pipe
        // holds. Reading stdout first, alone, would hang here forever.
        let logs = tempfile::tempdir().unwrap();
        let script = "head -c 1000000 /dev/zero >&2; head -c 1000000 /dev/zero";

        let run = supervise(
            shell(script),
            logs.path(),
            false,
            never(),
            no_force(),
            GRACE,
        );
        let outcome = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("supervise deadlocked")
            .unwrap();

        assert!(matches!(outcome, Outcome::Exited { code: Some(0), .. }));
        let size = |name: &str| std::fs::metadata(logs.path().join(name)).unwrap().len();
        assert_eq!(size("stderr.log"), 1_000_000);
        assert_eq!(size("stdout.log"), 1_000_000);
    }

    #[tokio::test]
    async fn stop_sends_sigterm_and_the_child_exits() {
        let logs = tempfile::tempdir().unwrap();
        let started = Instant::now();

        let outcome = supervise(
            shell("exec sleep 30"),
            logs.path(),
            false,
            after(100, StopReason::TimedOut),
            no_force(),
            GRACE,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            Outcome::Stopped {
                reason: StopReason::TimedOut,
                forced: false
            }
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "SIGTERM was not enough"
        );
    }

    #[tokio::test]
    async fn a_child_ignoring_sigterm_is_killed_after_the_grace_period() {
        // `trap '' TERM` ignores SIGTERM; `exec` keeps that for `sleep`.
        let logs = tempfile::tempdir().unwrap();
        let started = Instant::now();

        let outcome = supervise(
            shell("trap '' TERM; exec sleep 30"),
            logs.path(),
            false,
            after(100, StopReason::Cancelled),
            no_force(),
            Duration::from_millis(300),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            Outcome::Stopped {
                reason: StopReason::Cancelled,
                forced: true
            }
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn sigkill_reaches_the_whole_process_group() {
        // The shell ignores SIGTERM and starts a background `sleep`, which
        // inherits that and also holds our stdout pipe. Killing only the
        // shell would leave `sleep` running for 30 s, holding the pipe
        // open, and supervise would wait for it.
        let logs = tempfile::tempdir().unwrap();
        let started = Instant::now();

        let run = supervise(
            shell("trap '' TERM; sleep 30 & wait"),
            logs.path(),
            false,
            after(100, StopReason::Cancelled),
            no_force(),
            Duration::from_millis(300),
        );
        let outcome = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("a process the group kill missed kept supervise waiting")
            .unwrap();

        assert!(matches!(outcome, Outcome::Stopped { forced: true, .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn force_skips_the_rest_of_the_grace_period() {
        let logs = tempfile::tempdir().unwrap();
        let started = Instant::now();

        let outcome = supervise(
            shell("trap '' TERM; exec sleep 30"),
            logs.path(),
            false,
            after(100, StopReason::Terminated),
            async { tokio::time::sleep(Duration::from_millis(200)).await },
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            Outcome::Stopped {
                reason: StopReason::Terminated,
                forced: true
            }
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited out the grace period"
        );
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_is_an_error() {
        let logs = tempfile::tempdir().unwrap();
        let command = Command::new("definitely-not-a-real-program");
        let err = supervise(command, logs.path(), false, never(), no_force(), GRACE)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("starting the child process"));
    }
}
