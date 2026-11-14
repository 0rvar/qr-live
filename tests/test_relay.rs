use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures_util::StreamExt;
use qr_live::languages::{self, ExtraBuildFile, QuineLanguageSpec};
use qr_live::relay::{self, Mode, Relay, RelayEvent, RunConfig};
use qr_live::server::{self, Assets};
use sha1::{Digest, Sha1};
use tokio::sync::{broadcast, watch};
use tower::ServiceExt;

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

struct FakeStep {
    name: &'static str,
    input: &'static str,
    reference: &'static [u8],
    build: Option<&'static str>,
    build_file: Option<(&'static str, &'static str)>,
    command: String,
}

fn fake(name: &'static str, input: &'static str, reference: &'static str) -> FakeStep {
    FakeStep {
        name,
        input,
        reference: reference.as_bytes(),
        build: None,
        build_file: None,
        command: format!("cat {input}"),
    }
}

/// Writes each step's reference input into `dir` and returns specs where
/// step i's output is step i+1's input (wrapping) and its `output_sha1` is
/// the SHA-1 of that reference file.
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
                output_sha1: sha1_hex(next.reference),
            },
        );
    }
    specs
}

/// The three-step ring a.txt -> b.txt -> c.txt -> a.txt used by the exec
/// tests. Given its reference input, b produces c's reference and c produces
/// a's; a runs `a_command`.
fn abc(a_command: &str) -> Vec<FakeStep> {
    let mut a = fake("a", "a.txt", "REF-A");
    a.command = a_command.to_string();
    let mut b = fake("b", "b.txt", "REF-B");
    b.command = "cat b.txt | tr B C".into();
    let mut c = fake("c", "c.txt", "REF-C");
    c.command = "cat c.txt | tr C A".into();
    vec![a, b, c]
}

fn config(mode: Mode, laps: u64) -> RunConfig {
    RunConfig {
        mode,
        step_time: Duration::ZERO,
        min_step_time: Duration::ZERO,
        step_timeout: Duration::from_secs(30),
        docker_image: None,
        docker_args: Vec::new(),
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
    let relay = Arc::new(Relay::new(steps, config.mode));
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

/// A compact trace like `start:1:0 phase:1:0:run done:1:0 lap:1`.
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
        assert_eq!(step.exec.input_bytes, input);
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

#[test]
fn ring_with_wrong_reference_hash_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(dir.path(), &abc("cat a.txt"));
    std::fs::write(dir.path().join("c.txt"), "TAMPERED").unwrap();
    let error = relay::build_ring(&specs, dir.path(), "a").unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("c.txt"), "{message}");
    assert!(message.contains("output_sha1"), "{message}");
}

#[test]
fn ring_with_missing_reference_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(dir.path(), &abc("cat a.txt"));
    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    let error = relay::build_ring(&specs, dir.path(), "a").unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("b.txt"), "{message}");
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
        assert!(done.skipped.is_none());
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
    assert_eq!(snapshot["totals"]["steps_replayed"], 6);
    assert_eq!(snapshot["totals"]["steps_live"], 0);
    assert_eq!(snapshot["totals"]["laps_done"], 2);
    assert_eq!(snapshot["totals"]["last_lap"]["lap"], 2);
}

#[tokio::test]
async fn exec_runs_commands_and_feeds_verified_output_forward() {
    let dir = tempfile::tempdir().unwrap();
    // a prints b's reference, b derives c's reference from its input, and c
    // (with a build step and a build file) produces a's reference.
    let mut steps = abc("printf REF-B");
    steps[0].reference = b"C-A\n";
    steps[2].build = Some("cp c.txt c2.txt && test -f c2.txt");
    steps[2].build_file = Some(("extra.txt", "-A"));
    steps[2].command = "cat c2.txt extra.txt | cut -c5-".into();
    let specs = fake_ring(dir.path(), &steps);

    let (relay, events) = run_to_end(dir.path(), &specs, "a", config(Mode::Exec, 2)).await;
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
        assert!(done.skipped.is_none());
        assert_eq!(done.output_sha1, done.expected_sha1);
    }
    assert_eq!(dones[2].output_bytes, 4);
    let totals = &relay.snapshot()["totals"];
    assert_eq!(totals["steps_live"], 6);
    assert_eq!(totals["steps_failed"], 0);
    assert_eq!(totals["steps_replayed"], 0);
    assert_eq!(totals["last_lap"]["live_steps"], 3);
}

#[tokio::test]
async fn hash_mismatch_fails_and_next_step_gets_reference() {
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(dir.path(), &abc("printf REAL"));

    let (relay, events) = run_to_end(dir.path(), &specs, "a", config(Mode::Exec, 1)).await;
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
    let totals = &relay.snapshot()["totals"];
    assert_eq!(totals["steps_failed"], 1);
    assert_eq!(totals["steps_live"], 2);
}

