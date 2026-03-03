use std::{process::Stdio, time::Duration};

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{Value, from_str};
use tokio::{io::AsyncWriteExt, process::Command};
use tracing::{debug, trace, warn};

/// Configuration for Codex CLI invocation
pub struct CodexRequest<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub model: Option<&'a str>,
    pub prompt: &'a str,
    pub spinner_message: &'a str,
}

/// Invokes Codex CLI and returns the result text.
///
/// Uses async I/O to write stdin and read stdout/stderr concurrently,
/// avoiding pipe buffer deadlocks with large prompts.
/// Returns `None` if the command fails or output cannot be parsed.
pub async fn invoke_codex(request: &CodexRequest<'_>) -> Option<String> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("✶✸✹✺✹✷")
            .template("{spinner:.yellow} {msg}")
            .ok()?,
    );
    spinner.set_message(request.spinner_message.to_string());
    spinner.enable_steady_tick(Duration::from_millis(200));

    debug!(
        command = %request.command,
        args = ?request.args,
        model = request.model.unwrap_or("auto"),
        prompt_len = request.prompt.len(),
        "Executing Codex CLI via stdin"
    );

    let mut command = Command::new(request.command);
    command
        .args(request.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(model) = request.model {
        command.arg("--model").arg(model);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            warn!(error = %e, "Failed to spawn Codex CLI");
            spinner.finish_and_clear();
            return None;
        }
    };

    // Write prompt to stdin in a separate task to avoid pipe buffer deadlock:
    // if the prompt exceeds the OS pipe buffer (~64KB), write_all blocks while
    // the child may simultaneously fill stdout/stderr buffers and block on write.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let prompt_bytes = request.prompt.as_bytes().to_vec();
    let stdin_task = tokio::spawn(async move {
        stdin.write_all(&prompt_bytes).await?;
        stdin.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });

    // Concurrently read stdout/stderr and wait for exit
    let output = child.wait_with_output().await;

    // Check stdin write result
    match stdin_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = %e, "Failed to write prompt to stdin"),
        Err(e) => warn!(error = %e, "stdin write task panicked"),
    }

    let result = match output {
        Ok(output) => {
            debug!(
                status = %output.status,
                stdout_len = output.stdout.len(),
                stderr_len = output.stderr.len(),
                "Codex CLI completed"
            );

            let raw_output = String::from_utf8_lossy(&output.stdout);

            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                if !stderr.trim().is_empty() {
                    warn!(status = %output.status, stderr = %stderr, "Codex CLI failed");
                }
            } else if !stderr.trim().is_empty() {
                trace!(stderr = %stderr, "Codex CLI stderr");
            }

            trace!(raw_output = %raw_output, "Codex CLI raw output");
            parse_result_text(&raw_output)
        }
        Err(e) => {
            warn!(error = %e, "Failed to wait for Codex CLI");
            None
        }
    };

    spinner.finish_and_clear();
    result
}

/// Parse Codex CLI output and extract the result text.
///
/// Supports:
/// - Claude JSON output (for backward compatibility)
/// - Object: `{"type": "result", "result": "text", ...}`
/// - Array: `[..., {"type": "result", "result": "text", ...}]`
///
/// - Codex JSONL events:
///   `{"type":"item.completed","item":{"type":"agent_message","text":"..."}}`
/// - Plain text output (best-effort fallback)
fn parse_result_text(raw_output: &str) -> Option<String> {
    if let Ok(json) = from_str::<Value>(raw_output) {
        let result_obj = if let Some(arr) = json.as_array() {
            arr.iter()
                .rfind(|obj| obj.get("type").and_then(|v| v.as_str()) == Some("result"))
        } else {
            Some(&json)
        };

        if let Some(result_obj) = result_obj
            && result_obj.get("type").and_then(|v| v.as_str()) == Some("result")
        {
            if result_obj.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                let error_text = result_obj
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                warn!(error = %error_text, "Provider CLI returned an error");
                return None;
            }

            if let Some(text) = result_obj.get("result").and_then(|v| v.as_str()) {
                return Some(text.to_string());
            }

            warn!("Provider JSON missing 'result' text field");
            return None;
        }
    }

    let mut last_agent_message: Option<String> = None;
    let mut last_error: Option<String> = None;
    for line in raw_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok(event) = from_str::<Value>(line) else {
            continue;
        };
        let event_type = event.get("type").and_then(|v| v.as_str());
        match event_type {
            Some("item.completed") => {
                let item = event.get("item");
                let item_type = item.and_then(|v| v.get("type")).and_then(|v| v.as_str());
                let text = item.and_then(|v| v.get("text")).and_then(|v| v.as_str());
                if item_type == Some("agent_message") {
                    if let Some(text) = text {
                        last_agent_message = Some(text.to_string());
                    }
                } else if item_type == Some("reasoning") {
                    trace!(reasoning = ?text, "Codex reasoning item");
                }
            }
            Some("error") => {
                if let Some(message) = event.get("message").and_then(|v| v.as_str()) {
                    last_error = Some(message.to_string());
                }
            }
            Some("turn.failed") => {
                if let Some(message) = event
                    .get("error")
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                {
                    last_error = Some(message.to_string());
                }
            }
            _ => {}
        }
    }

    if let Some(message) = last_agent_message {
        return Some(message);
    }
    if let Some(error) = last_error {
        warn!(error = %error, "Codex CLI returned an error");
        return None;
    }

    let plain_text = raw_output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('{') && !line.starts_with('['))
        .filter(|line| *line != "Reading prompt from stdin...")
        .collect::<Vec<_>>()
        .join("\n");

    if !plain_text.is_empty() {
        Some(plain_text)
    } else {
        warn!("Codex CLI output did not contain a parseable result");
        None
    }
}
