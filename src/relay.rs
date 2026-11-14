//! The quine relay as a ring of steps, the events it produces while running,
//! the shared state that new subscribers are bootstrapped from, and the runner
//! that drives the ring forever in replay or exec mode.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use bytes::Bytes;
use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, watch};

use crate::languages::QuineLanguageSpec;

/// Number of finished steps kept in the snapshot history.
pub const HISTORY_LEN: usize = 256;
/// Capacity of the broadcast channel feeding SSE subscribers.
pub const CHANNEL_CAPACITY: usize = 1024;
/// Maximum length of the `error` string in a `step_done` event.
const MAX_ERROR_LEN: usize = 300;
/// Maximum stdout captured from a command; more fails the step.
pub const STDOUT_CAP: usize = 64 * 1024 * 1024;
/// How much of the end of a command's stderr is kept.
const STDERR_TAIL: usize = 64 * 1024;
/// How long the output pipes may stay open after the shell has exited.
const PIPE_GRACE: Duration = Duration::from_secs(2);
/// Bound on each `docker kill` / `docker rm` cleanup command.
const DOCKER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

/// A random 64-bit value from the standard library's randomly keyed hasher.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(now_ms());
    hasher.write_u32(std::process::id());
    hasher.finish()
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

/// What is needed to execute a step, and its reference programs in memory.
#[derive(Clone, Debug, Default)]
pub struct StepExec {
    /// The file name the input is written under in the work directory.
    pub input_name: String,
    pub build: Option<String>,
    pub build_file: Option<(String, String)>,
    pub env: Vec<(String, String)>,
    pub command: String,
    pub expected_sha1: String,
    /// The reference input program.
    pub input_bytes: Bytes,
    pub input_sha1: String,
    /// The reference output program (the next step's input); its SHA-1 is
    /// `expected_sha1`.
    pub reference_output: Bytes,
}

/// Orders the languages into a ring starting at `start`, following each
/// language's `output` to the language whose `input` it is, and loads every
/// reference program from `reference_dir` into memory. Fails unless the walk
/// visits every language exactly once and returns to `start`, every reference
/// file is readable, and each reference output hashes to its `output_sha1`.
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

    let inputs = order
        .iter()
        .map(|name| {
            let path = reference_dir.join(&languages[*name].input);
            std::fs::read(&path)
                .map(Bytes::from)
                .with_context(|| format!("cannot read reference file {}", path.display()))
        })
        .collect::<anyhow::Result<Vec<Bytes>>>()?;

    let count = order.len();
    order
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let spec = &languages[name];
            let input = inputs[index].clone();
            let output = inputs[(index + 1) % count].clone();
            let output_sha1 = sha1_hex(&output);
            if output_sha1 != spec.output_sha1 {
                bail!(
                    "reference file {} has SHA-1 {output_sha1}, but {name}'s output_sha1 is {}",
                    reference_dir.join(&spec.output).display(),
                    spec.output_sha1
                );
            }
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
                    input_name: spec.work_input().to_string(),
                    build: spec.build.clone(),
                    build_file: spec
                        .build_file
                        .as_ref()
                        .map(|f| (f.name.clone(), f.content.clone())),
                    env,
                    command: spec.command.clone(),
                    expected_sha1: spec.output_sha1.clone(),
                    input_sha1: sha1_hex(&input),
                    input_bytes: input,
                    reference_output: output,
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
    /// Why exec mode replayed the step instead of running it.
    pub skipped: Option<String>,
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

/// Counters since the process started. A step counts as failed when
/// `ok == false`, live when `live && ok`, replayed when `!live`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Totals {
    pub steps_live: u64,
    pub steps_replayed: u64,
    pub steps_failed: u64,
    pub laps_done: u64,
    pub last_lap: Option<LapDone>,
}

#[derive(Debug)]
struct RelayState {
    lap: u64,
    current: Option<Current>,
    history: VecDeque<StepDone>,
    totals: Totals,
}

