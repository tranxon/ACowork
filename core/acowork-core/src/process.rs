//! Process management utilities with idle-timeout monitoring.
//!
//! Provides [`run_with_idle_timeout`] and [`run_command_with_idle_timeout`]
//! for running child processes that should be killed if they produce no
//! stdout/stderr output for a configurable duration.
//!
//! # Idle timeout vs absolute timeout
//!
//! An **absolute timeout** kills the process after a fixed wall-clock duration,
//! regardless of whether it is making progress. This forces the caller to guess
//! a "reasonable" upper bound, which is fragile — a slow network can make a
//! 5-minute download take 10 minutes.
//!
//! An **idle timeout** only kills the process when it has produced *zero output*
//! for the specified duration. As long as the process keeps printing to stdout
//! or stderr, the timer resets. This means:
//!
//! - A `curl` download printing progress bars every second → never times out
//! - A `cargo build` streaming compiler output → never times out
//! - An `npm install` stuck on a TCP handshake with 60s of silence → killed
//!
//! # Example
//!
//! ```rust,no_run
//! use acowork_core::process::{run_command_with_idle_timeout, ProcessOutput};
//! use std::time::Duration;
//!
//! # async fn example() {
//! let mut cmd = tokio::process::Command::new("bash");
//! cmd.arg("some_install_script.sh");
//!
//! match run_command_with_idle_timeout(&mut cmd, Duration::from_secs(120)).await {
//!     Ok(ProcessOutput { exit_code, stdout, stderr }) => {
//!         println!("Script finished with exit code: {:?}", exit_code);
//!     }
//!     Err(e) => {
//!         eprintln!("Script timed out: {e}");
//!         // e.stdout() and e.stderr() contain partial output captured before timeout
//!     }
//! }
//! # }
//! ```

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

// ── Public types ──────────────────────────────────────────────────────────

/// Output captured from a completed child process.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Exit code, or `None` if terminated by a signal.
    pub exit_code: Option<i32>,
    /// Full stdout captured line-by-line (lines separated by `\n`).
    pub stdout: String,
    /// Full stderr captured line-by-line (lines separated by `\n`).
    pub stderr: String,
}

/// Error returned when a process exceeds the idle timeout.
///
/// Contains any stdout/stderr captured *before* the timeout fired,
/// which is useful for diagnosing what the process was doing when it got stuck.
#[derive(Debug, Clone)]
pub struct IdleTimeoutError {
    /// How long the process was idle before being killed.
    pub idle_secs: u64,
    /// Partial stdout captured before timeout.
    pub stdout: String,
    /// Partial stderr captured before timeout.
    pub stderr: String,
}

impl std::fmt::Display for IdleTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Process idle timeout after {}s with no output",
            self.idle_secs
        )
    }
}

impl std::error::Error for IdleTimeoutError {}

impl IdleTimeoutError {
    /// Access partial stdout captured before the timeout.
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    /// Access partial stderr captured before the timeout.
    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}

// ── Internal channel type ─────────────────────────────────────────────────

/// A line of output from either stdout or stderr.
enum OutputLine {
    Stdout(String),
    Stderr(String),
}

// ── Public API ────────────────────────────────────────────────────────────

