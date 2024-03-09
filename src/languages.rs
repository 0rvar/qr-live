use std::collections::HashMap;

#[derive(serde_derive::Deserialize, Debug)]
pub struct QuineLanguageSpec {
    pub input: String,
    pub build: Option<String>,
    pub build_output: Option<String>,
    pub build_file: Option<ExtraBuildFile>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub command: String,
    pub output: String,
    pub output_sha1: String,
}

#[derive(serde_derive::Deserialize, Debug)]
pub struct ExtraBuildFile {
    pub name: String,
    pub content: String,
}

pub fn get_languages() -> HashMap<String, QuineLanguageSpec> {
    let yaml = include_str!("../languages.yaml");
    serde_yaml::from_str(yaml).unwrap()
}
