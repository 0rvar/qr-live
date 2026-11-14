use std::collections::HashMap;

use anyhow::Context;

/// How one language turns its input program into the next language's.
#[derive(serde_derive::Deserialize, Clone, Debug, PartialEq)]
pub struct QuineLanguageSpec {
    /// The reference file name of the input program; links the ring and names
    /// the file in `reference/`.
    pub input: String,
    /// The name the input is written under in the step's work directory, when
    /// the toolchain needs a different one than `input`.
    #[serde(default)]
    pub work_input: Option<String>,
    pub build: Option<String>,
    pub build_output: Option<String>,
    pub build_file: Option<ExtraBuildFile>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub command: String,
    pub output: String,
    pub output_sha1: String,
}

impl QuineLanguageSpec {
    /// The file name the input program is written to before building.
    pub fn work_input(&self) -> &str {
        self.work_input.as_deref().unwrap_or(&self.input)
    }
}

#[derive(serde_derive::Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExtraBuildFile {
    pub name: String,
    pub content: String,
}

/// The toolchain a step's commands are written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Toolchain {
    /// The base values in `languages.yaml`, for the tools of `flake.nix`.
    Nix,
    /// The base values with each language's `upstream:` block applied, for
    /// the upstream quine-relay Docker image (`docker/Dockerfile`).
    Upstream,
}

/// Fields that replace the base values of a language under one toolchain.
/// Absent fields keep the base value. An empty `build` removes the build
/// step; `env` entries are added to (and win over) the base `env`.
#[derive(serde_derive::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpecOverride {
    pub work_input: Option<String>,
    pub build: Option<String>,
    pub build_output: Option<String>,
    pub build_file: Option<ExtraBuildFile>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub command: Option<String>,
}

impl SpecOverride {
    pub fn apply(&self, spec: &mut QuineLanguageSpec) {
        if let Some(work_input) = &self.work_input {
            spec.work_input = Some(work_input.clone());
        }
        match self.build.as_deref() {
            Some("") => {
                spec.build = None;
                spec.build_output = None;
            }
            Some(build) => spec.build = Some(build.to_string()),
            None => {}
        }
        if let Some(build_output) = &self.build_output {
            spec.build_output = Some(build_output.clone());
        }
        if let Some(build_file) = &self.build_file {
            spec.build_file = Some(build_file.clone());
        }
        for (key, value) in &self.env {
            spec.env.insert(key.clone(), value.clone());
        }
        if let Some(command) = &self.command {
            spec.command = command.clone();
        }
    }
}

/// One entry of the languages file as written: the base spec plus its
/// overrides. Unknown keys are rejected.
#[derive(serde_derive::Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageEntry {
    input: String,
    #[serde(default)]
    work_input: Option<String>,
    build: Option<String>,
    build_output: Option<String>,
    build_file: Option<ExtraBuildFile>,
    #[serde(default)]
    env: HashMap<String, String>,
    command: String,
    output: String,
    output_sha1: String,
    upstream: Option<SpecOverride>,
}

/// Parses a languages file and returns its specs for `toolchain`.
pub fn parse_languages(
    yaml: &str,
    toolchain: Toolchain,
) -> anyhow::Result<HashMap<String, QuineLanguageSpec>> {
    let entries: HashMap<String, LanguageEntry> =
        serde_yaml::from_str(yaml).context("cannot parse languages")?;
    Ok(entries
        .into_iter()
        .map(|(name, entry)| {
            let mut spec = QuineLanguageSpec {
                input: entry.input,
                work_input: entry.work_input,
                build: entry.build,
                build_output: entry.build_output,
                build_file: entry.build_file,
                env: entry.env,
                command: entry.command,
                output: entry.output,
                output_sha1: entry.output_sha1,
            };
            if toolchain == Toolchain::Upstream {
                if let Some(upstream) = &entry.upstream {
                    upstream.apply(&mut spec);
                }
            }
            (name, spec)
        })
        .collect())
}

/// The embedded `languages.yaml` for `toolchain`.
pub fn get_languages_for(toolchain: Toolchain) -> HashMap<String, QuineLanguageSpec> {
    parse_languages(include_str!("../languages.yaml"), toolchain).unwrap()
}

/// The embedded `languages.yaml` for the nix toolchain.
pub fn get_languages() -> HashMap<String, QuineLanguageSpec> {
    get_languages_for(Toolchain::Nix)
}
