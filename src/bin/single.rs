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

    if let Some(build_file) = &spec.build_file {
        let build_file_name = tmp_dir.path().join(&build_file.name);
        std::fs::write(&build_file_name, &build_file.content).expect("Could not write build file");
    }

    if let Some(build) = &spec.build {
        // Run build.command
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&build)
            .current_dir(tmp_dir.path())
            .output()
            .expect("Could not run build command");
        if !output.status.success() {
            eprintln!(
                "Build command failed \n{}\n{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
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
    let output = output.stdout;
    let output_sha1 = Sha1::digest(&output);
    let output_sha1 = format!("{:x}", output_sha1);
    if output_sha1 != spec.output_sha1 {
        eprintln!(
            "Output SHA1 mismatch: expected {}, got {}",
            spec.output_sha1, output_sha1
        );

        let expectected_output =
            std::fs::read(Path::new("./reference").join(&spec.output)).unwrap();
        if let Ok(expectected_output) = String::from_utf8(expectected_output.clone()) {
            let string_output = String::from_utf8_lossy(&output).to_string();
            let diff = prettydiff::diff_lines(&expectected_output, &string_output);
            eprintln!("Diff:\n{}", diff);
        } else {
            eprintln!("Diff: <binary>");
        }

        std::process::exit(1);
    }

    println!("OK: {}", language);
}
