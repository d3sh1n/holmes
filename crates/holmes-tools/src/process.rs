//! Bounded subprocess execution with process-group termination (AGT-002/AGT-003).
//!
//! `run_shell` spawns a command in its own process group (Unix) with `kill_on_drop`
//! and races it against a deadline and an optional cancellation token. On timeout or
//! cancellation the whole process group receives `SIGKILL` and the direct child is
//! awaited (reaped), so a command that forked grandchildren leaves nothing behind —
//! this is what the plain `tokio::time::timeout(cmd.output())` pattern got wrong:
//! dropping the output future left the child (and its tree) running.
//!
//! Platform support (P1-01): process-group termination is implemented for Unix only
//! and that is the supported target — CI runs Linux exclusively and `libc` is a
//! `cfg(unix)` dependency. On non-Unix builds there is no process-tree equivalent
//! (a correct implementation needs Windows Job Objects, out of scope for what this
//! environment can verify); the fallback is `kill_on_drop`, which still terminates
//! the direct child on every exit path but cannot reclaim forked grandchildren.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// How a bounded subprocess run ended.
pub enum ProcessRun {
    /// The process exited on its own before the deadline/cancellation.
    Completed(std::process::Output),
    /// The deadline fired; the process group was killed and the child reaped.
    TimedOut,
    /// The cancellation token fired; the process group was killed and the child reaped.
    Cancelled,
    /// The process could not be spawned or awaited.
    Failed(String),
}

impl ProcessRun {
    pub fn completed(self) -> Option<std::process::Output> {
        match self {
            Self::Completed(output) => Some(output),
            _ => None,
        }
    }
}

/// Run `sh -c <command>` (or `$SHELL -c` when `shell` is provided) with an optional
/// stdin payload, bounded by `timeout` and `cancel`.
pub async fn run_shell(
    command: &str,
    stdin_payload: Option<Vec<u8>>,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> ProcessRun {
    run_shell_via("sh", command, stdin_payload, timeout, cancel).await
}

/// Variant of [`run_shell`] using an explicit shell binary (e.g. `$SHELL` for hooks).
pub async fn run_shell_via(
    shell: &str,
    command: &str,
    stdin_payload: Option<Vec<u8>>,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> ProcessRun {
    let mut cmd = Command::new(shell);
    cmd.arg("-c").arg(command);
    run_command(cmd, stdin_payload, timeout, cancel).await
}

/// Run an arbitrary prepared `Command` bounded by `timeout` and `cancel`. The command
/// is spawned in its own process group (Unix) with piped stdio and `kill_on_drop`.
pub async fn run_command(
    mut cmd: Command,
    stdin_payload: Option<Vec<u8>>,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> ProcessRun {
    use tokio::io::AsyncWriteExt;

    cmd.kill_on_drop(true);
    // Own process group so the timeout path can SIGKILL the whole tree (children and
    // grandchildren the command forked), not just the direct child.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return ProcessRun::Failed(format!("spawn failed: {e}")),
    };
    let pid = child.id();

    if let Some(payload) = stdin_payload {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&payload).await;
            // Drop closes stdin so the child sees EOF.
        }
    }

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout.take() {
            let _ = out.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut err) = stderr.take() {
            let _ = err.read_to_end(&mut buf).await;
        }
        buf
    });

    let wait = child.wait();
    tokio::pin!(wait);
    let cancelled = async {
        match cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };

    enum End {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }
    let end = tokio::select! {
        status = &mut wait => End::Exited(status),
        _ = tokio::time::sleep(timeout) => End::TimedOut,
        _ = cancelled => End::Cancelled,
    };

    let outcome = match end {
        End::Exited(Ok(status)) => {
            let stdout = stdout_reader.await.unwrap_or_default();
            let stderr = stderr_reader.await.unwrap_or_default();
            ProcessRun::Completed(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        End::Exited(Err(e)) => {
            stdout_reader.abort();
            stderr_reader.abort();
            ProcessRun::Failed(format!("wait failed: {e}"))
        }
        End::TimedOut | End::Cancelled => {
            let reason = if matches!(end, End::TimedOut) {
                "deadline"
            } else {
                "cancellation"
            };
            terminate_group(pid);
            // Reap the direct child (grandchildren are SIGKILLed via the group and
            // reparented away; tokio's orphan reaper handles them).
            let _ = wait.await;
            holmes_core::metrics::metrics().count("process.killed");
            info!(
                pid = pid.unwrap_or(0),
                reason,
                event = "ProcessKilled",
                "process group terminated and reaped"
            );
            // Readers finish at EOF once the group is dead; whatever was captured is
            // discarded — a killed run reports no partial output.
            stdout_reader.abort();
            stderr_reader.abort();
            if matches!(end, End::TimedOut) {
                ProcessRun::TimedOut
            } else {
                ProcessRun::Cancelled
            }
        }
    };
    outcome
}

/// SIGKILL the process group led by `pid` (Unix). Falls back to nothing elsewhere —
/// `kill_on_drop` still covers the direct child on non-Unix platforms.
fn terminate_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // Safety: killpg with an already-exited group leader is a no-op (ESRCH), so a
        // race with natural exit is harmless. The pgid is the child's own pid because
        // it was spawned with process_group(0).
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn pid_alive(pid: u32) -> bool {
        // kill(pid, 0): succeeds (or EPERM) while the process exists, ESRCH once gone.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[tokio::test]
    async fn completes_and_captures_output() {
        let run = run_shell(
            "echo out; echo err >&2; exit 3",
            None,
            Duration::from_secs(5),
            None,
        )
        .await;
        let output = run.completed().expect("completed");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
        assert_eq!(output.status.code(), Some(3));
    }

    #[tokio::test]
    async fn timeout_kills_entire_process_group() {
        // Spawn a shell that forks a grandchild sleep, records both pids, then blocks.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pids");
        let command = format!(
            "sleep 300 & echo $! > {}; echo $$ >> {}; sleep 300",
            pidfile.display(),
            pidfile.display()
        );
        let start = std::time::Instant::now();
        let run = run_shell(&command, None, Duration::from_millis(300), None).await;
        let elapsed = start.elapsed();
        assert!(matches!(run, ProcessRun::TimedOut));
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");

        let pids: Vec<u32> = std::fs::read_to_string(&pidfile)
            .expect("pidfile written before timeout")
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();
        assert_eq!(pids.len(), 2, "grandchild and shell pids recorded");

        // Give SIGKILL a moment to settle, then both must be gone — no orphans.
        tokio::time::sleep(Duration::from_millis(100)).await;
        for pid in pids {
            assert!(!pid_alive(pid), "pid {pid} survived group termination");
        }
    }

    #[tokio::test]
    async fn cancellation_kills_process_group() {
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });
        let run = run_shell("sleep 300", None, Duration::from_secs(60), Some(&token)).await;
        assert!(matches!(run, ProcessRun::Cancelled));
    }

    #[tokio::test]
    async fn stdin_payload_is_delivered() {
        let run = run_shell(
            "cat",
            Some(b"hello-stdin".to_vec()),
            Duration::from_secs(5),
            None,
        )
        .await;
        let output = run.completed().expect("completed");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello-stdin"
        );
    }

    #[tokio::test]
    async fn spawn_failure_is_reported() {
        let cmd = Command::new("/nonexistent/holmes-no-such-binary");
        let run = run_command(cmd, None, Duration::from_secs(1), None).await;
        assert!(matches!(run, ProcessRun::Failed(_)));
    }
}
