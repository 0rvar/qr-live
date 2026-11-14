use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use qr_live::languages::{self, ExtraBuildFile, QuineLanguageSpec};
use qr_live::relay::{self, Mode, Relay, RelayEvent, RunConfig};
use sha1::{Digest, Sha1};
use tokio::sync::{broadcast, watch};

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

struct FakeStep {
    name: &'static str,
    input: &'static str,
    reference: &'static str,
    build: Option<&'static str>,
    build_file: Option<(&'static str, &'static str)>,
    command: String,
    expected_output: &'static str,
}

fn fake(name: &'static str, input: &'static str, reference: &'static str) -> FakeStep {
    FakeStep {
        name,
        input,
        reference,
        build: None,
        build_file: None,
        command: format!("cat {input}"),
        expected_output: "",
    }
}

/// Writes each step's reference input into `dir` and returns specs where
/// step i's output is step i+1's input, wrapping around.
fn fake_ring(dir: &Path, steps: &[FakeStep]) -> HashMap<String, QuineLanguageSpec> {
    let mut specs = HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        std::fs::write(dir.join(step.input), step.reference).unwrap();
        let next = &steps[(i + 1) % steps.len()];
        specs.insert(
            step.name.to_string(),
            QuineLanguageSpec {
                input: step.input.to_string(),
                build: step.build.map(str::to_string),
                build_output: None,
                build_file: step.build_file.map(|(name, content)| ExtraBuildFile {
                    name: name.to_string(),
                    content: content.to_string(),
                }),
                env: HashMap::new(),
                command: step.command.clone(),
                output: next.input.to_string(),
                output_sha1: sha1_hex(step.expected_output.as_bytes()),
            },
        );
    }
    specs
}

fn config(mode: Mode, laps: u64) -> RunConfig {
    RunConfig {
        mode,
        step_time: Duration::ZERO,
        min_step_time: Duration::ZERO,
        step_timeout: Duration::from_secs(30),
        docker_image: None,
        max_laps: Some(laps),
    }
}

/// Runs the relay to completion and returns every event it broadcast.
async fn run_to_end(
    dir: &Path,
    specs: &HashMap<String, QuineLanguageSpec>,
    start: &str,
    config: RunConfig,
) -> (Arc<Relay>, Vec<RelayEvent>) {
    let steps = relay::build_ring(specs, dir, start).unwrap();
    let relay = Arc::new(Relay::new(steps, config.mode, dir.to_path_buf()));
    let (mut rx, _) = relay.subscribe();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    relay::run(relay.clone(), config, shutdown_rx).await;
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => events.push(event),
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(e) => panic!("receiver error: {e:?}"),
        }
    }
    (relay, events)
}

fn step_dones(events: &[RelayEvent]) -> Vec<&relay::StepDone> {
    events
        .iter()
        .filter_map(|e| match e {
            RelayEvent::StepDone(d) => Some(d),
            _ => None,
        })
        .collect()
}

/// A compact trace like `start:0 phase:0:build phase:0:run done:0 lap:1`.
fn trace(events: &[RelayEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            RelayEvent::StepStart(s) => format!("start:{}:{}", s.lap, s.index),
            RelayEvent::Phase(p) => {
                let phase = serde_json::to_value(p.phase).unwrap();
                format!("phase:{}:{}:{}", p.lap, p.index, phase.as_str().unwrap())
            }
            RelayEvent::StepDone(d) => format!("done:{}:{}", d.lap, d.index),
            RelayEvent::LapDone(l) => format!("lap:{}", l.lap),
        })
        .collect()
}

