//! The quine relay as a ring of steps, the events it produces while running,
//! the shared state that new subscribers are bootstrapped from, and the runner
//! that drives the ring forever in replay or exec mode.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, watch};

use crate::languages::QuineLanguageSpec;

/// Number of finished steps kept in the snapshot history.
pub const HISTORY_LEN: usize = 256;
/// Capacity of the broadcast channel feeding SSE subscribers.
pub const CHANNEL_CAPACITY: usize = 1024;
/// Maximum length of the `error` string in a `step_done` event.
const MAX_ERROR_LEN: usize = 300;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

/// One step of the relay: the language that turns `input` into `output`.
#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub index: usize,
    pub name: String,
    pub input: String,
    pub output: String,
    /// Size in bytes of the reference input program.
    pub size: u64,
    pub has_build: bool,
    /// The reference input is not valid UTF-8 or contains a NUL byte.
    pub binary: bool,
    #[serde(skip)]
    pub exec: StepExec,
}

/// What is needed to execute a step and to describe its reference output.
#[derive(Clone, Debug, Default)]
pub struct StepExec {
    pub build: Option<String>,
    pub build_file: Option<(String, String)>,
    pub env: Vec<(String, String)>,
    pub command: String,
    pub expected_sha1: String,
    pub reference_output_bytes: u64,
    pub reference_output_sha1: String,
}