#[tokio::test]
async fn step_with_missing_tool_is_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(dir.path(), &abc("qr-live-no-such-tool a.txt"));

    let mut cfg = config(Mode::Exec, 1);
    cfg.min_step_time = Duration::from_millis(50);
    let (relay, events) = run_to_end(dir.path(), &specs, "a", cfg).await;
    let dones = step_dones(&events);
    assert!(!dones[0].live && dones[0].ok);
    assert_eq!(
        dones[0].skipped.as_deref(),
        Some("tool not found: qr-live-no-such-tool")
    );
    assert_eq!(dones[0].output_sha1, sha1_hex(b"REF-B"));
    assert!(dones[0].duration_ms >= 50);
    assert!(dones[1].live && dones[1].ok, "{:?}", dones[1]);
    let totals = &relay.snapshot()["totals"];
    assert_eq!(totals["steps_replayed"], 1);
    assert_eq!(totals["steps_live"], 2);
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
    let specs = fake_ring(
        dir.path(),
        &abc(&format!(
            "sleep 30 & echo $! > {}; wait",
            pid_file.display()
        )),
    );

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

#[tokio::test]
async fn endless_output_fails_on_the_cap_before_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(dir.path(), &abc("yes 0123456789abcdef0123456789abcdef"));

    let mut cfg = config(Mode::Exec, 1);
    cfg.step_timeout = Duration::from_secs(60);
    let started = Instant::now();
    let (_relay, events) = run_to_end(dir.path(), &specs, "a", cfg).await;
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    let dones = step_dones(&events);
    assert!(dones[0].live && !dones[0].ok);
    assert_eq!(dones[0].error.as_deref(), Some("output exceeded 64 MiB"));
    assert!(dones[1].live && dones[1].ok, "{:?}", dones[1]);
    let peak = peak_rss_bytes();
    assert!(peak < 1 << 30, "peak RSS {} MiB", peak >> 20);
}

/// Peak resident set size of this test process.
fn peak_rss_bytes() -> u64 {
    // SAFETY: getrusage only writes into the struct it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let max = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024
    }
}

#[tokio::test]
async fn escaped_process_holding_stdout_fails_after_grace() {
    // perl's POSIX::setsid moves the background process out of the step's
    // process group, so killing the group cannot close its copy of stdout.
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(
        dir.path(),
        &abc("perl -MPOSIX -e 'if (fork() == 0) { setsid(); sleep 8; exit }'; printf REF-B"),
    );

    let mut cfg = config(Mode::Exec, 1);
    cfg.step_timeout = Duration::from_secs(10);
    let started = Instant::now();
    let (_relay, events) = run_to_end(dir.path(), &specs, "a", cfg).await;
    let dones = step_dones(&events);
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "took {:?}",
        started.elapsed()
    );
    assert!(dones[0].live && !dones[0].ok, "{:?}", dones[0]);
    assert_eq!(
        dones[0].error.as_deref(),
        Some("output pipe still open after exit")
    );
    assert!(dones[1].live && dones[1].ok, "{:?}", dones[1]);
}

#[test]
fn snapshot_totals_count_exactly_the_preceding_events() {
    let dir = tempfile::tempdir().unwrap();
    let specs = fake_ring(dir.path(), &abc("printf REAL"));
    let steps = relay::build_ring(&specs, dir.path(), "a").unwrap();
    let relay = Arc::new(Relay::new(steps, Mode::Exec));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut cfg = config(Mode::Exec, 3);
    cfg.min_step_time = Duration::from_millis(5);
    let runner = runtime.spawn(relay::run(relay.clone(), cfg, shutdown_rx));

    // Subscribe repeatedly while the runner works.
    let mut subscriptions = Vec::new();
    while !runner.is_finished() {
        subscriptions.push(relay.subscribe());
        std::thread::sleep(Duration::from_micros(500));
    }
    assert!(subscriptions.len() > 10);

    // Each snapshot's totals plus everything received after it must add up
    // to the final totals.
    let end = relay.snapshot()["totals"].clone();
    assert_eq!(end["steps_failed"], 3);
    assert_eq!(end["steps_live"], 6);
    assert_eq!(end["steps_replayed"], 0);
    assert_eq!(end["laps_done"], 3);
    for (mut rx, snapshot) in subscriptions {
        let count = |key: &str| snapshot["totals"][key].as_u64().unwrap();
        let (mut failed, mut live, mut replayed, mut laps) = (
            count("steps_failed"),
            count("steps_live"),
            count("steps_replayed"),
            count("laps_done"),
        );
        let history = snapshot["history"].as_array().unwrap();
        assert_eq!(failed + live + replayed, history.len() as u64);
        if laps > 0 {
            assert_eq!(snapshot["totals"]["last_lap"]["lap"], laps);
        }
        while let Ok(event) = rx.try_recv() {
            match event {
                RelayEvent::StepDone(d) => {
                    failed += u64::from(!d.ok);
                    live += u64::from(d.live && d.ok);
                    replayed += u64::from(!d.live);
                }
                RelayEvent::LapDone(_) => laps += 1,
                _ => {}
            }
        }
        assert_eq!(failed, end["steps_failed"].as_u64().unwrap());
        assert_eq!(live, end["steps_live"].as_u64().unwrap());
        assert_eq!(replayed, end["steps_replayed"].as_u64().unwrap());
        assert_eq!(laps, end["laps_done"].as_u64().unwrap());
    }
}