/// Run an already-spawned [`Child`] process with idle-timeout monitoring.
///
/// The child **must** have its stdout and stderr set to `Stdio::piped()`.
/// Use [`run_command_with_idle_timeout`] for a convenience wrapper that
/// configures this automatically.
///
/// # How it works
///
/// Two background tasks read stdout and stderr line-by-line, sending each
/// line through an mpsc channel. The main loop does a `select!` between
/// receiving the next line and an idle timer:
///
/// - **Line received** → appended to the output buffer, timer resets
/// - **Channel closed** (both readers reached EOF) → process finished, wait for exit code
/// - **Timer fires** → process killed, partial output returned as error
///
/// # Panics
///
/// Panics if `child.stdout` or `child.stderr` is `None` (i.e. not piped).
pub async fn run_with_idle_timeout(
    mut child: Child,
    idle_timeout: Duration,
) -> Result<ProcessOutput, IdleTimeoutError> {
    let stdout = child
        .stdout
        .take()
        .expect("run_with_idle_timeout requires child.stdout to be Stdio::piped()");
    let stderr = child
        .stderr
        .take()
        .expect("run_with_idle_timeout requires child.stderr to be Stdio::piped()");

    let (tx, mut rx) = mpsc::unbounded_channel::<OutputLine>();

    // Spawn a task to read stdout line-by-line.
    let tx_stdout = tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx_stdout.send(OutputLine::Stdout(line)).is_err() {
                break; // Receiver dropped — parent task finished
            }
        }
    });

    // Spawn a task to read stderr line-by-line.
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx.send(OutputLine::Stderr(line)).is_err() {
                break; // Receiver dropped
            }
        }
    });

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    loop {
        // Wrap channel recv in a timeout — each successful recv resets the
        // timer implicitly because the next loop iteration creates a fresh timeout.
        match tokio::time::timeout(idle_timeout, rx.recv()).await {
            // A line arrived before the timeout — reset happens on next iteration.
            Ok(Some(OutputLine::Stdout(line))) => {
                stdout_buf.push_str(&line);
                stdout_buf.push('\n');
            }
            Ok(Some(OutputLine::Stderr(line))) => {
                stderr_buf.push_str(&line);
                stderr_buf.push('\n');
            }
            // Channel closed — both readers reached EOF, process finished.
            Ok(None) => {
                break;
            }
            // No output for `idle_timeout` — kill the process.
            Err(_elapsed) => {
                tracing::warn!(
                    idle_secs = idle_timeout.as_secs(),
                    "Process idle timeout — killing child process"
                );
                let _ = child.kill().await;
                // Abort reader tasks rather than joining them — the killed
                // process may have grandchildren still holding stdout/stderr
                // open, which would block the readers indefinitely.
                stdout_task.abort();
                stderr_task.abort();
                return Err(IdleTimeoutError {
                    idle_secs: idle_timeout.as_secs(),
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                });
            }
        }
    }

    // Both readers finished — wait for them and the process.
    let _ = tokio::join!(stdout_task, stderr_task);

    let status = child
        .wait()
        .await
        .map_err(|e| IdleTimeoutError {
            idle_secs: 0,
            stdout: format!("Failed to wait for child process: {e}"),
            stderr: String::new(),
        })?;

    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Convenience wrapper: configure a [`Command`] for idle-timeout monitoring
