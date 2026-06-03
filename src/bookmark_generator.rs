use std::sync::LazyLock;

use regex::Regex;
use tracing::{debug, trace, warn};

use crate::{
    config::{CONFIG, backend},
    llm_client::{LlmRequest, RETRY_EMPHASIS, invoke},
};

static VALID_BOOKMARK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z][a-z0-9]*(-[a-z0-9]+){1,7}$").expect("Failed to compile bookmark regex")
});

pub struct BookmarkGenerator<'a> {
    model: &'a str,
}

impl<'a> BookmarkGenerator<'a> {
    pub fn new(model: &'a str) -> Self {
        Self { model }
    }

    pub async fn generate(&self, commit_summaries: &str) -> Option<String> {
        debug!(summaries_len = commit_summaries.len(), "Starting bookmark name generation");
        // Try once normally; if the output contains prose and fails the format check,
        // retry once with an emphasized prompt, then give up.
        for emphasize in [false, true] {
            if emphasize {
                debug!("Retrying bookmark generation with emphasized prompt");
            }
            let Some(name) = self.try_generate(commit_summaries, emphasize).await else {
                continue;
            };
            let name = name.trim().to_lowercase();
            if VALID_BOOKMARK_RE.is_match(&name) {
                debug!(bookmark = %name, "Generated valid bookmark name");
                return Some(name);
            }
            warn!(bookmark = %name, emphasize, "Generated bookmark name doesn't match expected format");
        }
        None
    }

    async fn try_generate(&self, commit_summaries: &str, emphasize: bool) -> Option<String> {
        let mut prompt = CONFIG
            .bookmark
            .prompt_template
            .replace("{commit_summaries}", commit_summaries);
        if emphasize {
            prompt.insert_str(0, RETRY_EMPHASIS);
        }
        trace!(prompt_len = prompt.len(), "Prepared prompt");

        let spinner = format!("Generating bookmark name with {}...", backend());
        let request = LlmRequest::new(&CONFIG.generator, self.model, &prompt, &spinner);

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
