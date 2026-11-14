use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use qr_live::languages::{self, parse_languages, QuineLanguageSpec, Toolchain};
use qr_live::relay::{self, Mode, Relay, RelayEvent, RunConfig};
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

/// A one-step ring whose command prints its own input file.
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
    }
}

#[tokio::test]
async fn exec_writes_the_input_under_its_work_name() {
    let dir = tempfile::tempdir().unwrap();
    let steps = one_step_ring(dir.path(), Some("A.TXT"), "cat A.TXT");
    let result = relay::exec_once(
        &steps[0],
        &steps[0].exec.input_bytes,
        &exec_config(Duration::ZERO),
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(&result.stdout[..], b"A");
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

#[tokio::test]
async fn min_step_time_is_a_floor_not_an_addition() {
    let dir = tempfile::tempdir().unwrap();
    let steps = one_step_ring(dir.path(), None, "sleep 0.3; cat a.txt");
    let relay = Arc::new(Relay::new(steps, Mode::Exec));
    let (mut rx, _) = relay.subscribe();
    let (_tx, shutdown) = watch::channel(false);
    relay::run(
        relay.clone(),
        exec_config(Duration::from_millis(600)),
        shutdown,
    )
    .await;
    let mut durations = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let RelayEvent::StepDone(done) = event {
            assert!(done.live && done.ok, "{done:?}");
            durations.push(done.duration_ms);
        }
    }
    assert_eq!(durations.len(), 1);
    assert!(
        (600..850).contains(&durations[0]),
        "a 300ms step with a 600ms floor took {}ms",
        durations[0]
    );
}