#[derive(Debug, Serialize)]
pub struct Snapshot<'a> {
    pub run_id: &'a str,
    pub mode: Mode,
    pub server_time_ms: u64,
    pub lap: u64,
    pub steps: &'a [Step],
    pub current: Option<Current>,
    pub history: Vec<StepDone>,
    pub totals: Totals,
}

/// Owns the ring, the shared state and the broadcast channel. Every state
/// update and its broadcast happen under one lock, and so do subscribing and
/// taking a snapshot, so a subscriber sees each event either in its snapshot
/// or on its receiver, never both and never neither.
pub struct Relay {
    pub steps: Vec<Step>,
    pub mode: Mode,
    /// Unique per relay instance, so clients can tell a restarted server.
    pub run_id: String,
    tx: broadcast::Sender<RelayEvent>,
    state: Mutex<RelayState>,
}

impl Relay {
    pub fn new(steps: Vec<Step>, mode: Mode) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Relay {
            steps,
            mode,
            run_id: format!("{:x}-{:016x}", now_ms(), random_u64()),
            tx,
            state: Mutex::new(RelayState {
                lap: 1,
                current: None,
                history: VecDeque::with_capacity(HISTORY_LEN),
                totals: Totals::default(),
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
            run_id: &self.run_id,
            mode: self.mode,
            server_time_ms: now_ms(),
            lap: state.lap,
            steps: &self.steps,
            current: state.current.clone(),
            history: state.history.iter().cloned().collect(),
            totals: state.totals.clone(),
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
                let totals = &mut state.totals;
                if !e.ok {
                    totals.steps_failed += 1;
                }
                if e.live && e.ok {
                    totals.steps_live += 1;
                }
                if !e.live {
                    totals.steps_replayed += 1;
                }
            }
            RelayEvent::LapDone(e) => {
                state.lap = e.lap + 1;
                state.totals.laps_done += 1;
                state.totals.last_lap = Some(e.clone());
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
    /// Deadline for each of the build and run phases in exec mode.
    pub step_timeout: Duration,
    /// Run build and command inside this docker image instead of locally.
    pub docker_image: Option<String>,
    /// Extra `docker run` arguments, inserted before the image name.
    pub docker_args: Vec<String>,
    /// Stop after this many laps; `None` runs forever.
    pub max_laps: Option<u64>,
}

/// Runs the relay until shutdown is signalled (or `max_laps` is reached).
/// Stopping drops any running step, which kills its process group; any
/// container still registered is then removed before this returns.
pub async fn run(relay: Arc<Relay>, config: RunConfig, mut shutdown: watch::Receiver<bool>) {
    tokio::select! {
        _ = run_laps(&relay, &config) => {}
        _ = shutdown.wait_for(|stop| *stop) => {}
    }
    let leftover: Vec<String> = LIVE_CONTAINERS.lock().unwrap().drain().collect();
    for name in leftover {
        force_remove_container(&name).await;
    }
}

enum Outcome {
    Replayed { skipped: Option<String> },
    Executed(ExecResult),
}

async fn run_laps(relay: &Relay, config: &RunConfig) {
    let steps = &relay.steps;
    let mut lap = relay.state.lock().unwrap().lap;
    let mut laps_run = 0;
    let mut program: Option<Bytes> = None;
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
                    Outcome::Replayed { skipped: None }
                }
                Mode::Exec => {
                    let input = program
                        .take()
                        .unwrap_or_else(|| step.exec.input_bytes.clone());
                    let missing = if config.docker_image.is_none() {
                        missing_tool(step)
                    } else {
                        None
                    };
                    let outcome = match missing {
                        Some(tool) => {
                            replay_step(relay, lap, step, config.min_step_time).await;
                            Outcome::Replayed {
                                skipped: Some(format!("tool not found: {tool}")),
                            }
                        }
                        None => Outcome::Executed(
                            exec_step(Some((relay, lap)), step, &input, config).await,
                        ),
                    };
                    let elapsed = started.elapsed();
                    if elapsed < config.min_step_time {
                        tokio::time::sleep(config.min_step_time - elapsed).await;
                    }
                    outcome
                }
            };

            let done = match outcome {
                Outcome::Replayed { skipped } => {
                    if config.mode == Mode::Exec {
                        program = Some(step.exec.reference_output.clone());
                    }
                    StepDone {
                        lap,
                        index: step.index,
                        name: step.name.clone(),
                        ok: true,
                        live: false,
                        duration_ms: started.elapsed().as_millis() as u64,
                        output_bytes: step.exec.reference_output.len() as u64,
                        output_sha1: step.exec.expected_sha1.clone(),
                        expected_sha1: step.exec.expected_sha1.clone(),
                        error: None,
                        skipped,
                        at_ms: now_ms(),
                    }
                }
                Outcome::Executed(result) => {
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
                        program = Some(step.exec.reference_output.clone());
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
                        skipped: None,
                        at_ms: now_ms(),
                    }
                }
            };
            let detail = done
                .error
                .as_deref()
                .or(done.skipped.as_deref())
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
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
                detail
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

/// Shell builtins that may lead a command segment.
const SHELL_BUILTINS: &[&str] = &[
    "cd", "exec", "exit", "export", "set", "true", "false", "ulimit", "umask", ":",
];

/// The first program named by a segment of the step's build or command that
/// is not found on PATH. Segments are split on unquoted `&&`, `||`, `;` and
/// `|`. Leading `NAME=value` assignments and shell builtins are skipped;
/// programs starting with `./` are built by the step and are not checked.
pub fn missing_tool(step: &Step) -> Option<String> {
    let mut scripts: Vec<&str> = Vec::new();
    if let Some(build) = &step.exec.build {
        scripts.push(build);
    }
    scripts.push(&step.exec.command);
    scripts
        .into_iter()
        .flat_map(command_segments)
        .filter_map(|segment| {
            segment
                .split_whitespace()
                .find(|word| !is_assignment(word))
                .map(|word| word.trim_start_matches(['(', '{']).to_string())
        })
        .filter(|word| {
            !word.is_empty() && !word.starts_with("./") && !SHELL_BUILTINS.contains(&word.as_str())
        })
        .find(|word| !on_path(word))
}

/// Splits a shell script on `&&`, `||`, `;` and `|` outside quotes.
fn command_segments(script: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                ';' | '|' => {
                    if c == '|' && chars.peek() == Some(&'|') {
                        chars.next();
                    }
                    segments.push(std::mem::take(&mut current));
                }
                '&' if chars.peek() == Some(&'&') => {
                    chars.next();
                    segments.push(std::mem::take(&mut current));
                }
                _ => current.push(c),
            },
        }
    }
    segments.push(current);
    segments
}

