//! Preflight: can this machine run jobs? (DESIGN.md §4.1, local mode step 1)
//!
//! Checks Java, Nextflow and (unless it's a stub run) Docker. The checks
//! are independent, so they run at the same time.

use std::io::ErrorKind;
use std::process::Output;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Every job runs on this Nextflow version. Setting `NXF_VER` makes the
/// Nextflow launcher use exactly this version (downloading it if needed).
pub const NEXTFLOW_VERSION: &str = "26.10.5";
pub const MIN_JAVA_MAJOR: u32 = 17;

/// Generous, because `nextflow -version` may first download the pinned
/// version.
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreflightError {
    #[error("`{tool}` is not installed or not on PATH")]
    Missing { tool: String },

    #[error("`{tool}` could not be started: {reason}")]
    CouldNotStart { tool: String, reason: String },

    #[error("`{tool}` did not answer within {seconds}s")]
    TimedOut { tool: String, seconds: u64 },

    #[error("could not read {tool}'s version from its output")]
    UnknownVersion { tool: String },

    #[error("Java {found} is too old: Nextflow needs Java {min} or newer", min = MIN_JAVA_MAJOR)]
    JavaTooOld { found: u32 },

    #[error("Nextflow {found} answered, but jobs are pinned to {pinned}", pinned = NEXTFLOW_VERSION)]
    WrongNextflow { found: String },

    #[error("Docker is installed but not usable: {0}")]
    DockerUnusable(String),
}

/// What preflight found, for the agent to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub java_major: u32,
    pub nextflow: String,
    /// `None` when Docker wasn't checked (stub runs don't use it).
    pub docker: Option<String>,
}

pub async fn check(need_docker: bool) -> Result<Environment, Vec<PreflightError>> {
    let docker = async {
        if need_docker {
            Some(check_docker().await)
        } else {
            None
        }
    };
    // `join!` polls all three futures concurrently and waits for all of
    // them, like Futures.allAsList. Nothing runs until here: the three
    // futures above were only *created*.
    let (java, nextflow, docker) = tokio::join!(check_java(), check_nextflow(), docker);

    let mut errors = Vec::new();
    let java = java.map_err(|error| errors.push(error)).ok();
    let nextflow = nextflow.map_err(|error| errors.push(error)).ok();
    let docker = match docker {
        None => None,
        Some(Ok(version)) => Some(version),
        Some(Err(error)) => {
            errors.push(error);
            None
        }
    };

    match (java, nextflow) {
        (Some(java_major), Some(nextflow)) if errors.is_empty() => Ok(Environment {
            java_major,
            nextflow,
            docker,
        }),
        _ => Err(errors),
    }
}

async fn check_java() -> Result<u32, PreflightError> {
    let mut command = Command::new("java");
    command.arg("-version");
    let output = run_tool("java", command).await?;
    // `java -version` prints to stderr, for historical reasons.
    let text = String::from_utf8_lossy(&output.stderr);
    let major = parse_java_major(&text).ok_or_else(|| PreflightError::UnknownVersion {
        tool: "java".to_string(),
    })?;
    if major < MIN_JAVA_MAJOR {
        return Err(PreflightError::JavaTooOld { found: major });
    }
    Ok(major)
}

async fn check_nextflow() -> Result<String, PreflightError> {
    let mut command = Command::new("nextflow");
    command.arg("-version").env("NXF_VER", NEXTFLOW_VERSION);
    let output = run_tool("nextflow", command).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = parse_nextflow_version(&text).ok_or_else(|| PreflightError::UnknownVersion {
        tool: "nextflow".to_string(),
    })?;
    if version != NEXTFLOW_VERSION {
        return Err(PreflightError::WrongNextflow { found: version });
    }
    Ok(version)
}

async fn check_docker() -> Result<String, PreflightError> {
    let mut command = Command::new("docker");
    // `docker info` talks to the daemon, so it also catches "installed but
    // this user may not use it" (the docker group problem).
    command.args(["info", "--format", "{{.ServerVersion}}"]);
    let output = run_tool("docker", command).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("no error message").trim();
        return Err(PreflightError::DockerUnusable(first_line.to_string()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a tool to completion, with a time limit.
async fn run_tool(tool: &str, mut command: Command) -> Result<Output, PreflightError> {
    // If the timeout fires, the `output()` future is dropped. Dropping it
    // only stops *waiting*; `kill_on_drop` makes it also kill the process,
    // so a hung tool doesn't outlive the check.
    command.kill_on_drop(true);
    match timeout(TOOL_TIMEOUT, command.output()).await {
        Err(_elapsed) => Err(PreflightError::TimedOut {
            tool: tool.to_string(),
            seconds: TOOL_TIMEOUT.as_secs(),
        }),
        Ok(Err(error)) if error.kind() == ErrorKind::NotFound => Err(PreflightError::Missing {
            tool: tool.to_string(),
        }),
        Ok(Err(error)) => Err(PreflightError::CouldNotStart {
            tool: tool.to_string(),
            reason: error.to_string(),
        }),
        Ok(Ok(output)) => Ok(output),
    }
}

/// `openjdk version "17.0.20.1" 2026-08-18` → 17. Old Javas report
/// `"1.8.0_392"`, which is Java 8.
fn parse_java_major(text: &str) -> Option<u32> {
    let quoted = text.split('"').nth(1)?;
    let mut parts = quoted.split('.');
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// Finds `version 26.10.5 build 14654` in Nextflow's banner → "26.10.5".
fn parse_nextflow_version(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    words.find(|&word| word == "version")?;
    words.next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_modern_java_versions() {
        let banner = "openjdk version \"17.0.20.1\" 2026-08-18\nOpenJDK Runtime Environment\n";
        assert_eq!(parse_java_major(banner), Some(17));
        assert_eq!(
            parse_java_major("openjdk version \"21\" 2023-09-19"),
            Some(21)
        );
    }

    #[test]
    fn reads_old_style_java_versions() {
        assert_eq!(parse_java_major("java version \"1.8.0_392\""), Some(8));
    }

    #[test]
    fn unreadable_java_version_is_none() {
        assert_eq!(parse_java_major("something else entirely"), None);
    }

    #[test]
    fn reads_the_nextflow_banner() {
        let banner = "\n      N E X T F L O W\n      version 26.10.5 build 14654\n      created 01-10-2026\n";
        assert_eq!(parse_nextflow_version(banner).as_deref(), Some("26.10.5"));
        assert_eq!(parse_nextflow_version("no banner here"), None);
    }

    #[test]
    fn error_messages_name_the_fix() {
        assert_eq!(
            PreflightError::JavaTooOld { found: 11 }.to_string(),
            "Java 11 is too old: Nextflow needs Java 17 or newer"
        );
        assert_eq!(
            PreflightError::WrongNextflow {
                found: "25.04.0".to_string()
            }
            .to_string(),
            "Nextflow 25.04.0 answered, but jobs are pinned to 26.10.5"
        );
    }
}
