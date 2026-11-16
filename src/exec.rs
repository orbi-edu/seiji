//! Running host commands for a tenant.
//!
//! In `os_user` isolation every command goes through `sudo -n <helper> exec-as <account> -- ...`,
//! which runs it as the tenant's macOS account inside that account's launchd domain (so
//! CoreSimulator uses the tenant's own device set). In `shared` isolation commands run directly
//! as the daemon's account. Either way each command leads its own process group so it can be
//! killed as a whole on timeout, cancellation or lease release.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, IsolationMode};

/// Captured stdout/stderr is capped here; the rest is drained and dropped.
const MAX_CAPTURE: usize = 32 * 1024 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Who a command runs as and where its files live.
#[derive(Debug, Clone)]
pub struct Account {
    /// Seiji user name.
    pub name: String,
    /// macOS account (`os_user` isolation) or `None` (shared).
    pub os_user: Option<String>,
    /// Root of the tenant's files: `~<os_user>/seiji` or `<dataDir>/tenants/<name>`.
    pub workspace: PathBuf,
}

impl Account {
    pub fn lease_dir(&self, lease_id: &str) -> PathBuf {
        self.workspace.join("leases").join(lease_id)
    }
}

#[derive(Debug, Default)]
pub struct CmdSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub stdin_file: Option<PathBuf>,
}

impl CmdSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), ..Default::default() }
    }
    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
    pub fn stdin_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.stdin_file = Some(path.into());
        self
    }
    /// `/bin/sh -c <script> sh <args...>`: arguments arrive as `$1..`, never interpolated.
    pub fn sh<I, S>(script: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self::new("/bin/sh").arg("-c").arg(script).arg("sh").args(args)
    }
}

#[derive(Debug)]
pub struct Output {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn text(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.stdout).into_owned();
        if !self.stderr.is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&String::from_utf8_lossy(&self.stderr));
        }
        s
    }

    /// Error carrying the command's output unless it exited 0.
    pub fn check(self, what: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        let why = if self.timed_out {
            "timed out".to_string()
        } else if self.cancelled {
            "cancelled".to_string()
        } else {
            match self.code {
                Some(c) => format!("exited {c}"),
                None => "killed".to_string(),
            }
        };
        bail!("{what} {why}: {}", tail(&self.text(), 4000))
    }
}

/// The first `max` bytes of `s`, cut at a char boundary.
pub fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Result of [`Runner::run_to_file`].
#[derive(Debug)]
pub struct FileCopy {
    pub bytes: u64,
    pub truncated: bool,
    pub code: Option<i32>,
    pub stderr: String,
}

/// A spawned command that keeps running (an emulator).
pub struct Job {
    child: Child,
    pgid: Pgid,
}

enum Pgid {
    /// Shared mode: our direct child leads its group.
    Direct(i32),
    /// os_user mode: `exec-drop` writes the leader's pid here after `setsid`.
    PidFile(PathBuf),
}