/// A shell variable assignment such as `LANG=C`.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
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

/// What executing a step produced. `error` is set when the build or command
/// failed; the output hash is not checked here.
pub struct ExecResult {
    pub stdout: Bytes,
    pub error: Option<String>,
}

/// Executes one step outside a relay, exactly as exec mode would, without
/// emitting events or checking the output hash.
pub async fn exec_once(step: &Step, input: &[u8], config: &RunConfig) -> ExecResult {
    exec_step(None, step, input, config).await
}

/// Executes one step in a fresh temporary directory: writes the input and the
/// build file, runs the build (if any) and then the command, and captures the
/// command's stdout. The directory is removed afterwards.
async fn exec_step(
    events: Option<(&Relay, u64)>,
    step: &Step,
    input: &[u8],
    config: &RunConfig,
) -> ExecResult {
    let dir = match tempfile::Builder::new().prefix("qr-live-").tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return ExecResult {
                stdout: Bytes::new(),
                error: Some(format!("cannot create temp dir: {e}")),
            }
        }
    };
    let result = exec_in_dir(events, step, input, config, dir.path()).await;
    let path = dir.path().to_path_buf();
    if let Err(e) = dir.close() {
        eprintln!("warning: cannot remove temp dir {}: {e}", path.display());
    }
    result
}

