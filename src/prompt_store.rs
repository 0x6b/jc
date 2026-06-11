//! Records user prompts sent to coding agents and folds them into commit messages.
//!
//! This mirrors the behaviour of [`ayumi`](https://github.com/stefafafan/ayumi): user
//! instructions are captured (via an agent's `UserPromptSubmit` hook) and later inserted into the
//! generated commit description as a quoted "AI Instructions" section so the steps that produced a
//! change are recorded alongside it.
//!
//! Only user instructions are stored. AI responses, transcripts, reasoning, and tool output are
//! never recorded.

use std::{
    env::{var, var_os},
    fs::{OpenOptions, canonicalize, create_dir_all, read_to_string},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use dirs::data_dir;
use serde::{Deserialize, Serialize};
use serde_json::{Value, from_str, to_string};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

const DEFAULT_HEADING: &str = "AI Instructions";

/// A single recorded user prompt with the time it was captured.
#[derive(Serialize, Deserialize)]
struct PromptEntry {
    timestamp: String,
    prompt: String,
}

/// Append-only store of user prompts, keyed by jj workspace.
pub struct PromptStore {
    storage_dir: PathBuf,
    heading: String,
}

impl PromptStore {
    /// Build a store from the environment.
    ///
    /// - `JC_PROMPT_STORAGE_DIR` overrides the storage directory (default: `<data dir>/jc`).
    /// - `JC_PROMPT_HEADING` overrides the section heading (default: `AI Instructions`).
    pub fn new() -> Self {
        let storage_dir = var_os("JC_PROMPT_STORAGE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_storage_dir);
        let heading = var("JC_PROMPT_HEADING")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HEADING.to_string());
        Self { storage_dir, heading }
    }

    /// Path to the JSONL log for a given workspace.
    fn log_path(&self, workspace_root: &Path) -> PathBuf {
        let id = hash_path(workspace_root);
        self.storage_dir.join("workspaces").join(format!("{id}.jsonl"))
    }

    /// Record a prompt for the given workspace.
    ///
    /// `raw` is the bytes read from stdin: either a coding-agent hook JSON payload (the `prompt`,
    /// `user_prompt`, or `input` field is extracted) or plain text.
    pub fn add(&self, workspace_root: &Path, raw: &[u8]) -> Result<()> {
        let prompt = extract_prompt(raw)?;
        if prompt.trim().is_empty() {
            bail!("empty prompt");
        }

        self.validate_outside(workspace_root)?;

        let entry = PromptEntry { timestamp: Utc::now().to_rfc3339(), prompt };
        let line = to_string(&entry)?;

        let path = self.log_path(workspace_root);
        if let Some(parent) = path.parent() {
            create_dir_all(parent)
                .with_context(|| format!("failed to create storage dir {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open prompt log {}", path.display()))?;
        writeln!(file, "{line}")?;
        debug!(path = %path.display(), "Recorded prompt");
        Ok(())
    }

    /// Return prompts recorded after `cutoff` (or all prompts when `cutoff` is `None`).
    pub fn prompts_since(
        &self,
        workspace_root: &Path,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<Vec<String>> {
        let path = self.log_path(workspace_root);
        let content = match read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read prompt log {}", path.display()));
            }
        };

        let mut prompts = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: PromptEntry = match from_str(line) {
                Ok(entry) => entry,
                Err(e) => {
                    warn!(error = %e, "Skipping malformed prompt log line");
                    continue;
                }
            };
            let include = match cutoff {
                None => true,
                Some(cutoff) => DateTime::parse_from_rfc3339(&entry.timestamp)
                    .map(|t| t.with_timezone(&Utc) > cutoff)
                    .unwrap_or(true),
            };
            if include {
                prompts.push(entry.prompt);
            }
        }
        Ok(prompts)
    }

    /// Append a quoted "AI Instructions" section to a commit message.
    ///
    /// Returns the message unchanged when there are no prompts or the section already exists.
    pub fn append_instructions(&self, message: &str, prompts: &[String]) -> String {
        if prompts.is_empty() || self.has_heading(message) {
            return message.to_string();
        }

        let mut out = message.trim_end_matches(['\r', '\n']).to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&self.heading);
        out.push_str(":\n");
        for (i, prompt) in prompts.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&quote_prompt(prompt));
        }
        out
    }

    /// Whether the message already contains the instruction heading.
    fn has_heading(&self, message: &str) -> bool {
        let target = format!("{}:", self.heading);
        message.lines().any(|line| line.trim() == target)
    }

    /// Refuse to write the log inside the workspace, so prompts are never committed.
    fn validate_outside(&self, workspace_root: &Path) -> Result<()> {
        let storage = canonical(&self.storage_dir);
        let root = canonical(workspace_root);
        if storage.starts_with(&root) {
            bail!(
                "JC_PROMPT_STORAGE_DIR ({}) must be outside the workspace",
                self.storage_dir.display()
            );
        }
        Ok(())
    }
}

