use std::sync::LazyLock;

use regex::Regex;
use tracing::{debug, error, trace, warn};

use crate::{
    codex_client::{CodexRequest, invoke_codex},
    config::CONFIG,
    text_formatter::format_text,
};

static CONVENTIONAL_COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z]+(?:\([^)]+\))?(?:!)?:\s.+")
        .expect("Failed to compile conventional commit regex")
});

/// Generates commit messages using Codex CLI based on diff content
pub struct CommitMessageGenerator {
    prompt_template: String,
    command: String,
    args: Vec<String>,
    language: String,
    model: String,
}

impl CommitMessageGenerator {
    pub fn new(language: &str, model: &str) -> Self {
        Self {
            prompt_template: CONFIG.prompt.template.clone(),
            command: CONFIG.generator.command.clone(),
            args: CONFIG.generator.args.clone(),
            language: language.to_string(),
            model: model.to_string(),
        }
    }

    pub async fn generate(&self, diff_content: &str) -> Option<String> {
        debug!(diff_len = diff_content.len(), "Starting commit message generation");
        self.try_generate(diff_content).await.map(|message| {
            let first_line = message.lines().next().unwrap_or("").trim();
            let message = if CONVENTIONAL_COMMIT_RE.is_match(first_line) {
                debug!("Generated message follows conventional commit format");
                message
            } else {
                error!(first_line = %first_line, "Generated message does not follow conventional commit format, prepending default");
                format!("{}\n\n{message}", CONFIG.generator.default_commit_message)
            };
            format_text(&message, 72)
        })
    }

    async fn try_generate(&self, diff_content: &str) -> Option<String> {
        let prompt = self
            .prompt_template
            .replace("{language}", &self.language)
            .replace("{diff_content}", diff_content);
        trace!(prompt_len = prompt.len(), "Prepared prompt for Codex");

        let model = self.model.trim();
        let model =
            if model.is_empty() || model.eq_ignore_ascii_case("auto") { None } else { Some(model) };

        let request = CodexRequest {
            command: &self.command,
            args: &self.args,
            model,
            prompt: &prompt,
            spinner_message: "Generating commit message with Codex...",
        };

        let text = invoke_codex(&request).await?;
        let message = text.trim();

        if message.is_empty() {
            warn!("Codex CLI returned empty message");
            return None;
        }

        trace!(message = %message, "Codex CLI output");
        Some(message.to_string())
    }
}

impl Default for CommitMessageGenerator {
    fn default() -> Self {
        Self::new("English", "auto")
    }
}
