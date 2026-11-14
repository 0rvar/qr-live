use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use qr_live::languages::{self, parse_languages, QuineLanguageSpec, Toolchain};
use qr_live::relay::{self, now_ms, Mode, Relay, RelayEvent, RunConfig};
use sha1::{Digest, Sha1};
use tokio::sync::watch;

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

const YAML: &str = r#"
plain:
  input: a.txt
  build: make-a
  build_output: a.out
  env:
    KEEP: base
    SHARED: base
  command: run-a
  output: b.txt
  output_sha1: 0000000000000000000000000000000000000000
  upstream:
    command: run-a-upstream

full:
  input: b.txt
  build: make-b
  build_output: b.out
  build_file:
    name: b.proj
    content: base project
  env:
    KEEP: base
    SHARED: base
  command: run-b
  output: a.txt
  output_sha1: 1111111111111111111111111111111111111111
  upstream:
    work_input: B.TXT
    build: make-b-upstream
    build_output: B.OUT
    build_file:
      name: b-upstream.proj
      content: upstream project
    env:
      SHARED: upstream
      ADDED: upstream
    command: run-b-upstream

no_build:
  input: c.txt
  build: make-c
  build_output: c.out
  command: run-c
  output: d.txt
  output_sha1: 2222222222222222222222222222222222222222
  upstream:
    build: ""
    command: run-c-upstream

untouched:
  input: d.txt
  command: run-d
  output: c.txt
  output_sha1: 3333333333333333333333333333333333333333
"#;

fn both() -> (
    HashMap<String, QuineLanguageSpec>,
    HashMap<String, QuineLanguageSpec>,
) {
    (
        parse_languages(YAML, Toolchain::Nix).unwrap(),
        parse_languages(YAML, Toolchain::Upstream).unwrap(),
    )
}

#[test]
fn nix_toolchain_ignores_upstream_blocks() {
    let (nix, _) = both();
    let full = &nix["full"];
    assert_eq!(full.work_input, None);
    assert_eq!(full.work_input(), "b.txt");
    assert_eq!(full.build.as_deref(), Some("make-b"));
    assert_eq!(full.build_file.as_ref().unwrap().name, "b.proj");
    assert_eq!(full.env["SHARED"], "base");
    assert!(!full.env.contains_key("ADDED"));
    assert_eq!(full.command, "run-b");
    assert_eq!(nix["no_build"].build.as_deref(), Some("make-c"));
}

#[test]
fn upstream_block_changes_only_the_fields_it_names() {
    let (nix, upstream) = both();

    let mut expected = nix["plain"].clone();
    expected.command = "run-a-upstream".into();
    assert_eq!(upstream["plain"], expected);

    assert_eq!(upstream["untouched"], nix["untouched"]);

    let full = &upstream["full"];
    assert_eq!(full.input, "b.txt");
    assert_eq!(full.work_input(), "B.TXT");
    assert_eq!(full.build.as_deref(), Some("make-b-upstream"));
    assert_eq!(full.build_output.as_deref(), Some("B.OUT"));
    let build_file = full.build_file.as_ref().unwrap();
    assert_eq!(build_file.name, "b-upstream.proj");
    assert_eq!(build_file.content, "upstream project");
    assert_eq!(full.env["KEEP"], "base");
    assert_eq!(full.env["SHARED"], "upstream");
    assert_eq!(full.env["ADDED"], "upstream");
    assert_eq!(full.env.len(), 3);
    assert_eq!(full.command, "run-b-upstream");
    assert_eq!(full.output, nix["full"].output);
    assert_eq!(full.output_sha1, nix["full"].output_sha1);
}

#[test]
fn empty_upstream_build_removes_the_build_step() {
    let (_, upstream) = both();
    let spec = &upstream["no_build"];
    assert_eq!(spec.build, None);
    assert_eq!(spec.build_output, None);
    assert_eq!(spec.command, "run-c-upstream");
}

