//! Runs one step from its reference input, printing the full stderr of a
//! failing phase and a diff of a wrong output.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use qr_live::languages::{self, QuineLanguageSpec, Toolchain};
use sha1::{Digest, Sha1};

#[derive(Parser, Debug)]
struct Args {
    /// Language to run, as named in languages.yaml.
    language: String,
    /// Toolchain the step commands are written for.
    #[arg(long, value_enum, default_value = "nix")]
    toolchain: Toolchain,
    /// Directory holding the reference programs.
    #[arg(long, default_value = "./reference")]
    reference_dir: PathBuf,
}

fn run(script: &str, dir: &Path, spec: &QuineLanguageSpec) -> std::process::Output {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .envs(&spec.env)
        .output()
        .expect("Could not run sh")
}

fn main() {
    let args = Args::parse();
    let language = &args.language;
    let languages = languages::get_languages_for(args.toolchain);
    let spec = languages.get(language).expect("Language not found");
    let tmp_dir = tempfile::tempdir().expect("Could not create tmp dir");

    let input_content = args.reference_dir.join(&spec.input);
    let input_content = std::fs::read(input_content).expect("Could not read input file");
    let input_file = tmp_dir.path().join(spec.work_input());
    std::fs::write(&input_file, input_content).expect("Could not write input file");

    if let Some(build_file) = &spec.build_file {
        println!("Writing {}", build_file.name);
        let build_file_name = tmp_dir.path().join(&build_file.name);
        std::fs::write(&build_file_name, &build_file.content).expect("Could not write build file");
    }

    if let Some(build) = &spec.build {
        println!(
            "Building {} from {} ({})",
            spec.build_output.as_deref().unwrap_or_default(),
            spec.work_input(),
            build
        );
        let started = Instant::now();
        let output = run(build, tmp_dir.path(), spec);
        println!("Build took {}ms", started.elapsed().as_millis());
        if !output.status.success() {
            eprintln!(
                "Build command failed ({})\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            std::process::exit(1);
        }
    }

    println!("Running {}", spec.command);
    let started = Instant::now();
    let output = run(&spec.command, tmp_dir.path(), spec);
    println!("Run took {}ms", started.elapsed().as_millis());
    if !output.status.success() {
        eprintln!(
            "Command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        std::process::exit(1);
    }
    let output = output.stdout;
    let output_sha1 = format!("{:x}", Sha1::digest(&output));
    if output_sha1 != spec.output_sha1 {
        eprintln!(
            "Output SHA1 mismatch: expected {}, got {}",
            spec.output_sha1, output_sha1
        );

        let expected_output = std::fs::read(args.reference_dir.join(&spec.output)).unwrap();
        if let Ok(expected_output) = String::from_utf8(expected_output) {
            let string_output = String::from_utf8_lossy(&output).to_string();
            let diff = prettydiff::diff_lines(&expected_output, &string_output);
            eprintln!("Diff:\n{}", diff);
        } else {
            eprintln!("Diff: <binary>");
        }

        std::process::exit(1);
    }

    println!("OK: {}", language);
}