/// Default storage directory: `<platform data dir>/jc`, falling back to `./.jc-prompts`.
fn default_storage_dir() -> PathBuf {
    data_dir()
        .map(|d| d.join("jc"))
        .unwrap_or_else(|| PathBuf::from(".jc-prompts"))
}

/// Stable hex identifier for a workspace, derived from its canonical path.
fn hash_path(path: &Path) -> String {
    let canon = canonical(path);
    let mut hasher = Sha256::new();
    hasher.update(canon.to_string_lossy().as_bytes());
    hasher.finalize().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Best-effort canonicalization that falls back to the original path.
fn canonical(path: &Path) -> PathBuf {
    canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Extract the user prompt from agent hook JSON or treat the input as plain text.
fn extract_prompt(data: &[u8]) -> Result<String> {
    match from_str::<Value>(&String::from_utf8_lossy(data)) {
        Ok(Value::String(s)) => Ok(s),
        Ok(value) => prompt_field(&value)
            .context("prompt field not found in hook input (expected prompt/user_prompt/input)"),
        // Not JSON: treat the raw input as the prompt.
        Err(_) => Ok(String::from_utf8_lossy(data).to_string()),
    }
}

/// Recursively search a JSON value for a `prompt`, `user_prompt`, or `input` string field.
fn prompt_field(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => {
            for key in ["prompt", "user_prompt", "input"] {
                if let Some(Value::String(s)) = map.get(key) {
                    return Some(s.clone());
                }
            }
            map.values().find_map(prompt_field)
        }
        Value::Array(items) => items.iter().find_map(prompt_field),
        _ => None,
    }
}

/// Render a prompt as a Markdown quote block, one `> ` per line.
///
/// Trailing newlines are stripped so a prompt captured with a trailing newline (e.g. from
/// `echo "..." | jc add`) does not produce a dangling `> ` blank line. Internal blank lines are
/// preserved so multi-paragraph prompts keep their structure.
fn quote_prompt(prompt: &str) -> String {
    let normalized = prompt.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim_end_matches('\n');
    let mut out = String::new();
    for line in normalized.split('\n') {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PromptStore {
        PromptStore {
            storage_dir: PathBuf::from("/tmp/jc-test"),
            heading: "AI Instructions".into(),
        }
    }

    #[test]
    fn extract_plain_text() {
        assert_eq!(extract_prompt(b"hello world").unwrap(), "hello world");
    }

    #[test]
    fn extract_json_prompt_field() {
        let raw = br#"{"session_id":"x","prompt":"do the thing"}"#;
        assert_eq!(extract_prompt(raw).unwrap(), "do the thing");
    }

    #[test]
    fn extract_json_nested_user_prompt() {
        let raw = br#"{"hook":{"user_prompt":"nested instruction"}}"#;
        assert_eq!(extract_prompt(raw).unwrap(), "nested instruction");
    }

    #[test]
    fn extract_bare_json_string() {
        assert_eq!(extract_prompt(br#""just a string""#).unwrap(), "just a string");
    }

    #[test]
    fn append_section_formats_quotes() {
        let result = store().append_instructions(
            "feat: add thing",
            &["first".to_string(), "second\nline".to_string()],
        );
        assert_eq!(result, "feat: add thing\n\nAI Instructions:\n> first\n\n> second\n> line\n");
    }

    #[test]
    fn append_section_skips_when_empty() {
        assert_eq!(store().append_instructions("feat: x", &[]), "feat: x");
    }

    #[test]
    fn append_section_idempotent() {
        let once = store().append_instructions("feat: x", &["a".to_string()]);
        let twice = store().append_instructions(&once, &["a".to_string()]);
        assert_eq!(once, twice);
    }
}