async fn exec_in_dir(
    events: Option<(&Relay, u64)>,
    step: &Step,
    input: &[u8],
    config: &RunConfig,
    dir: &Path,
) -> ExecResult {
    let fail = |error: String| ExecResult {
        stdout: Bytes::new(),
        error: Some(error),
    };
    if let Err(e) = tokio::fs::write(dir.join(&step.exec.input_name), input).await {
        return fail(format!("cannot write {}: {e}", step.exec.input_name));
    }
    if let Some((name, content)) = &step.exec.build_file {
        if let Err(e) = tokio::fs::write(dir.join(name), content).await {
            return fail(format!("cannot write {name}: {e}"));
        }
    }

    let phase = |phase: Phase| {
        if let Some((relay, lap)) = events {
            emit_phase(relay, lap, step, phase);
        }
    };
    if let Some(build) = &step.exec.build {
        phase(Phase::Build);
        let output = run_shell(build, dir, &step.exec.env, config).await;
        if let Some(error) = output.error {
            return fail(format!("build: {error}"));
        }
    }

    phase(Phase::Run);
    run_shell(&step.exec.command, dir, &step.exec.env, config).await
}

static CONTAINER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Containers that have been started and not yet confirmed gone.
static LIVE_CONTAINERS: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Runs a docker cleanup command quietly, bounded by `DOCKER_CLEANUP_TIMEOUT`,
/// and reaps it.
async fn docker_quiet(args: &[&str]) {
    let mut command = tokio::process::Command::new("docker");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match command.spawn() {
        Ok(mut child) => {
            if tokio::time::timeout(DOCKER_CLEANUP_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                eprintln!("warning: docker {} timed out", args.join(" "));
                let _ = child.kill().await;
            }
        }
        Err(e) => eprintln!("warning: cannot run docker {}: {e}", args.join(" ")),
    }
}

/// Kills and removes a container, then forgets it.
async fn force_remove_container(name: &str) {
    docker_quiet(&["kill", name]).await;
    docker_quiet(&["rm", "-f", name]).await;
    LIVE_CONTAINERS.lock().unwrap().remove(name);
}

/// SIGKILLs a process group when dropped.
struct ProcessGroupGuard {
    pgid: Option<u32>,
}

