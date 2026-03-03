use std::{process::Stdio, time::Duration};

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{Value, from_str};
use tokio::{io::AsyncWriteExt, process::Command};
use tracing::{debug, trace, warn};

/// Configuration for Claude CLI invocation
pub struct ClaudeRequest<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub model: &'a str,
    pub prompt: &'a str,
    pub spinner_message: &'a str,
}

/// Invokes Claude CLI and returns the result text.
///
/// Uses async I/O to write stdin and read stdout/stderr concurrently,
/// avoiding pipe buffer deadlocks with large prompts.
/// Returns `None` if the command fails or output cannot be parsed.
pub async fn invoke_claude(request: &ClaudeRequest<'_>) -> Option<String> {
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
        model = %request.model,
        prompt_len = request.prompt.len(),
        "Executing Claude CLI via stdin"
    );

    let mut child = match Command::new(request.command)
        .env_remove("CLAUDECODE")
        .args(request.args)
        .arg("--model")
        .arg(request.model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            warn!(error = %e, "Failed to spawn Claude CLI");
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
                "Claude CLI completed"
            );

            let raw_output = String::from_utf8_lossy(&output.stdout);

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.trim().is_empty() {
                    warn!(status = %output.status, stderr = %stderr, "Claude CLI failed");
                }
            }

            trace!(raw_output = %raw_output, "Claude CLI raw output");
            parse_result_text(&raw_output)
        }
        Err(e) => {
            warn!(error = %e, "Failed to wait for Claude CLI");
            None
        }
    };

    spinner.finish_and_clear();
    result
}

/// Parse Claude CLI JSON output and extract the result text.
///
/// Handles both single-object and array (streaming) formats:
/// - Object: `{"type": "result", "result": "text", ...}`
/// - Array: `[..., {"type": "result", "result": "text", ...}]`
///
/// Checks `is_error` on the result event and returns `None` with a warning
/// if the CLI reported an error (e.g., rate limit, auth failure).
fn parse_result_text(raw_output: &str) -> Option<String> {
    match from_str::<Value>(raw_output) {
        Ok(json) => {
            let result_obj = if let Some(arr) = json.as_array() {
                arr.iter()
                    .rfind(|obj| obj.get("type").and_then(|v| v.as_str()) == Some("result"))
            } else {
                Some(&json)
            };

            let Some(result_obj) = result_obj else {
                warn!("Claude CLI JSON missing 'result' event");
                return None;
            };

            if result_obj.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                let error_text = result_obj
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                warn!(error = %error_text, "Claude CLI returned an error");
                return None;
            }

            let text = result_obj.get("result").and_then(|v| v.as_str());

            if let Some(text) = text {
                Some(text.to_string())
            } else {
                warn!("Claude CLI JSON missing 'result' text field");
                None
            }
        }
        Err(e) => {
            warn!(error = %e, raw = %raw_output, "Failed to parse Claude CLI JSON output");
            None
        }
    }
}