fn test_router(relay: Arc<Relay>) -> (axum::Router, watch::Sender<bool>) {
    let (tx, rx) = watch::channel(false);
    let assets = Assets {
        index_html: "<html></html>",
        app_js: "",
        style_css: "",
    };
    (server::router(relay, assets, rx), tx)
}

fn source_relay(dir: &Path) -> Arc<Relay> {
    let mut a = fake("a", "a.txt", "");
    a.reference = "aé€😀z".as_bytes();
    let mut b = fake("b", "b.png", "");
    b.reference = b"\x89PNG\r\n\x1a\n\0\0";
    let specs = fake_ring(dir, &[a, b]);
    let steps = relay::build_ring(&specs, dir, "a").unwrap();
    Arc::new(Relay::new(steps, Mode::Replay))
}

async fn get(
    router: &axum::Router,
    uri: &str,
    if_none_match: Option<&str>,
) -> axum::http::Response<Body> {
    let mut request = Request::builder().uri(uri);
    if let Some(etag) = if_none_match {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn source_endpoint_serves_truncated_cached_reference() {
    let dir = tempfile::tempdir().unwrap();
    let relay = source_relay(dir.path());
    let full = "aé€😀z".as_bytes();
    let (router, _shutdown) = test_router(relay.clone());

    let response = get(&router, "/api/source/0", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(response.headers()["x-total-size"], full.len().to_string());
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=3600"
    );
    let full_etag = format!("\"{}-full\"", sha1_hex(full));
    assert_eq!(response.headers()[header::ETAG], full_etag.as_str());
    assert_eq!(body_bytes(response).await, full);

    // "a" (1) + "é" (2) + "€" (3) = 6 bytes; a limit of 8 cuts the emoji.
    let response = get(&router, "/api/source/0?limit=8", None).await;
    assert_eq!(response.headers()["x-total-size"], full.len().to_string());
    let etag = response.headers()[header::ETAG].to_str().unwrap().to_string();
    assert_eq!(etag, format!("\"{}-8\"", sha1_hex(full)));
    assert_eq!(body_bytes(response).await, "aé€".as_bytes());

    let response = get(&router, "/api/source/0?limit=3", None).await;
    assert_eq!(body_bytes(response).await, "aé".as_bytes());

    let response = get(&router, "/api/source/0?limit=8", Some(&etag)).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()[header::ETAG], etag.as_str());
    assert!(body_bytes(response).await.is_empty());

    let response = get(&router, "/api/source/0?limit=8", Some(&full_etag)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = get(&router, "/api/source/1?limit=4", None).await;
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(response.headers()["x-total-size"], "10");
    assert_eq!(body_bytes(response).await, b"\x89PNG");

    let response = get(&router, "/api/source/2", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = get(&router, "/", None).await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
}

#[tokio::test]
async fn events_endpoint_starts_with_snapshot_carrying_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let relay = source_relay(dir.path());
    let (router, shutdown) = test_router(relay.clone());

    let response = get(&router, "/api/events", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    while !text.contains("\n\n") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("snapshot arrives")
            .expect("stream open")
            .unwrap();
        text.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let message = text.split("\n\n").next().unwrap();
    let lines: Vec<&str> = message.lines().collect();
    assert!(lines.contains(&"event: snapshot"), "{message}");
    assert!(lines.contains(&"retry: 1000"), "{message}");
    let data = lines
        .iter()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("data line");
    let snapshot: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(snapshot["run_id"], relay.run_id.as_str());
    assert!(!relay.run_id.is_empty());
    assert_eq!(snapshot["totals"]["laps_done"], 0);
    assert_eq!(snapshot["totals"]["last_lap"], serde_json::Value::Null);

    shutdown.send(true).unwrap();
    let rest = tokio::time::timeout(Duration::from_secs(5), async {
        while body.next().await.is_some() {}
    })
    .await;
    assert!(rest.is_ok(), "stream ends on shutdown");
}

#[test]
fn run_ids_differ_between_instances() {
    let dir = tempfile::tempdir().unwrap();
    let a = source_relay(dir.path());
    let b = source_relay(dir.path());
    assert_ne!(a.run_id, b.run_id);
}
