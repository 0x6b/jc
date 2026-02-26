use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{Value, from_str};
use tracing::{debug, trace, warn};

/// Configuration for Claude CLI invocation
pub struct ClaudeRequest<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub model: &'a str,
    pub json_schema: &'a str,
    pub prompt: &'a str,
    pub spinner_message: &'a str,
}

/// Invokes Claude CLI and returns the structured output JSON value.
///
/// Handles spinner display, subprocess spawning, and JSON parsing.
/// Returns `None` if the command fails or output cannot be parsed.
pub fn invoke_claude(request: &ClaudeRequest<'_>) -> Option<Value> {
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

    let result = Command::new(request.command)
        .env_remove("CLAUDECODE")
        .args(request.args)
        .arg("--model")
        .arg(request.model)
        .arg("--json-schema")
        .arg(request.json_schema)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(request.prompt.as_bytes())?;
            }
            child.wait_with_output()
        });

    let result = match result {
        Ok(output) => {
            debug!(
                status = %output.status,
                stdout_len = output.stdout.len(),
                stderr_len = output.stderr.len(),
                "Claude CLI completed"
            );
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(status = %output.status, stderr = %stderr, "Claude CLI failed");
                None
            } else {
                let raw_output = String::from_utf8_lossy(&output.stdout);
                trace!(raw_output = %raw_output, "Claude CLI raw output");
                parse_structured_output(&raw_output)
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to execute Claude CLI");
            None
        }
    };

    spinner.finish_and_clear();
    result
}

/// Parse Claude CLI JSON output and extract the structured_output field.
fn parse_structured_output(raw_output: &str) -> Option<Value> {
    match from_str::<Value>(raw_output) {
        Ok(json) => {
            let structured = if let Some(arr) = json.as_array() {
                arr.iter()
                    .rfind(|obj| obj.get("type").and_then(|v| v.as_str()) == Some("result"))
                    .and_then(|obj| obj.get("structured_output"))
            } else {
                json.get("structured_output")
            };

            if let Some(structured) = structured {
                Some(structured.clone())
            } else {
                warn!("Claude CLI JSON missing 'structured_output' field");
                None
            }
        }
        Err(e) => {
            warn!(error = %e, raw = %raw_output, "Failed to parse Claude CLI JSON output");
            None
        }
    }
}
