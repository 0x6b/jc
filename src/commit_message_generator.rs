use std::sync::LazyLock;

use regex::Regex;
use tracing::{debug, error, trace, warn};

use crate::{
    config::CONFIG,
    llm_client::{LlmRequest, invoke},
    text_formatter::format_text,
};

static CONVENTIONAL_COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z]+(?:\([^)]+\))?(?:!)?:\s.+")
        .expect("Failed to compile conventional commit regex")
});

/// Generates commit messages using an LLM CLI based on diff content
pub struct CommitMessageGenerator<'a> {
    language: &'a str,
    model: &'a str,
}

impl<'a> CommitMessageGenerator<'a> {
    pub fn new(language: &'a str, model: &'a str) -> Self {
        Self { language, model }
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
        let prompt = CONFIG
            .prompt
            .template
            .replace("{language}", self.language)
            .replace("{diff_content}", diff_content);
        trace!(prompt_len = prompt.len(), "Prepared prompt");

        let spinner = format!("Generating commit message with {}...", crate::config::backend());
        let request = LlmRequest::new(&CONFIG.generator, self.model, &prompt, &spinner);

        let text = invoke(&request).await?;
        let message = text.trim();

        if message.is_empty() {
            warn!("LLM CLI returned empty message");
            return None;
        }

        trace!(message = %message, "LLM CLI output");
        Some(message.to_string())
    }
}

impl Default for CommitMessageGenerator<'_> {
    fn default() -> Self {
        Self::new("English", "auto")
    }
}