/// and spawn it.
///
/// Automatically sets `stdout(Stdio::piped())`, `stderr(Stdio::piped())`,
/// and `kill_on_drop(true)`, then calls [`run_with_idle_timeout`].
///
/// # Example
///
/// ```rust,no_run
/// use acowork_core::process::run_command_with_idle_timeout;
/// use std::time::Duration;
///
/// # async fn example() {
/// let mut cmd = tokio::process::Command::new("echo");
/// cmd.arg("hello");
/// let output = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(5))
///     .await
///     .unwrap();
/// assert_eq!(output.stdout.trim(), "hello");
/// # }
/// ```
pub async fn run_command_with_idle_timeout(
    cmd: &mut Command,
    idle_timeout: Duration,
) -> Result<ProcessOutput, IdleTimeoutError> {
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| IdleTimeoutError {
        idle_secs: 0,
        stdout: String::new(),
        stderr: format!("Failed to spawn process: {e}"),
    })?;

    run_with_idle_timeout(child, idle_timeout).await
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Short idle timeout for tests — keeps test suite fast.
    const TEST_IDLE: Duration = Duration::from_secs(2);

    // ── Helper ─────────────────────────────────────────────────────────

    fn bash_cmd(script: &str) -> Command {
        let mut cmd = Command::new("bash");
        cmd.args(["-c", script]);
        cmd
    }

    // ── Normal exit ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_normal_exit_success() {
        let mut cmd = bash_cmd("echo hello");
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_normal_exit_failure() {
        let mut cmd = bash_cmd("exit 1");
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally (even with non-zero exit)");
        assert_eq!(output.exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_empty_output() {
        let mut cmd = bash_cmd("true"); // produces no output, exits 0 immediately
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.is_empty() || output.stdout == "\n");
    }

    // ── Idle timeout ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_idle_timeout_no_output() {
        // `sleep 120` produces zero output — should trigger idle timeout.
        let mut cmd = bash_cmd("sleep 120");
        let start = tokio::time::Instant::now();
        let result = run_command_with_idle_timeout(&mut cmd, TEST_IDLE).await;
        let elapsed = start.elapsed();

        match result {
            Err(e) => {
                assert_eq!(e.idle_secs, TEST_IDLE.as_secs());
                // Should fire close to the idle timeout, not 120s.
                assert!(elapsed < Duration::from_secs(5));
            }
            Ok(_) => panic!("Expected idle timeout, but process completed"),
        }
    }

    #[tokio::test]
    async fn test_output_then_hang() {
        // Print one line, then hang — should timeout and capture the line.
        let mut cmd = bash_cmd("echo 'started setup...'; sleep 120");
        let result = run_command_with_idle_timeout(&mut cmd, TEST_IDLE).await;

        match result {
            Err(e) => {
                assert!(e.stdout.contains("started setup"));
                assert_eq!(e.idle_secs, TEST_IDLE.as_secs());
            }
            Ok(_) => panic!("Expected idle timeout"),
        }
    }

    // ── Long-running with continuous output ────────────────────────────

    #[tokio::test]
    async fn test_continuous_output_no_timeout() {
        // Print 10 lines at 0.3s intervals — total 3s > idle timeout 2s,
        // but should NOT timeout because output is continuous.
        let script = r#"
            for i in $(seq 1 10); do
                echo "line $i"
                sleep 0.3
            done
        "#;
        let mut cmd = bash_cmd(script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should NOT timeout — output is continuous");

        assert_eq!(output.exit_code, Some(0));
        // All 10 lines should be present.
        for i in 1..=10 {
            assert!(
                output.stdout.contains(&format!("line {i}")),
                "Missing line {i} in output: {}",
                output.stdout
            );
        }
    }

    // ── stderr ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stderr_captured() {
        let mut cmd = bash_cmd("echo 'to stdout'; echo 'to stderr' >&2");
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stdout.contains("to stdout"));
        assert!(output.stderr.contains("to stderr"));
    }

    #[tokio::test]
    async fn test_stderr_only() {
        let mut cmd = bash_cmd("echo 'error message' >&2");
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stderr.contains("error message"));
        // stdout may be empty or just a newline.
    }

    // ── Mixed stdout/stderr interleaving ───────────────────────────────

    #[tokio::test]
    async fn test_mixed_stdout_stderr() {
        // Interleave stdout and stderr output.
        let script = r#"
            echo "out1"
            echo "err1" >&2
            echo "out2"
            echo "err2" >&2
        "#;
        let mut cmd = bash_cmd(script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stdout.contains("out1"));
        assert!(output.stdout.contains("out2"));
        assert!(output.stderr.contains("err1"));
        assert!(output.stderr.contains("err2"));
    }

    // ── stdout EOF before stderr ───────────────────────────────────────

    #[tokio::test]
    async fn test_stdout_eof_before_stderr() {
        // stdout closes first, stderr continues briefly.
        let script = r#"
            echo "out"           # stdout
            exec 1>&-            # close stdout
            sleep 0.5
            echo "err" >&2       # stderr still open
        "#;
        let mut cmd = bash_cmd(script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stdout.contains("out"));
        assert!(output.stderr.contains("err"));
    }

    // ── Large output ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_large_output() {
        // Generate 1000 lines — should not OOM or lose data.
        let script = r#"
            for i in $(seq 1 1000); do
                echo "line $i"
            done
        "#;
        let mut cmd = bash_cmd(script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert_eq!(output.exit_code, Some(0));
        let line_count = output.stdout.lines().count();
        assert_eq!(line_count, 1000);
    }

    // ── kill_on_drop ───────────────────────────────────────────────────

    /// Verify that `tokio::time::timeout` + `UnboundedReceiver::recv()` works
    /// as expected: after consuming the only message, the next recv should
    /// time out (channel still open, no messages).
    #[tokio::test]
    async fn test_timeout_pattern_sanity() {
        let (tx, mut rx) = mpsc::unbounded_channel::<i32>();

        // Send one message, then nothing.
        tx.send(1).unwrap();

        // First recv — should get the message immediately.
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(result.is_ok(), "first recv should succeed");
        assert_eq!(result.unwrap(), Some(1));

        // Second recv — no messages, channel still open (tx alive).
        // Should time out after ~2s.
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "second recv should time out");
        assert!(
            elapsed < Duration::from_secs(4),
            "timeout should fire in ~2s, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_kill_on_drop() {
        // Verify that dropping the future kills the child process.
        // We spawn a long-sleeping process, drop the future, and verify
        // the process is gone.
        let mut cmd = Command::new("sleep");
        cmd.arg("120")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().expect("should spawn");
        let pid = child.id().expect("should have pid");

        // Wrap in a task we can abort.
        let handle = tokio::spawn(run_with_idle_timeout(child, Duration::from_secs(60)));

        // Give it a moment to start, then drop.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();

        // Give the OS a moment to reap the process.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify the process is gone.
        let still_running = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        assert!(
            !still_running,
            "Child process {pid} should have been killed on drop"
        );
    }
}