#[test]
fn unknown_key_in_a_language_entry_is_rejected() {
    let yaml = r#"
x:
  input: a
  command: c
  output: a
  output_sha1: 0
  upstrem:
    command: typo
"#;
    for toolchain in [Toolchain::Nix, Toolchain::Upstream] {
        let error = format!("{:#}", parse_languages(yaml, toolchain).unwrap_err());
        assert!(error.contains("upstrem"), "{error}");
    }
}

#[test]
fn unknown_field_in_upstream_block_is_rejected() {
    let yaml = r#"
x:
  input: a
  command: c
  output: a
  output_sha1: 0
  upstream:
    commnd: typo
"#;
    assert!(parse_languages(yaml, Toolchain::Upstream).is_err());
}

#[test]
fn both_toolchains_build_the_full_ring() {
    for toolchain in [Toolchain::Nix, Toolchain::Upstream] {
        let specs = languages::get_languages_for(toolchain);
        let steps = relay::build_ring(&specs, Path::new("reference"), "ruby").unwrap();
        assert_eq!(steps.len(), 128, "{toolchain:?}");
        assert_eq!(steps[0].name, "ruby");
        assert_eq!(steps[127].name, "rexx");
        for step in &steps {
            assert!(!step.exec.command.trim().is_empty(), "{}", step.name);
            if let Some(build) = &step.exec.build {
                assert!(!build.trim().is_empty(), "{}", step.name);
            }
        }
    }
}

#[test]
fn upstream_toolchain_keeps_reference_names_and_renames_only_the_work_file() {
    let nix = relay::build_ring(
        &languages::get_languages_for(Toolchain::Nix),
        Path::new("reference"),
        "ruby",
    )
    .unwrap();
    let upstream = relay::build_ring(
        &languages::get_languages_for(Toolchain::Upstream),
        Path::new("reference"),
        "ruby",
    )
    .unwrap();
    for (n, u) in nix.iter().zip(&upstream) {
        assert_eq!(n.name, u.name);
        assert_eq!(n.input, u.input);
        assert_eq!(n.output, u.output);
        assert_eq!(n.exec.input_bytes, u.exec.input_bytes);
        assert_eq!(n.exec.expected_sha1, u.exec.expected_sha1);
        assert_eq!(n.exec.input_name, n.input);
    }
    let lisaac = upstream.iter().find(|s| s.name == "lisaac").unwrap();
    assert_eq!(lisaac.input, "QR.li");
    assert_eq!(lisaac.exec.input_name, "qr.li");
}

/// A ring of one step named "a" whose reference input is `a.txt` with the
/// content "A", and which must output "A" again.
fn one_step_ring(dir: &Path, work_input: Option<&str>, command: &str) -> Vec<relay::Step> {
    std::fs::write(dir.join("a.txt"), "A").unwrap();
    let spec = QuineLanguageSpec {
        input: "a.txt".into(),
        work_input: work_input.map(str::to_string),
        build: None,
        build_output: None,
        build_file: None,
        env: HashMap::new(),
        command: command.into(),
        output: "a.txt".into(),
        output_sha1: sha1_hex(b"A"),
    };
    let specs = HashMap::from([("a".to_string(), spec)]);
    relay::build_ring(&specs, dir, "a").unwrap()
}

fn exec_config(min_step_time: Duration) -> RunConfig {
    RunConfig {
        mode: Mode::Exec,
        step_time: Duration::ZERO,
        min_step_time,
        step_timeout: Duration::from_secs(30),
        docker_image: None,
        docker_args: Vec::new(),
        max_laps: Some(1),
        max_failed_steps: None,
        work_root: None,
    }
}

/// Runs the relay to completion and returns its result and every event.
async fn run_collect(
    relay: &Arc<Relay>,
    config: RunConfig,
) -> (anyhow::Result<()>, Vec<RelayEvent>) {
    let (mut rx, _) = relay.subscribe();
    let (_tx, shutdown) = watch::channel(false);
    let result = relay::run(relay.clone(), config, shutdown).await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
}

