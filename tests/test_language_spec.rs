use std::collections::HashMap;

use qr_live::languages;
use sha1::{Digest, Sha1};

#[test]
fn test_all_languages_loop() {
    let languages = languages::get_languages();
    let languages_by_input = languages
        .into_iter()
        .map(|(k, v)| (v.input.clone(), (k.clone(), v)))
        .collect::<HashMap<_, _>>();
    for (input, (name, spec)) in &languages_by_input {
        let output = &spec.output;
        assert!(
            languages_by_input.contains_key(output),
            "Language {name} has input {input} but no other language has input {output}"
        );
    }
}

#[test]
fn test_tools_available() {
    if cfg!(not(target_os = "linux")) {
        return;
    }
    let mut missing_commands = vec![];
    let languages = languages::get_languages();
    for (name, spec) in languages {
        if let Some(build) = spec.build {
            let commands = build
                .split("&&")
                .filter_map(|s| s.trim().split_whitespace().next());
            for command in commands {
                if !command_exists(command) {
                    missing_commands.push((name.clone(), command.to_string()));
                }
            }
        }
        if !spec.command.starts_with("./") {
            let command = spec.command.split_whitespace().next().unwrap();
            if !command_exists(command) {
                missing_commands.push((name.clone(), command.to_string()));
            }
        }
    }
    if !missing_commands.is_empty() {
        let missing_commands = missing_commands
            .into_iter()
            .map(|(name, command)| format!("{:<16} (used by {})", command, name))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "The following commands are not available:\n{}",
            missing_commands
        );
    }
}

#[test]
fn test_reference_files_exist() {
    let languages = languages::get_languages();
    for (name, spec) in languages {
        assert!(
            std::path::Path::new("./")
                .join("reference")
                .join(&spec.input)
                .exists(),
            "Reference file for {} does not exist: {}",
            name,
            spec.input
        );
    }
}

#[test]
fn test_reference_files_have_correct_sha1() {
    let mut incorrect_hashes = vec![];
    let languages = languages::get_languages();
    for (_, spec) in languages {
        let reference_sha1 = spec.output_sha1.clone();
        let reference_file = std::path::Path::new("./")
            .join("reference")
            .join(&spec.output);
        let reference_file_contents = std::fs::read(&reference_file).unwrap();
        let actual_sha1 = Sha1::digest(&reference_file_contents);
        let actual_sha1 = format!("{:x}", actual_sha1);
        if actual_sha1 != reference_sha1 {
            incorrect_hashes.push((reference_file, reference_sha1, actual_sha1));
        }
    }
    if !incorrect_hashes.is_empty() {
        let incorrect_hashes = incorrect_hashes
            .into_iter()
            .map(|(file, expected, actual)| {
                format!(
                    "{:<16} (expected: {}, actual: {})",
                    file.display(),
                    expected,
                    actual
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "The following reference files have incorrect SHA1 hashes:\n{}",
            incorrect_hashes
        );
    }
}

fn command_exists(command: &str) -> bool {
    std::process::Command::new("which")
        .arg(command)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