/// Orders the languages into a ring starting at `start`, following each
/// language's `output` to the language whose `input` it is, and loads the
/// reference sizes and hashes from `reference_dir`. Fails unless the walk
/// visits every language exactly once and returns to `start`.
pub fn build_ring(
    languages: &HashMap<String, QuineLanguageSpec>,
    reference_dir: &Path,
    start: &str,
) -> anyhow::Result<Vec<Step>> {
    let mut by_input: HashMap<&str, &str> = HashMap::new();
    for (name, spec) in languages {
        if let Some(other) = by_input.insert(spec.input.as_str(), name.as_str()) {
            bail!("languages {other} and {name} share the input {}", spec.input);
        }
    }
    if !languages.contains_key(start) {
        bail!("start language {start} not found");
    }

    let mut order: Vec<&str> = Vec::with_capacity(languages.len());
    let mut seen: HashSet<&str> = HashSet::new();
    let mut name = start;
    loop {
        if !seen.insert(name) {
            bail!("ring revisits {name} before returning to {start}");
        }
        order.push(name);
        let output = &languages[name].output;
        let next = by_input
            .get(output.as_str())
            .with_context(|| format!("no language takes {output}, the output of {name}"))?;
        if *next == start {
            break;
        }
        name = next;
    }
    if order.len() != languages.len() {
        bail!(
            "ring from {start} visits {} of {} languages",
            order.len(),
            languages.len()
        );
    }

    order
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let spec = &languages[name];
            let input_path = reference_dir.join(&spec.input);
            let input = std::fs::read(&input_path)
                .with_context(|| format!("reading {}", input_path.display()))?;
            let output_path = reference_dir.join(&spec.output);
            let output = std::fs::read(&output_path)
                .with_context(|| format!("reading {}", output_path.display()))?;
            let binary = std::str::from_utf8(&input).is_err() || input.contains(&0);
            let mut env: Vec<(String, String)> = spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            env.sort();
            Ok(Step {
                index,
                name: name.to_string(),
                input: spec.input.clone(),
                output: spec.output.clone(),
                size: input.len() as u64,
                has_build: spec.build.is_some(),
                binary,
                exec: StepExec {
                    build: spec.build.clone(),
                    build_file: spec
                        .build_file
                        .as_ref()
                        .map(|f| (f.name.clone(), f.content.clone())),
                    env,
                    command: spec.command.clone(),
                    expected_sha1: spec.output_sha1.clone(),
                    reference_output_bytes: output.len() as u64,
                    reference_output_sha1: sha1_hex(&output),
                },
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Replay,
    Exec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Build,
    Run,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepStart {
    pub lap: u64,
    pub index: usize,
    pub name: String,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PhaseChange {
    pub lap: u64,
    pub index: usize,
    pub phase: Phase,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepDone {
    pub lap: u64,
    pub index: usize,
    pub name: String,
    pub ok: bool,
    pub live: bool,
    pub duration_ms: u64,
    pub output_bytes: u64,
    pub output_sha1: String,
    pub expected_sha1: String,
    pub error: Option<String>,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LapDone {
    pub lap: u64,
    pub duration_ms: u64,
    pub live_steps: usize,
    pub failed_steps: usize,
}

/// An event broadcast to every subscriber.
#[derive(Clone, Debug)]
pub enum RelayEvent {
    StepStart(StepStart),
    Phase(PhaseChange),
    StepDone(StepDone),
    LapDone(LapDone),
}

impl RelayEvent {
    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            RelayEvent::StepStart(_) => "step_start",
            RelayEvent::Phase(_) => "phase",
            RelayEvent::StepDone(_) => "step_done",
            RelayEvent::LapDone(_) => "lap_done",
        }
    }

    /// The JSON payload.
    pub fn data(&self) -> serde_json::Value {
        let value = match self {
            RelayEvent::StepStart(e) => serde_json::to_value(e),
            RelayEvent::Phase(e) => serde_json::to_value(e),
            RelayEvent::StepDone(e) => serde_json::to_value(e),
            RelayEvent::LapDone(e) => serde_json::to_value(e),
        };
        value.expect("event serializes")
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Current {
    pub index: usize,
    pub phase: Phase,
    pub started_at_ms: u64,
}

#[derive(Debug)]
struct RelayState {
    lap: u64,
    current: Option<Current>,
    history: VecDeque<StepDone>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot<'a> {
    pub mode: Mode,
    pub server_time_ms: u64,
    pub lap: u64,
    pub steps: &'a [Step],
    pub current: Option<Current>,
    pub history: Vec<StepDone>,
}

/// Owns the ring, the shared state and the broadcast channel. Every state
/// update and its broadcast happen under one lock, and so do subscribing and
/// taking a snapshot, so a subscriber sees each event either in its snapshot
/// or on its receiver, never both and never neither.
pub struct Relay {
    pub steps: Vec<Step>,
    pub mode: Mode,
    pub reference_dir: PathBuf,
    tx: broadcast::Sender<RelayEvent>,
    state: Mutex<RelayState>,
}

impl Relay {
    pub fn new(steps: Vec<Step>, mode: Mode, reference_dir: PathBuf) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Relay {
            steps,
            mode,
            reference_dir,
            tx,
            state: Mutex::new(RelayState {
                lap: 1,
                current: None,
                history: VecDeque::with_capacity(HISTORY_LEN),
            }),
        }
    }

    /// Subscribes to events and returns the snapshot that precedes the first
    /// event the receiver will yield, serialized as JSON.
    pub fn subscribe(&self) -> (broadcast::Receiver<RelayEvent>, serde_json::Value) {
        let state = self.state.lock().unwrap();
        let rx = self.tx.subscribe();
        (rx, self.snapshot_locked(&state))
    }

    /// The current snapshot, serialized as JSON.
    pub fn snapshot(&self) -> serde_json::Value {
        let state = self.state.lock().unwrap();
        self.snapshot_locked(&state)
    }

    fn snapshot_locked(&self, state: &RelayState) -> serde_json::Value {
        serde_json::to_value(Snapshot {
            mode: self.mode,
            server_time_ms: now_ms(),
            lap: state.lap,
            steps: &self.steps,
            current: state.current.clone(),
            history: state.history.iter().cloned().collect(),
        })
        .expect("snapshot serializes")
    }

    fn emit(&self, event: RelayEvent) {
        let mut state = self.state.lock().unwrap();
        match &event {
            RelayEvent::StepStart(e) => {
                state.lap = e.lap;
                let phase = if self.steps[e.index].has_build {
                    Phase::Build
                } else {
                    Phase::Run
                };
                state.current = Some(Current {
                    index: e.index,
                    phase,
                    started_at_ms: e.at_ms,
                });
            }
            RelayEvent::Phase(e) => {
                if let Some(current) = state.current.as_mut() {
                    current.phase = e.phase;
                }
            }
            RelayEvent::StepDone(e) => {
                state.current = None;
                if state.history.len() == HISTORY_LEN {
                    state.history.pop_front();
                }
                state.history.push_back(e.clone());
            }
            RelayEvent::LapDone(e) => {
                state.lap = e.lap + 1;
            }
        }
        // An error only means nobody is subscribed.
        let _ = self.tx.send(event);
    }
}

/// How the runner drives the ring.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub mode: Mode,
    /// Time per step in replay mode.
    pub step_time: Duration,
    /// Minimum time each step stays on screen in exec mode.
    pub min_step_time: Duration,
    /// Timeout for each of the build and run phases in exec mode.
    pub step_timeout: Duration,
    /// Run build and command inside this docker image instead of locally.
    pub docker_image: Option<String>,
    /// Stop after this many laps; `None` runs forever.
    pub max_laps: Option<u64>,
}

/// Runs the relay until shutdown is signalled (or `max_laps` is reached).
/// Stopping drops any running step, which kills its processes.
pub async fn run(relay: Arc<Relay>, config: RunConfig, mut shutdown: watch::Receiver<bool>) {
    tokio::select! {
        _ = run_laps(&relay, &config) => {}
        _ = shutdown.wait_for(|stop| *stop) => {}
    }
}

async fn run_laps(relay: &Relay, config: &RunConfig) {
    let steps = &relay.steps;
    let mut lap = relay.state.lock().unwrap().lap;
    let mut laps_run = 0;
    let mut program: Option<Vec<u8>> = None;
    loop {
        let lap_started = Instant::now();
        let mut live_steps = 0;
        let mut failed_steps = 0;
        for step in steps {
            let started = Instant::now();
            relay.emit(RelayEvent::StepStart(StepStart {
                lap,
                index: step.index,
                name: step.name.clone(),
                at_ms: now_ms(),
            }));

            let outcome = match config.mode {
                Mode::Replay => {
                    replay_step(relay, lap, step, config.step_time).await;
                    None
                }
                Mode::Exec => {
                    let input = match program.take() {
                        Some(bytes) => bytes,
                        None => read_reference(relay, &step.input).await,
                    };
                    let outcome = if config.docker_image.is_none() && missing_tool(step).is_some()
                    {
                        replay_step(relay, lap, step, config.min_step_time).await;
                        None
                    } else {
                        Some(exec_step(relay, lap, step, &input, config).await)
                    };
                    let elapsed = started.elapsed();
                    if elapsed < config.min_step_time {
                        tokio::time::sleep(config.min_step_time - elapsed).await;
                    }
                    outcome
                }
            };

            let done = match outcome {
                None => {
                    if config.mode == Mode::Exec {
                        program = Some(read_reference(relay, &step.output).await);
                    }
                    StepDone {
                        lap,
                        index: step.index,
                        name: step.name.clone(),
                        ok: true,
                        live: false,
                        duration_ms: started.elapsed().as_millis() as u64,
                        output_bytes: step.exec.reference_output_bytes,
                        output_sha1: step.exec.reference_output_sha1.clone(),
                        expected_sha1: step.exec.expected_sha1.clone(),
                        error: None,
                        at_ms: now_ms(),
                    }
                }
                Some(result) => {
                    live_steps += 1;
                    let output_bytes = result.stdout.len() as u64;
                    let output_sha1 = sha1_hex(&result.stdout);
                    let error = match result.error {
                        Some(e) => Some(e),
                        None if output_sha1 != step.exec.expected_sha1 => {
                            Some(format!("output sha1 mismatch: got {output_sha1}"))
                        }
                        None => None,
                    };
                    if error.is_some() {
                        failed_steps += 1;
                        program = Some(read_reference(relay, &step.output).await);
                    } else {
                        program = Some(result.stdout);
                    }
                    StepDone {
                        lap,
                        index: step.index,
                        name: step.name.clone(),
                        ok: error.is_none(),
                        live: true,
                        duration_ms: started.elapsed().as_millis() as u64,
                        output_bytes,
                        output_sha1,
                        expected_sha1: step.exec.expected_sha1.clone(),
                        error: error.map(|e| one_line(&e)),
                        at_ms: now_ms(),
                    }
                }
            };
            eprintln!(
                "lap {} step {:>3} {:<14} {} {}ms{}",
                done.lap,
                done.index,
                done.name,
                match (done.live, done.ok) {
                    (false, _) => "replay",
                    (true, true) => "ok",
                    (true, false) => "FAIL",
                },
                done.duration_ms,
                done.error
                    .as_deref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            );
            relay.emit(RelayEvent::StepDone(done));
        }

        let lap_done = LapDone {
            lap,
            duration_ms: lap_started.elapsed().as_millis() as u64,
            live_steps,
            failed_steps,
        };
        eprintln!(
            "lap {} done in {}ms: {} live, {} failed",
            lap_done.lap, lap_done.duration_ms, lap_done.live_steps, lap_done.failed_steps
        );
        relay.emit(RelayEvent::LapDone(lap_done));
        lap += 1;
        laps_run += 1;
        if config.max_laps.is_some_and(|max| laps_run >= max) {
            return;
        }
    }
}

async fn read_reference(relay: &Relay, name: &str) -> Vec<u8> {
    let path = relay.reference_dir.join(name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Shows a step without executing it: the build phase (if any) for 40% of
/// `duration`, then the run phase for the rest.
async fn replay_step(relay: &Relay, lap: u64, step: &Step, duration: Duration) {
    let mut run_time = duration;
    if step.has_build {
        emit_phase(relay, lap, step, Phase::Build);
        let build_time = duration.mul_f64(0.4);
        tokio::time::sleep(build_time).await;
        run_time = duration.saturating_sub(build_time);
    }
    emit_phase(relay, lap, step, Phase::Run);
    tokio::time::sleep(run_time).await;
}

fn emit_phase(relay: &Relay, lap: u64, step: &Step, phase: Phase) {
    relay.emit(RelayEvent::Phase(PhaseChange {
        lap,
        index: step.index,
        phase,
        at_ms: now_ms(),
    }));
}

/// The first program named by the step's build segments or command that is
/// not found on PATH. Commands starting with `./` are built by the step and
/// are not checked.
pub fn missing_tool(step: &Step) -> Option<String> {
    let mut commands: Vec<&str> = Vec::new();
    if let Some(build) = &step.exec.build {
        commands.extend(build.split("&&"));
    }
    commands.push(&step.exec.command);
    commands
        .into_iter()
        .filter_map(|c| c.split_whitespace().next())
        .filter(|word| !word.starts_with("./"))
        .find(|word| !on_path(word))
        .map(str::to_string)
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program))))
        .unwrap_or(false)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

struct ExecResult {
    stdout: Vec<u8>,
    error: Option<String>,
}

/// Executes one step in a fresh temporary directory: writes the input and the
/// build file, runs the build (if any) and then the command, and captures the
/// command's stdout.
async fn exec_step(
    relay: &Relay,
    lap: u64,
    step: &Step,
    input: &[u8],
    config: &RunConfig,
) -> ExecResult {
    let fail = |error: String| ExecResult {
        stdout: Vec::new(),
        error: Some(error),
    };
    let dir = match tempfile::Builder::new().prefix("qr-live-").tempdir() {
        Ok(dir) => dir,
        Err(e) => return fail(format!("cannot create temp dir: {e}")),
    };
    if let Err(e) = tokio::fs::write(dir.path().join(&step.input), input).await {
        return fail(format!("cannot write {}: {e}", step.input));
    }
    if let Some((name, content)) = &step.exec.build_file {
        if let Err(e) = tokio::fs::write(dir.path().join(name), content).await {
            return fail(format!("cannot write {name}: {e}"));
        }
    }

    if let Some(build) = &step.exec.build {
        emit_phase(relay, lap, step, Phase::Build);
        let output = run_shell(build, dir.path(), &step.exec.env, config).await;
        if let Some(error) = output.error {
            return fail(format!("build: {error}"));
        }
    }

    emit_phase(relay, lap, step, Phase::Run);
    let output = run_shell(&step.exec.command, dir.path(), &step.exec.env, config).await;
    ExecResult {
        stdout: output.stdout,
        error: output.error,
    }
}

struct ShellOutput {
    stdout: Vec<u8>,
    error: Option<String>,
}

static CONTAINER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Kills a docker container when dropped, unless disarmed.
struct ContainerGuard {
    name: Option<String>,
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let _ = std::process::Command::new("docker")
                .args(["kill", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

/// Kills a process group when dropped, unless disarmed.
struct ProcessGroupGuard {
    pgid: Option<u32>,
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Runs `sh -c script` in `dir` (directly or in docker), with a timeout.
/// On timeout or when the future is dropped, the whole process group (and the
/// container, under docker) is killed.
async fn run_shell(
    script: &str,
    dir: &Path,
    env: &[(String, String)],
    config: &RunConfig,
) -> ShellOutput {
    let mut container = ContainerGuard { name: None };
    let mut command = match &config.docker_image {
        Some(image) => {
            let name = format!(
                "qr-live-{}-{}",
                std::process::id(),
                CONTAINER_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let mut c = tokio::process::Command::new("docker");
            c.args(["run", "--rm", "--network", "none", "--name", &name, "-v"])
                .arg(format!("{}:/work", dir.display()))
                .args(["-w", "/work"]);
            for (k, v) in env {
                c.arg("-e").arg(format!("{k}={v}"));
            }
            c.arg(image).args(["sh", "-c", script]);
            container.name = Some(name);
            c
        }
        None => {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", script])
                .current_dir(dir)
                .envs(env.iter().map(|(k, v)| (k, v)));
            c
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ShellOutput {
                stdout: Vec::new(),
                error: Some(format!("cannot spawn: {e}")),
            }
        }
    };
    let group = ProcessGroupGuard { pgid: child.id() };
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    let status = tokio::time::timeout(config.step_timeout, child.wait()).await;
    match status {
        Err(_) => {
            drop(group);
            drop(container);
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            ShellOutput {
                stdout: Vec::new(),
                error: Some(format!(
                    "timed out after {}s",
                    config.step_timeout.as_secs_f64()
                )),
            }
        }
        Ok(Err(e)) => ShellOutput {
            stdout: Vec::new(),
            error: Some(format!("wait failed: {e}")),
        },
        Ok(Ok(status)) => {
            // The leader has exited; kill anything it left running in its
            // group so the pipes close.
            drop(group);
            container.name = None;
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            let error = (!status.success()).then(|| {
                let stderr = String::from_utf8_lossy(&stderr);
                let last = stderr
                    .lines()
                    .map(str::trim)
                    .rfind(|l| !l.is_empty())
                    .unwrap_or("");
                match status.code() {
                    Some(code) => format!("exit status {code}: {last}"),
                    None => format!("killed by signal: {last}"),
                }
            });
            ShellOutput { stdout, error }
        }
    }
}

/// Collapses whitespace runs to single spaces and truncates to
/// `MAX_ERROR_LEN` characters.
fn one_line(s: &str) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= MAX_ERROR_LEN {
        joined
    } else {
        let mut out: String = joined.chars().take(MAX_ERROR_LEN - 1).collect();
        out.push('…');
        out
    }
}
