//! The real binaries as OS processes for the e2e run.
//!
//! Each process starts with a cleared environment plus exactly the variables the run
//! gives it, so no ambient provider key or proxy setting leaks in. Its stdout and stderr go
//! to `<name>.log` in the run's artifact directory. [`Process::terminate`] sends SIGTERM,
//! as the deploy does, and waits for the exit. A process still running when it is dropped
//! is killed.
//!
//! [`FailureReport`] prints the tail of every process log when the test panics, with JSON
//! log lines pretty-printed, so a broken step reads as a report and not as a bare assert.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How many log lines a failure report shows per process.
pub const TAIL_LINES: usize = 200;

/// The run's artifact directory, `<root>/target/e2e/<UTC timestamp>/`, and the logs in it.
#[derive(Clone)]
pub struct Artifacts {
    dir: PathBuf,
    logs: Arc<Mutex<Vec<(String, PathBuf)>>>,
}

impl Artifacts {
    /// Creates `<workspace>/target/e2e/<timestamp>/`.
    ///
    /// # Errors
    /// When the directory cannot be created.
    pub fn create(workspace: &Path) -> io::Result<Self> {
        Self::create_named(workspace, "e2e")
    }

    /// Creates `<workspace>/target/<kind>/<timestamp>/`.
    ///
    /// # Errors
    /// When the directory cannot be created.
    pub fn create_named(workspace: &Path, kind: &str) -> io::Result<Self> {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
        let dir = workspace.join("target").join(kind).join(stamp);
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            logs: Arc::default(),
        })
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes `name` into the directory.
    ///
    /// # Errors
    /// When the file cannot be written.
    pub fn write(&self, name: &str, contents: &str) -> io::Result<PathBuf> {
        let path = self.dir.join(name);
        fs::write(&path, contents)?;
        Ok(path)
    }

    fn log_path(&self, name: &str) -> PathBuf {
        let path = self.dir.join(format!("{name}.log"));
        self.logs
            .lock()
            .expect("log list")
            .push((name.to_owned(), path.clone()));
        path
    }

    /// Every process log so far: `(process name, text)`.
    #[must_use]
    pub fn logs(&self) -> Vec<(String, String)> {
        self.logs
            .lock()
            .expect("log list")
            .iter()
            .map(|(name, path)| (name.clone(), fs::read_to_string(path).unwrap_or_default()))
            .collect()
    }
}

/// One spawned binary.
pub struct Process {
    name: String,
    child: Child,
    log: PathBuf,
}

impl Process {
    /// Starts `program` with `args` and only `env`, logging to `<name>.log`.
    ///
    /// # Errors
    /// When the log file cannot be created or the program cannot start.
    pub fn spawn(
        artifacts: &Artifacts,
        name: &str,
        program: &Path,
        args: &[&str],
        env: &[(String, String)],
    ) -> io::Result<Self> {
        let log = artifacts.log_path(name);
        let out = File::create(&log)?;
        let err = out.try_clone()?;
        let child = Command::new(program)
            .args(args)
            .env_clear()
            .envs(
                env.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()?;
        Ok(Self {
            name: name.to_owned(),
            child,
            log,
        })
    }

    /// The process name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The OS process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Its log so far.
    #[must_use]
    pub fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The CPU time (user + system) it has used, from `/proc/<pid>/stat`. `None` off Linux or
    /// when the file cannot be read.
    #[must_use]
    pub fn cpu_seconds(&self) -> Option<f64> {
        let stat = fs::read_to_string(format!("/proc/{}/stat", self.child.id())).ok()?;
        // Fields after the `(comm)` entry, which may itself hold spaces: utime and stime
        // are fields 14 and 15 of the whole line, so 12 and 13 after it.
        let rest = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let ticks: f64 =
            fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
        Some(ticks / clock_ticks_per_second())
    }

    /// Waits for the process to exit by itself.
    ///
    /// # Errors
    /// When it is still running after `timeout`.
    pub async fn wait(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() >= deadline => {
                    return Err(format!("{} did not exit within {timeout:?}", self.name));
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(error) => return Err(format!("{}: waiting: {error}", self.name)),
            }
        }
    }

    /// Sends SIGTERM and waits for the exit.
    ///
    /// # Errors
    /// When the signal cannot be sent or the process outlives `timeout`.
    pub async fn terminate(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .map_err(|error| format!("{}: sending SIGTERM: {error}", self.name))?;
        if !sent.success() {
            return Err(format!("{}: kill -TERM failed: {sent}", self.name));
        }
        self.wait(timeout).await
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `getconf CLK_TCK`, or Linux's usual 100.
fn clock_ticks_per_second() -> f64 {
    Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(100.0)
}

/// A free local port. Another process can take it before it is used, which is fine for a
/// test on one machine.
///
/// # Panics
/// When no port can be bound.
#[must_use]
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("a free port")
}

/// Waits until `url` answers any HTTP response.
///
/// # Errors
/// When nothing answers within `timeout`.
pub async fn wait_ready(url: &str, timeout: Duration) -> Result<Duration, String> {
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| error.to_string())?;
    loop {
        if client.get(url).send().await.is_ok() {
            return Ok(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return Err(format!("{url} did not answer within {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The last `lines` lines of a log, JSON lines pretty-printed.
#[must_use]
pub fn tail(log: &str, lines: usize) -> String {
    let all: Vec<&str> = log.lines().collect();
    let mut out = String::new();
    for line in &all[all.len().saturating_sub(lines)..] {
        match serde_json::from_str::<Value>(line) {
            Ok(value @ Value::Object(_)) => {
                let pretty = serde_json::to_string_pretty(&value).unwrap_or_default();
                let _ = writeln!(out, "{pretty}");
            }
            _ => {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    out
}

/// A note computed when the report prints.
type Note = Box<dyn Fn() -> String + Send>;

/// Prints every process log's tail, and any extra notes, when dropped during a panic.
pub struct FailureReport {
    artifacts: Artifacts,
    notes: Mutex<Vec<Note>>,
}

impl FailureReport {
    /// A report over the run's artifacts.
    #[must_use]
    pub fn new(artifacts: &Artifacts) -> Self {
        Self {
            artifacts: artifacts.clone(),
            notes: Mutex::default(),
        }
    }

    /// Adds a note computed at failure time (for example the fake provider's misses).
    pub fn note(&self, note: impl Fn() -> String + Send + 'static) {
        self.notes.lock().expect("notes").push(Box::new(note));
    }
}

impl Drop for FailureReport {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        eprintln!(
            "\n##### e2e FAILED; artifacts in {} #####",
            self.artifacts.dir().display()
        );
        for note in self.notes.lock().expect("notes").iter() {
            eprintln!("{}", note());
        }
        for (name, log) in self.artifacts.logs() {
            eprintln!("\n----- {name}.log (last {TAIL_LINES} lines) -----");
            eprintln!("{}", tail(&log, TAIL_LINES));
        }
    }
}
