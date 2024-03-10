use std::path::Path;

use anyhow::Context;
use qr_live::languages::{self, QuineLanguageSpec};
use sha1::{Digest, Sha1};

fn main() {
    let languages = languages::get_languages();
    // Get a list of all languages sorted by name
    let mut languages: Vec<_> = languages.iter().collect();
    languages.sort_by_key(|(name, _)| (*name).clone());

    let mut failed = 0;
    for (name, spec) in &languages {
        match evolve(spec) {
            Ok(_) => println!("OK: {}", name),
            Err(e) => {
                failed += 1;
                eprintln!("Error: {}: {e:?}", name)
            }
        }
    }
    println!("{}/{} OK", languages.len() - failed, languages.len());
}

fn evolve(spec: &QuineLanguageSpec) -> Result<(), anyhow::Error> {
    let tmp_dir = tempfile::tempdir().expect("Could not create tmp dir");

    let input_content = Path::new("./reference").join(&spec.input);
    let input_content = std::fs::read(input_content).context("Could not read input file")?;
    let input_file = tmp_dir.path().join(&spec.input);
    std::fs::write(&input_file, input_content).context("Could not write input file")?;

    if let Some(build_file) = &spec.build_file {
        let build_file_name = tmp_dir.path().join(&build_file.name);
        std::fs::write(&build_file_name, &build_file.content)
            .context("Could not write build file")?;
    }

    if let Some(build) = &spec.build {
        // Run build.command
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&build)
            .current_dir(tmp_dir.path())
            .output()
            .context("Could not run build command")?;
        if !output.status.success() {
            anyhow::bail!(
                "Build command failed: {}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
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
        anyhow::bail!(
            "Command {} failed: {}{}",
            &spec.command,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let output = output.stdout;
    let output_sha1 = Sha1::digest(&output);
    let output_sha1 = format!("{:x}", output_sha1);
    if output_sha1 != spec.output_sha1 {
        anyhow::bail!(
            "Output SHA1 mismatch: expected {}, got {}",
            spec.output_sha1,
            output_sha1
        );
    }
    Ok(())
}