#[tokio::test]
async fn exec_writes_the_input_under_its_work_name_only() {
    let dir = tempfile::tempdir().unwrap();
    let steps = one_step_ring(
        dir.path(),
        Some("renamed-input.txt"),
        "test ! -e a.txt && cat renamed-input.txt",
    );
    let result = relay::exec_once(
        &steps[0],
        &steps[0].exec.input_bytes,
        &exec_config(Duration::ZERO),
    )
    .await
    .unwrap();
    assert_eq!(result.error, None);
    assert_eq!(&result.stdout[..], b"A");
}

#[tokio::test]
async fn work_name_need_not_exist_in_the_reference_dir() {
    let dir = tempfile::tempdir().unwrap();
    let steps = one_step_ring(dir.path(), Some("work-only.txt"), "cat work-only.txt");
    assert!(!dir.path().join("work-only.txt").exists());
    assert_eq!(steps[0].input, "a.txt");
    assert_eq!(steps[0].exec.input_name, "work-only.txt");
    assert_eq!(&steps[0].exec.input_bytes[..], b"A");

    let relay = Arc::new(Relay::new(steps, Mode::Exec));
    let (result, events) = run_collect(&relay, exec_config(Duration::ZERO)).await;
    result.unwrap();
    let done = events
        .iter()
        .find_map(|e| match e {
            RelayEvent::StepDone(d) => Some(d),
            _ => None,
        })
        .unwrap();
    assert!(done.live && done.ok, "{done:?}");
}

#[test]
fn missing_tool_skips_assignments_and_builtins_and_checks_every_segment() {
    let dir = tempfile::tempdir().unwrap();
    let steps = one_step_ring(
        dir.path(),
        None,
        "LANG=C X_1=y ulimit -s unlimited && cat a.txt || true; cat a.txt | cat",
    );
    assert_eq!(relay::missing_tool(&steps[0]), None);

    let steps = one_step_ring(dir.path(), None, "cat a.txt | qr-live-no-such-tool");
    assert_eq!(
        relay::missing_tool(&steps[0]).as_deref(),
        Some("qr-live-no-such-tool")
    );

    let steps = one_step_ring(dir.path(), None, "LANG=C qr-live-no-such-tool a.txt");
    assert_eq!(
        relay::missing_tool(&steps[0]).as_deref(),
        Some("qr-live-no-such-tool")
    );
}

/// A two-step ring a.txt -> b.txt -> a.txt where each step prints the
/// other's reference with `command` prefixed.
fn two_step_ring(dir: &Path, prefix_a: &str) -> Vec<relay::Step> {
    std::fs::write(dir.join("a.txt"), "A").unwrap();
    std::fs::write(dir.join("b.txt"), "B").unwrap();
    let spec = |input: &str, output: &str, command: String, sha: &[u8]| QuineLanguageSpec {
        input: input.into(),
        work_input: None,
        build: None,
        build_output: None,
        build_file: None,
        env: HashMap::new(),
        command,
        output: output.into(),
        output_sha1: sha1_hex(sha),
    };
    let specs = HashMap::from([
        (
            "a".to_string(),
            spec("a.txt", "b.txt", format!("{prefix_a}printf B"), b"B"),
        ),
        (
            "b".to_string(),
            spec("b.txt", "a.txt", "printf A".to_string(), b"A"),
        ),
    ]);
    relay::build_ring(&specs, dir, "a").unwrap()
}

