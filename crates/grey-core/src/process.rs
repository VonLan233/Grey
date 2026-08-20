use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const DEFAULT_STDIN_LIMIT: usize = 1024 * 1024;
pub const DEFAULT_STDOUT_LIMIT: usize = 64 * 1024;
pub const DEFAULT_STDERR_LIMIT: usize = 64 * 1024;
pub const OUTPUT_TRUNCATED_MARKER: &str = "\n[output truncated by Grey]\n";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

impl CommandSpec {
    pub fn direct<I, S>(program: impl Into<OsString>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Vec::new(),
            stdin: Vec::new(),
            timeout: Duration::from_secs(120),
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    #[cfg(unix)]
    pub fn legacy_shell(command: impl Into<OsString>) -> Self {
        Self::direct("/bin/sh", [OsString::from("-c"), command.into()])
    }

    #[cfg(windows)]
    pub fn legacy_shell(command: impl Into<OsString>) -> Self {
        Self::direct(
            "cmd.exe",
            [
                OsString::from("/D"),
                OsString::from("/S"),
                OsString::from("/C"),
                command.into(),
            ],
        )
    }

    pub fn current_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = stdin.into();
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn stdout_limit(mut self, limit: usize) -> Self {
        self.stdout_limit = limit;
        self
    }

    pub fn stderr_limit(mut self, limit: usize) -> Self {
        self.stderr_limit = limit;
        self
    }
}

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl CommandOutput {
    pub fn stdout_lossy(&self) -> String {
        decode_output(&self.stdout, self.stdout_truncated)
    }

    pub fn combined_lossy(&self) -> String {
        let mut text = self.stdout_lossy();
        if !self.stderr.is_empty() || self.stderr_truncated {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&decode_output(&self.stderr, self.stderr_truncated));
        }
        text
    }
}

pub async fn run_bounded(spec: CommandSpec) -> Result<CommandOutput> {
    ensure!(
        !spec.program.is_empty(),
        "command program must not be empty"
    );
    ensure!(
        spec.stdin.len() <= DEFAULT_STDIN_LIMIT,
        "command stdin exceeds {DEFAULT_STDIN_LIMIT} bytes"
    );

    let mut command = CommandWrap::with_new(&spec.program, |command| {
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.env)
            .stdin(if spec.stdin.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = spec.cwd {
            command.current_dir(cwd);
        }
    });
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);

    let child = command.spawn().context("spawning command")?;
    let (cancel_on_drop, cancelled) = oneshot::channel();
    let supervisor = tokio::spawn(supervise(
        child,
        spec.stdin,
        spec.timeout,
        spec.stdout_limit,
        spec.stderr_limit,
        cancelled,
    ));
    let result = supervisor.await.context("command supervisor task failed")?;
    drop(cancel_on_drop);
    result
}

enum Completion {
    TimedOut,
    Cancelled,
}

type DrainTask = JoinHandle<std::io::Result<(Vec<u8>, bool)>>;
type StdinTask = JoinHandle<std::io::Result<()>>;

async fn supervise(
    mut child: Box<dyn ChildWrapper>,
    stdin: Vec<u8>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancelled: oneshot::Receiver<()>,
) -> Result<CommandOutput> {
    let stdout = child.stdout().take().context("opening command stdout")?;
    let stderr = child.stderr().take().context("opening command stderr")?;
    let mut stdout_task = tokio::spawn(drain_bounded(stdout, stdout_limit));
    let mut stderr_task = tokio::spawn(drain_bounded(stderr, stderr_limit));
    let mut stdin_task = child.stdin().take().map(|mut pipe| {
        tokio::spawn(async move {
            pipe.write_all(&stdin).await?;
            pipe.shutdown().await
        })
    });

    let completion = tokio::select! {
        output = collect_output(
            child.as_mut(),
            &mut stdout_task,
            &mut stderr_task,
            &mut stdin_task,
        ) => return output,
        _ = tokio::time::sleep(timeout) => Completion::TimedOut,
        _ = cancelled => Completion::Cancelled,
    };

    let stop_error = match completion {
        Completion::TimedOut => {
            anyhow::anyhow!("command timed out after {}ms", timeout.as_millis())
        }
        Completion::Cancelled => anyhow::anyhow!("command cancelled"),
    };
    let cleanup = tokio::time::timeout(CLEANUP_TIMEOUT, terminate_and_reap(child.as_mut())).await;
    abort_io_tasks(&stdout_task, &stderr_task, &stdin_task);
    match cleanup {
        Ok(Ok(_)) => Err(stop_error),
        Ok(Err(error)) => Err(error.context("cleaning up command process tree")),
        Err(_) => Err(anyhow::anyhow!(
            "command cleanup timed out after {}ms",
            CLEANUP_TIMEOUT.as_millis()
        )),
    }
}

