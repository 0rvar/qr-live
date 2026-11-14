//! Runs every step of the ring once, each from its reference input, and
//! checks its output against the reference. Prints one line per step with
//! its wall time, then a summary; exits non-zero if any step failed.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use qr_live::languages::{self, Toolchain};
use qr_live::relay::{self, Mode, RunConfig};
use sha1::{Digest, Sha1};

#[derive(Parser, Debug)]
struct Args {
    /// Toolchain the step commands are written for.
    #[arg(long, value_enum, default_value = "nix")]
    toolchain: Toolchain,
    /// Directory holding the reference programs.
    #[arg(long, default_value = "./reference")]
    reference_dir: PathBuf,
    /// Timeout for each build and run phase, in seconds.
    #[arg(long, default_value_t = 900)]
    step_timeout_secs: u64,
    /// Run only these languages (comma separated); default is every step.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let steps = relay::build_ring(
        &languages::get_languages_for(args.toolchain),
        &args.reference_dir,
        "ruby",
    )
    .unwrap_or_else(|e| {
        eprintln!("{e:#}");
        std::process::exit(2);
    });
    let config = RunConfig {
        mode: Mode::Exec,
        step_time: Duration::ZERO,
        min_step_time: Duration::ZERO,
        step_timeout: Duration::from_secs(args.step_timeout_secs),
        docker_image: None,
        docker_args: Vec::new(),
        max_laps: Some(1),
        max_failed_steps: None,
        work_root: None,
    };

    let unknown: Vec<&str> = args
        .only
        .iter()
        .filter(|name| !steps.iter().any(|step| &step.name == *name))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        eprintln!("unknown language(s) in --only: {}", unknown.join(", "));
        std::process::exit(2);
    }

    let started = Instant::now();
    let mut run = 0;
    let mut failed = Vec::new();
    for step in &steps {
        if !args.only.is_empty() && !args.only.contains(&step.name) {
            continue;
        }
        run += 1;
        let step_started = Instant::now();
        let error = match relay::missing_tool(step) {
            Some(tool) => Some(format!("tool not found: {tool}")),
            None => {
                match relay::exec_once(step, &step.exec.input_bytes, &config).await {
                    Err(e) => Some(format!("{e:#}")),
                    Ok(result) => {
                        let sha1 = format!("{:x}", Sha1::digest(&result.stdout));
                        match result.error {
                            Some(e) => Some(e),
                            None if sha1 != step.exec.expected_sha1 => {
                                Some(format!("output sha1 mismatch: got {sha1}"))
                            }
                            None => None,
                        }
                    }
                }
            }
        };
        let ms = step_started.elapsed().as_millis();
        match &error {
            None => println!("{:>3} {:<20} OK   {ms:>8}ms", step.index, step.name),
            Some(e) => {
                println!("{:>3} {:<20} FAIL {ms:>8}ms {e}", step.index, step.name);
                failed.push(step.name.clone());
            }
        }
    }
    println!(
        "{}/{run} OK in {}s",
        run - failed.len(),
        started.elapsed().as_secs()
    );
    if !failed.is_empty() {
        println!("failed: {}", failed.join(" "));
        std::process::exit(1);
    }
}
