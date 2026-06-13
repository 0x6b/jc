use std::sync::LazyLock;

use regex::Regex;
use tracing::{debug, error, trace, warn};

use crate::{
    config::{CONFIG, backend},
    llm_client::{LlmRequest, RETRY_EMPHASIS, invoke},
    prompt_store::{extract_selection, render_numbered_prompts},
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

    /// Generate a commit message.
    ///
    /// When `instruction_prompts` is non-empty, the prompts are offered to the LLM as the
    /// motivation for the change, and the returned selection lists which of them (1-based) the
    /// LLM judged relevant to the diff. The selection is `None` when no instructions were given
    /// or the LLM did not report a usable selection.
    pub async fn generate(
        &self,
        diff_content: &str,
        instruction_prompts: &[String],
    ) -> Option<(String, Option<Vec<usize>>)> {
        debug!(diff_len = diff_content.len(), "Starting commit message generation");
        // Try once normally; if the output fails the format check, retry once with an
        // emphasized prompt. If still malformed, fall back to prepending the default title.
        let mut last_message = None;
        for emphasize in [false, true] {
            if emphasize {
                debug!("Retrying commit message generation with emphasized prompt");
            }
            let Some(raw) = self.try_generate(diff_content, instruction_prompts, emphasize).await
            else {
                continue;
            };
            let (message, selection) =
                if instruction_prompts.is_empty() { (raw, None) } else { extract_selection(&raw) };
            let first_line = message.lines().next().unwrap_or("").trim();
            if CONVENTIONAL_COMMIT_RE.is_match(first_line) {
                debug!("Generated message follows conventional commit format");
                return Some((format_text(&message, 72), selection));
            }
            warn!(first_line = %first_line, emphasize, "Generated message does not follow conventional commit format");
            last_message = Some((message, selection));
        }
        last_message.map(|(message, selection)| {
            error!("Message still malformed after retry, prepending default");
            let message = format_text(
                &format!("{}\n\n{message}", CONFIG.generator.default_commit_message),
                72,
            );
            (message, selection)
        })
    }

    async fn try_generate(
        &self,
        diff_content: &str,
        instruction_prompts: &[String],
        emphasize: bool,
    ) -> Option<String> {
        let instructions_section = if instruction_prompts.is_empty() {
            String::new()
        } else {
            CONFIG
                .prompt
                .instructions_template
                .replace("{numbered_prompts}", &render_numbered_prompts(instruction_prompts))
        };
        let mut prompt = CONFIG
            .prompt
            .template
            .replace("{language}", self.language)
            .replace("{diff_content}", diff_content)
            .replace("{instructions_section}", &instructions_section);
        if emphasize {
            prompt.insert_str(0, RETRY_EMPHASIS);
        }
        trace!(prompt_len = prompt.len(), "Prepared prompt");

        let spinner = format!("Generating commit message with {}...", backend());
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