#[test]
fn real_ring_has_all_languages_starting_at_ruby() {
    let steps =
        relay::build_ring(&languages::get_languages(), Path::new("reference"), "ruby").unwrap();
    assert_eq!(steps.len(), 128);
    assert_eq!(steps[0].name, "ruby");
    assert_eq!(steps[0].input, "QR.rb");
    for (i, step) in steps.iter().enumerate() {
        assert_eq!(step.index, i);
        let next = &steps[(i + 1) % steps.len()];
        assert_eq!(step.output, next.input, "{} feeds {}", step.name, next.name);
        let input = std::fs::read(Path::new("reference").join(&step.input)).unwrap();
        assert_eq!(step.size, input.len() as u64);
    }
    let png = steps
        .iter()
        .find(|s| s.input.ends_with(".png"))
        .expect("a step takes a png");
    assert!(png.binary);
    assert!(!steps[0].binary);
}

#[test]
fn ring_that_does_not_cover_every_language_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut specs = fake_ring(
        dir.path(),
        &[fake("a", "a.txt", "A"), fake("b", "b.txt", "B")],
    );
    specs.extend(fake_ring(
        dir.path(),
        &[fake("c", "c.txt", "C"), fake("d", "d.txt", "D")],
    ));
    assert!(relay::build_ring(&specs, dir.path(), "a").is_err());
}

#[tokio::test]
async fn replay_emits_events_in_order_with_reference_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = fake("b", "b.txt", "bee");
    b.build = Some("true");
    let specs = fake_ring(
        dir.path(),
        &[fake("a", "a.txt", "ay"), b, fake("c", "c.txt", "sea")],
    );
    let (relay, events) = run_to_end(dir.path(), &specs, "a", config(Mode::Replay, 2)).await;

    let mut expected = Vec::new();
    for lap in 1..=2 {
        for index in 0..3 {
            expected.push(format!("start:{lap}:{index}"));
            if index == 1 {
                expected.push(format!("phase:{lap}:{index}:build"));
            }
            expected.push(format!("phase:{lap}:{index}:run"));
            expected.push(format!("done:{lap}:{index}"));
        }
        expected.push(format!("lap:{lap}"));
    }
    assert_eq!(trace(&events), expected);

    let outputs = ["bee", "sea", "ay"];
    for done in step_dones(&events) {
        assert!(done.ok && !done.live);
        assert!(done.error.is_none());
        let output = outputs[done.index];
        assert_eq!(done.output_sha1, sha1_hex(output.as_bytes()));
        assert_eq!(done.output_bytes, output.len() as u64);
    }

    let snapshot = relay.snapshot();
    assert_eq!(snapshot["mode"], "replay");
    assert_eq!(snapshot["lap"], 3);
    assert_eq!(snapshot["current"], serde_json::Value::Null);
    assert_eq!(snapshot["steps"].as_array().unwrap().len(), 3);
    assert_eq!(snapshot["steps"][1]["has_build"], true);
    assert_eq!(snapshot["history"].as_array().unwrap().len(), 6);
    assert_eq!(snapshot["history"][0]["lap"], 1);
    assert_eq!(snapshot["history"][5]["lap"], 2);
}

#[tokio::test]
async fn exec_runs_commands_and_feeds_real_output_forward() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = fake("a", "a.txt", "start");
    a.command = "printf REAL".into();
    a.expected_output = "REAL";
    let mut b = fake("b", "b.txt", "REF-B");
    b.expected_output = "REAL";
    let mut c = fake("c", "c.txt", "REF-C");
    c.build = Some("cp c.txt c2.txt && test -f c2.txt");
    c.build_file = Some(("extra.txt", "!"));
    c.command = "cat c2.txt extra.txt".into();
    c.expected_output = "REAL!";
    let specs = fake_ring(dir.path(), &[a, b, c]);

    let (_relay, events) = run_to_end(dir.path(), &specs, "a", config(Mode::Exec, 2)).await;
    assert_eq!(
        &trace(&events)[..10],
        [
            "start:1:0",
            "phase:1:0:run",
            "done:1:0",
            "start:1:1",
            "phase:1:1:run",
            "done:1:1",
            "start:1:2",
            "phase:1:2:build",
            "phase:1:2:run",
            "done:1:2",
        ]
    );
    let dones = step_dones(&events);
    assert_eq!(dones.len(), 6);
    for done in &dones {
        assert!(done.live && done.ok, "{done:?}");
        assert_eq!(done.output_sha1, done.expected_sha1);
    }
    assert_eq!(dones[2].output_bytes, 5);
    let lap_dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RelayEvent::LapDone(l) => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(lap_dones.len(), 2);
    assert_eq!(lap_dones[0].live_steps, 3);
    assert_eq!(lap_dones[0].failed_steps, 0);
}