impl ProcessGroupGuard {
    fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: killpg has no memory-safety preconditions.
            unsafe {
                libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reads to EOF, failing once more than `cap` bytes have arrived.
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, cap: usize) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return Ok(buf);
        }
        if buf.len() + n > cap {
            return Err(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Reads to EOF, keeping only roughly the last `STDERR_TAIL` bytes in `tail`.
async fn drain_tail<R: AsyncRead + Unpin>(mut reader: R, tail: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = reader.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        let mut tail = tail.lock().unwrap();
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > 2 * STDERR_TAIL {
            let excess = tail.len() - STDERR_TAIL;
            tail.drain(..excess);
        }
    }
}

enum Ending {
    Exited(ExitStatus),
    WaitFailed(std::io::Error),
    TimedOut,
    Overflow,
}

/// Runs `sh -c script` in `dir` (directly or in docker). One deadline of
/// `step_timeout` covers the process and both output pipes. Stdout is capped
/// at `STDOUT_CAP`. After the shell exits, the pipes get `PIPE_GRACE` to
/// close. On every failure the process group is killed and, under docker,
/// the container is removed; dropping the future kills the group too.
async fn run_shell(
    script: &str,
    dir: &Path,
    env: &[(String, String)],
    config: &RunConfig,
) -> ExecResult {
    let fail = |error: String| ExecResult {
        stdout: Bytes::new(),
        error: Some(error),
    };
    let deadline = tokio::time::Instant::now() + config.step_timeout;
    let mut container: Option<String> = None;
    let mut command = match &config.docker_image {
        Some(image) => {
            let name = format!(
                "qr-live-{}-{}-{:08x}",
                std::process::id(),
                CONTAINER_SEQ.fetch_add(1, Ordering::Relaxed),
                random_u64() as u32
            );
            let mut c = tokio::process::Command::new("docker");
            c.args(["run", "--rm", "--network", "none", "--name", &name, "-v"])
                .arg(format!("{}:/work", dir.display()))
                .args(["-w", "/work"]);
            for (k, v) in env {
                c.arg("-e").arg(format!("{k}={v}"));
            }
            c.args(&config.docker_args)
                .arg(image)
                .args(["sh", "-c", script]);
            container = Some(name);
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

    if let Some(name) = &container {
        LIVE_CONTAINERS.lock().unwrap().insert(name.clone());
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            if let Some(name) = &container {
                LIVE_CONTAINERS.lock().unwrap().remove(name);
            }
            return fail(format!("cannot spawn: {e}"));
        }
    };
    let mut group = ProcessGroupGuard { pgid: child.id() };
    let stdout_pipe = child.stdout.take().expect("stdout is piped");
    let stderr_pipe = child.stderr.take().expect("stderr is piped");
    let mut stdout_task = tokio::spawn(read_capped(stdout_pipe, STDOUT_CAP));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let mut stderr_task = tokio::spawn(drain_tail(stderr_pipe, stderr.clone()));

    let mut stdout: Option<Vec<u8>> = None;
    let ending = loop {
        tokio::select! {
            status = child.wait() => break match status {
                Ok(status) => Ending::Exited(status),
                Err(e) => Ending::WaitFailed(e),
            },
            read = &mut stdout_task, if stdout.is_none() => match read {
                Ok(Ok(bytes)) => stdout = Some(bytes),
                _ => break Ending::Overflow,
            },
            _ = tokio::time::sleep_until(deadline) => break Ending::TimedOut,
        }
    };

    // The leader has exited or is about to be killed; take anything still in
    // its group with it so the pipes close.
    group.kill();

    // `docker run` exits with the container's own status unless it was killed
    // or failed itself (125); only a clean exit means the container is gone.
    let mut docker_clean = false;
    let result = match ending {
        Ending::Exited(status) => {
            let now = tokio::time::Instant::now();
            let grace_end = deadline.min(now + PIPE_GRACE);
            let grace_error = if deadline <= now + PIPE_GRACE {
                format!("timed out after {}s", config.step_timeout.as_secs_f64())
            } else {
                "output pipe still open after exit".to_string()
            };
            let pipes_closed = tokio::time::timeout_at(grace_end, async {
                let stdout = match stdout.take() {
                    Some(bytes) => Ok(bytes),
                    None => (&mut stdout_task).await.unwrap_or(Err(())),
                };
                let _ = (&mut stderr_task).await;
                stdout
            })
            .await;
            match pipes_closed {
                Err(_) => fail(grace_error),
                Ok(Err(())) => fail(format!("output exceeded {} MiB", STDOUT_CAP >> 20)),
                Ok(Ok(bytes)) => {
                    docker_clean = matches!(status.code(), Some(code) if code != 125);
                    let error = (!status.success()).then(|| {
                        let stderr = stderr.lock().unwrap();
                        let stderr = String::from_utf8_lossy(&stderr);
                        let last = stderr
                            .lines()
                            .map(str::trim)
                            .rfind(|l| !l.is_empty())
                            .unwrap_or("")
                            .to_string();
                        match status.code() {
                            Some(code) => format!("exit status {code}: {last}"),
                            None => format!("killed by signal: {last}"),
                        }
                    });
                    ExecResult {
                        stdout: Bytes::from(bytes),
                        error,
                    }
                }
            }
        }
        Ending::WaitFailed(e) => fail(format!("wait failed: {e}")),
        Ending::TimedOut => fail(format!(
            "timed out after {}s",
            config.step_timeout.as_secs_f64()
        )),
        Ending::Overflow => fail(format!("output exceeded {} MiB", STDOUT_CAP >> 20)),
    };
    stdout_task.abort();
    stderr_task.abort();
    let _ = child.kill().await;

    if let Some(name) = container {
        if docker_clean {
            LIVE_CONTAINERS.lock().unwrap().remove(&name);
        } else {
            force_remove_container(&name).await;
        }
    }
    result
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
