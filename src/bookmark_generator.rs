use std::sync::LazyLock;

use regex::Regex;
use tracing::{debug, trace, warn};

use crate::{
    config::CONFIG,
    llm_client::{LlmRequest, invoke},
};

static VALID_BOOKMARK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z][a-z0-9]*(-[a-z][a-z0-9]*){1,5}$").expect("Failed to compile bookmark regex")
});

pub struct BookmarkGenerator {
    prompt_template: String,
    command: String,
    args: Vec<String>,
    model: String,
}

impl BookmarkGenerator {
    pub fn new(model: &str) -> Self {
        Self {
            prompt_template: CONFIG.bookmark.prompt_template.clone(),
            command: CONFIG.generator.command.clone(),
            args: CONFIG.generator.args.clone(),
            model: model.to_string(),
        }
    }

    pub async fn generate(&self, commit_summaries: &str) -> Option<String> {
        debug!(summaries_len = commit_summaries.len(), "Starting bookmark name generation");
        self.try_generate(commit_summaries).await.and_then(|name| {
            let name = name.trim().to_lowercase();
            if VALID_BOOKMARK_RE.is_match(&name) {
                debug!(bookmark = %name, "Generated valid bookmark name");
                Some(name)
            } else {
                warn!(bookmark = %name, "Generated bookmark name doesn't match expected format");
                None
            }
        })
    }

    async fn try_generate(&self, commit_summaries: &str) -> Option<String> {
        let prompt = self.prompt_template.replace("{commit_summaries}", commit_summaries);
        trace!(prompt_len = prompt.len(), "Prepared prompt");

        let model = self.model.trim();
        let model =
            if model.is_empty() || model.eq_ignore_ascii_case("auto") { None } else { Some(model) };

        let request = LlmRequest {
            command: &self.command,
            args: &self.args,
            model,
            prompt: &prompt,
            spinner_message: "Generating bookmark name...",
        };

        let text = invoke(&request).await?;
        let bookmark = text.trim();

        if bookmark.is_empty() {
            warn!("LLM CLI returned empty bookmark");
            return None;
        }

        trace!(bookmark = %bookmark, "LLM CLI output");
        Some(bookmark.to_string())
    }
}