#[tokio::test]
async fn hash_mismatch_fails_and_next_step_gets_reference() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = fake("a", "a.txt", "start");
    a.command = "printf REAL".into();
    a.expected_output = "WRONG";
    let mut b = fake("b", "b.txt", "REF-B");
    b.expected_output = "REF-B";
    let mut c = fake("c", "c.txt", "REF-C");
    c.expected_output = "REF-B";
    let specs = fake_ring(dir.path(), &[a, b, c]);

    let (_relay, events) = run_to_end(dir.path(), &specs, "a", config(Mode::Exec, 1)).await;
    let dones = step_dones(&events);
    assert!(dones[0].live && !dones[0].ok);
    assert_eq!(dones[0].output_sha1, sha1_hex(b"REAL"));
    assert!(dones[0].error.as_deref().unwrap().contains("sha1"));
    assert!(dones[1].live && dones[1].ok, "{:?}", dones[1]);
    assert!(dones[1].error.is_none());
    match events.last().unwrap() {
        RelayEvent::LapDone(l) => {
            assert_eq!(l.live_steps, 3);
            assert_eq!(l.failed_steps, 1);
        }
        other => panic!("expected lap_done, got {other:?}"),
    }
}

#[tokio::test]
async fn step_with_missing_tool_is_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = fake("a", "a.txt", "start");
    a.command = "qr-live-no-such-tool a.txt".into();
    a.expected_output = "REF-B";
    let mut b = fake("b", "b.txt", "REF-B");
    b.expected_output = "REF-B";
    let mut c = fake("c", "c.txt", "REF-C");
    c.expected_output = "REF-B";
    let specs = fake_ring(dir.path(), &[a, b, c]);

    let mut cfg = config(Mode::Exec, 1);
    cfg.min_step_time = Duration::from_millis(50);
    let (_relay, events) = run_to_end(dir.path(), &specs, "a", cfg).await;
    let dones = step_dones(&events);
    assert!(!dones[0].live && dones[0].ok);
    assert_eq!(dones[0].output_sha1, sha1_hex(b"REF-B"));
    assert!(dones[0].duration_ms >= 50);
    assert!(dones[1].live && dones[1].ok, "{:?}", dones[1]);
}

fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn step_past_timeout_fails_and_its_processes_are_killed() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sleep.pid");
    let mut a = fake("a", "a.txt", "start");
    a.command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
    a.expected_output = "REF-B";
    let mut b = fake("b", "b.txt", "REF-B");
    b.expected_output = "REF-B";
    let mut c = fake("c", "c.txt", "REF-C");
    c.expected_output = "REF-B";
    let specs = fake_ring(dir.path(), &[a, b, c]);

    let mut cfg = config(Mode::Exec, 1);
    cfg.step_timeout = Duration::from_secs(1);
    let started = Instant::now();
    let (_relay, events) = run_to_end(dir.path(), &specs, "a", cfg).await;
    assert!(started.elapsed() < Duration::from_secs(10));

    let dones = step_dones(&events);
    assert!(dones[0].live && !dones[0].ok);
    assert!(dones[0].error.as_deref().unwrap().contains("timed out"));
    assert!(dones[1].live && dones[1].ok, "{:?}", dones[1]);

    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!process_alive(pid), "sleep {pid} is still running");
}
