use std::path::Path;

use qr_live::languages;
use sha1::{Digest, Sha1};

fn main() {
    let language = std::env::args().nth(1).expect("Expected language name");
    let languages = languages::get_languages();
    let spec = languages.get(&language).expect("Language not found");
    let tmp_dir = tempfile::tempdir().expect("Could not create tmp dir");

    let input_content = Path::new("./reference").join(&spec.input);
    let input_content = std::fs::read(input_content).expect("Could not read input file");
    let input_file = tmp_dir.path().join(&spec.input);
    std::fs::write(&input_file, input_content).expect("Could not write input file");

    if let Some(build) = &spec.build {
        // Run build.command
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&build)
            .current_dir(tmp_dir.path())
            .output()
            .expect("Could not run build command");
        if !output.status.success() {
            let mut error = String::from_utf8_lossy(&output.stderr);
            if error.is_empty() {
                error = String::from_utf8_lossy(&output.stdout);
            }
            eprintln!("Build command failed: {}", error);
            std::process::exit(1);
        }
    }

    // Run spec.command, put output in stdout
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&spec.command)
        .current_dir(tmp_dir.path())
        .output()
        .expect("Could not run command");
    if !output.status.success() {
        eprintln!(
            "Command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::process::exit(1);
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let output_sha1 = Sha1::digest(output.as_bytes());
    let output_sha1 = format!("{:x}", output_sha1);
    if output_sha1 != spec.output_sha1 {
        eprintln!(
            "Output SHA1 mismatch: expected {}, got {}",
            spec.output_sha1, output_sha1
        );
        std::process::exit(1);
    }

    println!("OK: {}", language);
}