async fn collect_output(
    child: &mut dyn ChildWrapper,
    stdout_task: &mut DrainTask,
    stderr_task: &mut DrainTask,
    stdin_task: &mut Option<StdinTask>,
) -> Result<CommandOutput> {
    let status = child.wait().await.context("waiting for command")?;
    collect_after_status(status, stdout_task, stderr_task, stdin_task).await
}

async fn collect_after_status(
    status: ExitStatus,
    stdout_task: &mut DrainTask,
    stderr_task: &mut DrainTask,
    stdin_task: &mut Option<StdinTask>,
) -> Result<CommandOutput> {
    let (stdout, stdout_truncated) = stdout_task
        .await
        .context("stdout drain task failed")?
        .context("reading command stdout")?;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .context("stderr drain task failed")?
        .context("reading command stderr")?;
    if let Some(stdin_task) = stdin_task {
        let stdin_result = stdin_task.await.context("stdin writer task failed")?;
        stdin_result.context("writing command stdin")?;
    }

    Ok(CommandOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn abort_io_tasks(
    stdout_task: &DrainTask,
    stderr_task: &DrainTask,
    stdin_task: &Option<StdinTask>,
) {
    stdout_task.abort();
    stderr_task.abort();
    if let Some(stdin_task) = stdin_task {
        stdin_task.abort();
    }
}

async fn terminate_and_reap(child: &mut dyn ChildWrapper) -> Result<ExitStatus> {
    if let Err(kill_error) = child.start_kill() {
        if child
            .try_wait()
            .context("checking command after failed termination")?
            .is_none()
        {
            return Err(kill_error).context("terminating command process tree");
        }
    }
    child.wait().await.context("reaping command process tree")
}

async fn drain_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let retained = read.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok((output, truncated))
}

fn decode_output(bytes: &[u8], truncated: bool) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        text.push_str(OUTPUT_TRUNCATED_MARKER);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{run_bounded, CommandSpec, DEFAULT_STDIN_LIMIT};

    const MODE_ENV: &str = "GREY_PROCESS_TEST_MODE";
    const PID_FILE_ENV: &str = "GREY_PROCESS_TEST_PID_FILE";
    #[cfg(unix)]
    const PARENT_ENV: &str = "HOME";
    #[cfg(windows)]
    const PARENT_ENV: &str = "USERPROFILE";

    fn helper(mode: &str) -> CommandSpec {
        CommandSpec::direct(
            std::env::current_exe().expect("current test executable"),
            [
                "--exact",
                "process::tests::process_test_helper",
                "--nocapture",
            ],
        )
        .env(MODE_ENV, mode)
    }

    async fn wait_for_pid_file(path: &std::path::Path) -> (u32, u32) {
        for _ in 0..200 {
            if let Ok(contents) = fs::read_to_string(path) {
                let mut pids = contents.split_whitespace();
                let parsed = (
                    pids.next().and_then(|pid| pid.parse().ok()),
                    pids.next().and_then(|pid| pid.parse().ok()),
                    pids.next(),
                );
                if let (Some(parent), Some(grandchild), None) = parsed {
                    return (parent, grandchild);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("PID file was not created at {}", path.display());
    }

    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args([OsString::from("-0"), OsString::from(pid.to_string())])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[cfg(windows)]
    fn process_is_alive(pid: u32) -> bool {
        let filter = format!("PID eq {pid}");
        Command::new("tasklist")
            .args(["/FI", &filter, "/NH"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }

    #[cfg(unix)]
    fn kill_process(pid: u32) {
        let _ = Command::new("/bin/kill")
            .args([OsString::from("-KILL"), OsString::from(pid.to_string())])
            .status();
    }

    #[cfg(windows)]
    fn kill_process(pid: u32) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }

    async fn wait_until_dead(pid: u32) -> bool {
        for _ in 0..200 {
            if !process_is_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn drains_both_pipes_and_caps_output() {
        let output = run_bounded(
            helper("flood-both")
                .stdout_limit(1024)
                .stderr_limit(2048)
                .timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap();

        assert_eq!(output.stdout.len(), 1024);
        assert_eq!(output.stderr.len(), 2048);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[tokio::test]
    async fn default_environment_does_not_inherit_parent_profile() {
        {
            let _guard = crate::test_support::test_env_lock();
            assert!(std::env::var_os(PARENT_ENV).is_some());
        }
        let output = run_bounded(helper("print-parent-env")).await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(
            stdout.contains("PARENT_ENV_IS_UNSET"),
            "stdout was {stdout:?}"
        );
    }

    #[tokio::test]
    async fn rejects_stdin_over_the_limit_before_spawn() {
        let error = run_bounded(
            CommandSpec::direct(
                "grey-process-command-must-not-exist",
                std::iter::empty::<&str>(),
            )
            .stdin(vec![b'x'; DEFAULT_STDIN_LIMIT + 1]),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("stdin exceeds"), "{error:#}");
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_child_and_grandchild() {
        let directory = tempdir().unwrap();
        let pid_file = directory.path().join("timeout-pids");
        let error = run_bounded(
            helper("parent")
                .env(PID_FILE_ENV, pid_file.as_os_str())
                .timeout(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
        let (parent, grandchild) = wait_for_pid_file(&pid_file).await;

        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(
            wait_until_dead(parent).await,
            "parent {parent} survived timeout"
        );
        assert!(
            wait_until_dead(grandchild).await,
            "grandchild {grandchild} survived timeout"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_child_and_grandchild() {
        let directory = tempdir().unwrap();
        let pid_file = directory.path().join("cancel-pids");
        let task = tokio::spawn(run_bounded(
            helper("parent")
                .env(PID_FILE_ENV, pid_file.as_os_str())
                .timeout(Duration::from_secs(30)),
        ));
        let (parent, grandchild) = wait_for_pid_file(&pid_file).await;

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            wait_until_dead(parent).await,
            "parent {parent} survived cancellation"
        );
        assert!(
            wait_until_dead(grandchild).await,
            "grandchild {grandchild} survived cancellation"
        );
    }

    #[tokio::test]
    async fn timeout_covers_pipes_held_by_descendant_after_leader_exit() {
        let directory = tempdir().unwrap();
        let pid_file = directory.path().join("orphan-timeout-pids");
        let completed = tokio::time::timeout(
            Duration::from_secs(1),
            run_bounded(
                helper("orphan-pipes")
                    .env(PID_FILE_ENV, pid_file.as_os_str())
                    .timeout(Duration::from_millis(100)),
            ),
        )
        .await;
        let (parent, grandchild) = wait_for_pid_file(&pid_file).await;
        if completed.is_err() {
            kill_process(grandchild);
            assert!(wait_until_dead(grandchild).await);
        }

        assert!(
            completed.is_ok(),
            "run_bounded ignored its pipe-drain deadline"
        );
        let error = completed.unwrap().unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(wait_until_dead(parent).await, "leader {parent} survived");
        assert!(
            wait_until_dead(grandchild).await,
            "descendant {grandchild} survived pipe-drain timeout"
        );
    }

    #[tokio::test]
    async fn timeout_handles_one_drain_finishing_before_the_other() {
        let directory = tempdir().unwrap();
        let pid_file = directory.path().join("orphan-stderr-timeout-pids");
        let completed = tokio::time::timeout(
            Duration::from_secs(1),
            run_bounded(
                helper("orphan-stderr")
                    .env(PID_FILE_ENV, pid_file.as_os_str())
                    .timeout(Duration::from_millis(100)),
            ),
        )
        .await;
        let (_, grandchild) = wait_for_pid_file(&pid_file).await;
        if completed.is_err() {
            kill_process(grandchild);
            assert!(wait_until_dead(grandchild).await);
        }

        assert!(completed.is_ok(), "run_bounded hung after a partial drain");
        let error = completed.unwrap().unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(
            wait_until_dead(grandchild).await,
            "descendant {grandchild} survived partial-drain timeout"
        );
    }

    #[tokio::test]
    async fn cancellation_covers_pipes_held_after_leader_exit() {
        let directory = tempdir().unwrap();
        let pid_file = directory.path().join("orphan-cancel-pids");
        let task = tokio::spawn(run_bounded(
            helper("orphan-pipes")
                .env(PID_FILE_ENV, pid_file.as_os_str())
                .timeout(Duration::from_secs(30)),
        ));
        let (parent, grandchild) = wait_for_pid_file(&pid_file).await;
        assert!(
            wait_until_dead(parent).await,
            "leader {parent} did not exit"
        );

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let descendant_was_reaped = wait_until_dead(grandchild).await;
        if !descendant_was_reaped {
            kill_process(grandchild);
            assert!(wait_until_dead(grandchild).await);
        }
        assert!(
            descendant_was_reaped,
            "descendant {grandchild} survived caller cancellation after leader exit"
        );
    }

    #[test]
    fn process_test_helper() {
        let _lock = crate::test_support::test_env_lock();
        let Some(mode) = std::env::var_os(MODE_ENV) else {
            return;
        };
        match mode.to_str().unwrap() {
            "flood-both" => {
                let stdout =
                    thread::spawn(|| std::io::stdout().write_all(&vec![b'o'; 2 * 1024 * 1024]));
                let stderr =
                    thread::spawn(|| std::io::stderr().write_all(&vec![b'e'; 2 * 1024 * 1024]));
                stdout.join().unwrap().unwrap();
                stderr.join().unwrap().unwrap();
            }
            "print-parent-env" => {
                if std::env::var_os(PARENT_ENV).is_none() {
                    print!("PARENT_ENV_IS_UNSET");
                } else {
                    print!("PARENT_ENV_WAS_INHERITED");
                }
            }
            "parent" => {
                let executable = std::env::current_exe().unwrap();
                let mut grandchild = Command::new(executable);
                grandchild
                    .args([
                        "--exact",
                        "process::tests::process_test_helper",
                        "--nocapture",
                    ])
                    .env_clear()
                    .env(MODE_ENV, "grandchild");
                let mut grandchild = grandchild.spawn().unwrap();
                let path = std::env::var_os(PID_FILE_ENV).unwrap();
                fs::write(path, format!("{} {}", std::process::id(), grandchild.id())).unwrap();
                grandchild.wait().unwrap();
            }
            "orphan-pipes" => {
                let executable = std::env::current_exe().unwrap();
                // Intentionally orphan the helper so the supervisor must close inherited pipes.
                #[allow(clippy::zombie_processes)]
                let grandchild = Command::new(executable)
                    .args([
                        "--exact",
                        "process::tests::process_test_helper",
                        "--nocapture",
                    ])
                    .env_clear()
                    .env(MODE_ENV, "grandchild")
                    .spawn()
                    .unwrap();
                let path = std::env::var_os(PID_FILE_ENV).unwrap();
                fs::write(path, format!("{} {}", std::process::id(), grandchild.id())).unwrap();
            }
            "orphan-stderr" => {
                let executable = std::env::current_exe().unwrap();
                // Intentionally orphan the helper so only its inherited stderr stays open.
                #[allow(clippy::zombie_processes)]
                let grandchild = Command::new(executable)
                    .args([
                        "--exact",
                        "process::tests::process_test_helper",
                        "--nocapture",
                    ])
                    .env_clear()
                    .env(MODE_ENV, "grandchild")
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let path = std::env::var_os(PID_FILE_ENV).unwrap();
                fs::write(path, format!("{} {}", std::process::id(), grandchild.id())).unwrap();
            }
            "grandchild" => thread::sleep(Duration::from_secs(30)),
            unexpected => panic!("unexpected helper mode {unexpected}"),
        }
    }
}