impl Job {
    pub fn try_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Pgid::PidFile(p) = &self.pgid {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub struct Runner {
    mode: IsolationMode,
    helper: PathBuf,
    /// Tenants write their process-group ids here: mode 1733, so they can create files but not
    /// list or remove anyone else's.
    pid_dir: PathBuf,
    base_path: String,
}

impl Runner {
    pub fn new(config: &Config) -> Result<Self> {
        let pid_dir = config.data_dir.join("run").join("pids");
        std::fs::create_dir_all(&pid_dir).with_context(|| format!("creating {}", pid_dir.display()))?;
        std::fs::set_permissions(&pid_dir, std::fs::Permissions::from_mode(0o1733))?;
        // Tenants must traverse `<dataDir>/run` to reach `pids`, and nothing more.
        for dir in [&config.data_dir, &config.data_dir.join("run")] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711))?;
        }
        Ok(Self {
            mode: config.isolation.mode,
            helper: config.isolation.helper.clone(),
            pid_dir,
            base_path: "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
        })
    }

    fn command(&self, account: &Account, spec: &CmdSpec, pidfile: Option<&Path>) -> Result<Command> {
        let mut cmd = match (self.mode, &account.os_user) {
            (IsolationMode::OsUser, Some(os_user)) => {
                let mut cmd = Command::new("/usr/bin/sudo");
                cmd.arg("-n").arg(&self.helper).arg("exec-as").arg(os_user);
                if let Some(p) = pidfile {
                    cmd.arg("--pidfile").arg(p);
                }
                if let Some(dir) = &spec.cwd {
                    cmd.arg("--cwd").arg(dir);
                }
                cmd.arg("--env").arg(format!("PATH={}", self.base_path));
                for (k, v) in &spec.env {
                    cmd.arg("--env").arg(format!("{k}={v}"));
                }
                cmd.arg("--").arg(&spec.program).args(&spec.args);
                cmd
            }
            (IsolationMode::OsUser, None) => bail!("user {:?} has no macOS account (os_user isolation)", account.name),
            (IsolationMode::Shared, _) => {
                let mut cmd = Command::new(&spec.program);
                cmd.args(&spec.args);
                cmd.env("PATH", &self.base_path);
                for (k, v) in &spec.env {
                    cmd.env(k, v);
                }
                if let Some(dir) = &spec.cwd {
                    cmd.current_dir(dir);
                }
                cmd
            }
        };
        cmd.process_group(0).kill_on_drop(true);
        cmd.stdin(match &spec.stdin_file {
            Some(path) => {
                Stdio::from(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?)
            }
            None => Stdio::null(),
        });
        Ok(cmd)
    }

    fn pidfile(&self) -> PathBuf {
        self.pid_dir.join(format!("{}.pid", uuid::Uuid::new_v4()))
    }

    fn spawn_job(&self, account: &Account, spec: &CmdSpec, stdout: Stdio, stderr: Stdio) -> Result<Job> {
        let pidfile = matches!(self.mode, IsolationMode::OsUser).then(|| self.pidfile());
        let mut cmd = self.command(account, spec, pidfile.as_deref())?;
        cmd.stdout(stdout).stderr(stderr);
        let child = cmd.spawn().with_context(|| format!("spawning {}", spec.program.display()))?;
        let pgid = match pidfile {
            Some(p) => Pgid::PidFile(p),
            None => Pgid::Direct(child.id().context("child exited before reporting a pid")? as i32),
        };
        Ok(Job { child, pgid })
    }

    /// Runs a command to completion, killing its process group on timeout or cancellation.
    pub async fn run(
        &self,
        account: &Account,
        spec: CmdSpec,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Output> {
        let mut job = self.spawn_job(account, &spec, Stdio::piped(), Stdio::piped())?;
        let stdout = job.child.stdout.take().map(|s| tokio::spawn(capture(s)));
        let stderr = job.child.stderr.take().map(|s| tokio::spawn(capture(s)));
        let (mut timed_out, mut cancelled) = (false, false);
        let status = tokio::select! {
            status = job.child.wait() => Some(status?),
            _ = tokio::time::sleep(timeout) => { timed_out = true; None }
            _ = cancel.cancelled() => { cancelled = true; None }
        };
        let status = match status {
            Some(s) => s,
            None => {
                self.kill(account, &mut job).await;
                job.child.wait().await?
            }
        };
        drop(job);
        let stdout = match stdout {
            Some(h) => h.await??,
            None => Vec::new(),
        };
        let stderr = match stderr {
            Some(h) => h.await??,
            None => Vec::new(),
        };
        Ok(Output { code: status.code(), stdout, stderr, timed_out, cancelled })
    }

    /// Runs a command whose stdout is written to `dest` (daemon-owned), at most `max_bytes`.
    /// Past the cap the command is killed and the copy is marked truncated.
    pub async fn run_to_file(
        &self,
        account: &Account,
        spec: CmdSpec,
        dest: &Path,
        max_bytes: u64,
        timeout: Duration,
    ) -> Result<FileCopy> {
        use tokio::io::AsyncWriteExt;
        let mut job = self.spawn_job(account, &spec, Stdio::piped(), Stdio::piped())?;
        let mut stdout = job.child.stdout.take().context("no stdout pipe")?;
        let stderr = job.child.stderr.take().map(|s| tokio::spawn(capture(s)));
        let mut file = tokio::fs::File::create(dest).await.with_context(|| format!("creating {}", dest.display()))?;
        let copy = async {
            let mut buf = vec![0u8; 256 * 1024];
            let mut bytes: u64 = 0;
            loop {
                let n = stdout.read(&mut buf).await?;
                if n == 0 {
                    return anyhow::Ok((bytes, false));
                }
                let room = max_bytes.saturating_sub(bytes);
                let take = (n as u64).min(room) as usize;
                file.write_all(&buf[..take]).await?;
                bytes += take as u64;
                if take < n {
                    return Ok((bytes, true));
                }
            }
        };
        let (bytes, truncated, timed_out) = match tokio::time::timeout(timeout, copy).await {
            Ok(r) => {
                let (b, t) = r?;
                (b, t, false)
            }
            Err(_) => (0, true, true),
        };
        file.flush().await?;
        if truncated {
            self.kill(account, &mut job).await;
        }
        let status = job.child.wait().await?;
        drop(job);
        let stderr = match stderr {
            Some(h) => String::from_utf8_lossy(&h.await??).into_owned(),
            None => String::new(),
        };
        if timed_out {
            bail!("copying {} timed out", dest.display());
        }
        Ok(FileCopy { bytes, truncated, code: status.code(), stderr })
    }

    /// Starts a long-running command whose stdout the caller reads (a screen stream). Its stderr
    /// is discarded.
    pub fn spawn_piped(&self, account: &Account, spec: CmdSpec) -> Result<(Job, tokio::process::ChildStdout)> {
        let mut job = self.spawn_job(account, &spec, Stdio::piped(), Stdio::null())?;
        let stdout = job.child.stdout.take().context("no stdout pipe")?;
        Ok((job, stdout))
    }

    /// Starts a long-running command with output appended to `log`.
    pub fn spawn(&self, account: &Account, spec: CmdSpec, log: &Path) -> Result<Job> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("opening {}", log.display()))?;
        self.spawn_job(account, &spec, Stdio::from(file.try_clone()?), Stdio::from(file))
    }

    /// SIGTERM the job's process group, SIGKILL after a grace period.
    pub async fn kill(&self, account: &Account, job: &mut Job) {
        self.stop(account, job, libc::SIGTERM, KILL_GRACE).await;
    }

    /// SIGINT the job's process group and give it `grace` to finish its output (a video
    /// recorder writes the movie's index on SIGINT), then SIGKILL.
    pub async fn interrupt(&self, account: &Account, job: &mut Job, grace: Duration) {
        self.stop(account, job, libc::SIGINT, grace).await;
    }

    async fn stop(&self, account: &Account, job: &mut Job, first: i32, grace: Duration) {
        if job.try_exited() {
            return;
        }
        let Some(pgid) = self.resolve_pgid(job).await else {
            let _ = job.child.start_kill();
            return;
        };
        self.signal(account, pgid, first).await;
        if tokio::time::timeout(grace, job.child.wait()).await.is_err() {
            self.signal(account, pgid, libc::SIGKILL).await;
            let _ = job.child.start_kill();
            let _ = job.child.wait().await;
        }
    }

    async fn resolve_pgid(&self, job: &mut Job) -> Option<i32> {
        let path = match &job.pgid {
            Pgid::Direct(p) => return Some(*p),
            Pgid::PidFile(path) => path.clone(),
        };
        // exec-drop writes the file right before exec; wait briefly if we raced it.
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&path)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                return Some(pid);
            }
            if job.try_exited() {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    async fn signal(&self, account: &Account, pgid: i32, sig: i32) {
        match (self.mode, &account.os_user) {
            (IsolationMode::OsUser, Some(os_user)) => {
                let out = Command::new("/usr/bin/sudo")
                    .arg("-n")
                    .arg(&self.helper)
                    .arg("kill-as")
                    .arg(os_user)
                    .arg(pgid.to_string())
                    .arg(sig.to_string())
                    .stdin(Stdio::null())
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() => {}
                    Ok(o) => {
                        tracing::warn!(pgid, sig, "kill-as refused: {}", String::from_utf8_lossy(&o.stderr).trim())
                    }
                    Err(e) => tracing::warn!(pgid, sig, "kill-as did not start: {e}"),
                }
            }
            _ => unsafe {
                libc::killpg(pgid, sig);
            },
        }
    }
}

async fn capture(mut r: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok(out);
        }
        // Keep the head; past the cap keep draining so the child never blocks on a full pipe.
        let room = MAX_CAPTURE.saturating_sub(out.len());
        out.extend_from_slice(&buf[..n.min(room)]);
    }
}
