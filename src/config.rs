use std::sync::{LazyLock, OnceLock};

use clap::ValueEnum;
use serde::Deserialize;
use toml::from_str;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    Claude,
    Codex,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Claude => write!(f, "Claude"),
            Backend::Codex => write!(f, "Codex"),
        }
    }
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

pub fn set_backend(backend: Backend) {
    BACKEND.set(backend).ok();
}

pub fn backend() -> Backend {
    BACKEND.get().copied().unwrap_or(Backend::Codex)
}

#[derive(Deserialize)]
pub struct Config {
    pub prompt: PromptConfig,
    pub generator: GeneratorConfig,
    pub bookmark: BookmarkConfig,
    pub diff: DiffConfig,
}

#[derive(Deserialize)]
pub struct PromptConfig {
    pub template: String,
}

#[derive(Deserialize)]
pub struct GeneratorConfig {
    pub command: String,
    pub args: Vec<String>,
    pub default_model: String,
    pub default_commit_message: String,
}

#[derive(Deserialize)]
pub struct BookmarkConfig {
    pub prompt_template: String,
}

#[derive(Deserialize)]
pub struct DiffConfig {
    pub collapse_patterns: Vec<String>,
    pub max_diff_lines: usize,
    pub max_diff_bytes: usize,
    pub max_total_diff_lines: usize,
    pub max_total_diff_bytes: usize,
}

pub static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let toml_str = match backend() {
        Backend::Claude => include_str!("../assets/claude-config.toml"),
        Backend::Codex => include_str!("../assets/codex-config.toml"),
    };
    from_str(toml_str).expect("Failed to parse embedded config")
});