#[tokio::test]
async fn fast_step_reports_its_real_duration_and_the_next_start_waits() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Arc::new(Relay::new(two_step_ring(dir.path(), ""), Mode::Exec));
    let mut config = exec_config(Duration::from_millis(500));
    config.max_laps = Some(2);

    let runner = tokio::spawn({
        let relay = relay.clone();
        let config = config.clone();
        async move { run_collect(&relay, config).await }
    });
    // Between the first step_done and the second step_start nothing runs.
    let mut saw_idle = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let snapshot = relay.snapshot();
        if !snapshot["history"].as_array().unwrap().is_empty() && snapshot["current"].is_null() {
            saw_idle = true;
            break;
        }
    }
    let (result, events) = runner.await.unwrap();
    result.unwrap();
    assert!(saw_idle, "current was never null between steps");

    let starts: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            RelayEvent::StepStart(s) => Some(s.at_ms),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 4);
    for pair in starts.windows(2) {
        assert!(
            pair[1] - pair[0] >= 500,
            "steps started {}ms apart",
            pair[1] - pair[0]
        );
    }
    for event in &events {
        if let RelayEvent::StepDone(done) = event {
            assert!(done.live && done.ok, "{done:?}");
            assert!(
                done.duration_ms < 300,
                "a printf step reported {}ms",
                done.duration_ms
            );
        }
    }
    // lap_done follows the lap's last step_done with no hold in between.
    let kinds: Vec<&str> = events.iter().map(RelayEvent::name).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k != "phase")
            .copied()
            .collect::<Vec<_>>(),
        [
            "step_start",
            "step_done",
            "step_start",
            "step_done",
            "lap_done",
            "step_start",
            "step_done",
            "step_start",
            "step_done",
            "lap_done",
        ]
    );
    for pair in events.windows(2) {
        if let (RelayEvent::StepDone(done), RelayEvent::LapDone(lap)) = (&pair[0], &pair[1]) {
            assert!(
                lap.duration_ms >= 500 && lap.duration_ms < 500 + 300,
                "{lap:?}"
            );
            assert!(done.at_ms <= now_ms());
        }
    }
}

#[tokio::test]
async fn slow_step_is_not_held_beyond_the_floor() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Arc::new(Relay::new(
        two_step_ring(dir.path(), "sleep 0.4; "),
        Mode::Exec,
    ));
    let (result, events) = run_collect(&relay, exec_config(Duration::from_millis(200))).await;
    result.unwrap();
    let starts: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            RelayEvent::StepStart(s) => Some(s.at_ms),
            _ => None,
        })
        .collect();
    let gap = starts[1] - starts[0];
    assert!(
        (400..1000).contains(&gap),
        "a 400ms step with a 200ms floor took {gap}ms"
    );
}

#[tokio::test]
async fn too_many_failed_steps_in_a_lap_stop_the_runner() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Arc::new(Relay::new(
        two_step_ring(dir.path(), "false && "),
        Mode::Exec,
    ));
    let mut config = exec_config(Duration::ZERO);
    config.max_laps = None;
    config.max_failed_steps = Some(0);
    let (result, events) = run_collect(&relay, config).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("1 failed steps"), "{error}");
    let laps = events.iter().filter(|e| e.name() == "lap_done").count();
    assert_eq!(laps, 1);
}

#[tokio::test]
async fn failures_within_the_limit_keep_the_runner_going() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Arc::new(Relay::new(
        two_step_ring(dir.path(), "false && "),
        Mode::Exec,
    ));
    let mut config = exec_config(Duration::ZERO);
    config.max_laps = Some(3);
    config.max_failed_steps = Some(1);
    let (result, events) = run_collect(&relay, config).await;
    result.unwrap();
    assert_eq!(events.iter().filter(|e| e.name() == "lap_done").count(), 3);
}

#[tokio::test]
async fn work_dir_that_cannot_be_created_stops_the_runner() {
    let dir = tempfile::tempdir().unwrap();
    let relay = Arc::new(Relay::new(two_step_ring(dir.path(), ""), Mode::Exec));
    let mut config = exec_config(Duration::ZERO);
    config.max_laps = None;
    config.work_root = Some(dir.path().join("does-not-exist"));
    let (result, events) = run_collect(&relay, config).await;
    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("work directory"), "{error}");
    assert!(events.iter().all(|e| e.name() != "step_done"));
}
