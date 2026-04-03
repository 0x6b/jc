use std::{fmt::Write, fs::read_to_string, path::Path};

use anyhow::Result;
use futures::StreamExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use jj_lib::{
    backend::{FileId, TreeValue},
    merged_tree::MergedTree,
    repo::{ReadonlyRepo, Repo},
    repo_path::RepoPath,
};
use similar::TextDiff;
use tokio::{io::AsyncReadExt, try_join};
use tracing::{debug, trace, warn};

/// Summary of file changes between two trees
#[derive(Debug, Default)]
pub struct FileChangeSummary {
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub modified: Vec<String>,
}

const MAX_LINES: usize = 50;
const CONTEXT_LINES: usize = 2;

/// Build a GlobSet from pattern strings
pub fn build_collapse_matcher(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(e) => {
                warn!(pattern = %pattern, error = %e, "Invalid collapse pattern, skipping");
            }
        }
    }

    match builder.build() {
        Ok(set) => Some(set),
        Err(e) => {
            warn!(error = %e, "Failed to build collapse matcher");
            None
        }
    }
}

/// A gitattributes rule: a glob pattern and the reason it should be collapsed.
struct GitAttrRule {
    glob: Glob,
    reason: &'static str,
}

/// Matcher built from .gitattributes that identifies files to collapse.
pub struct GitAttrMatcher {
    globset: GlobSet,
    /// Parallel vec with globset — each glob's index maps to a reason string.
    reasons: Vec<&'static str>,
}

impl GitAttrMatcher {
    /// Returns the collapse reason if the path matches, or None.
    pub fn collapse_reason(&self, path: &str) -> Option<&'static str> {
        let matches = self.globset.matches(path);
        // Return the first match's reason
        matches.first().map(|&idx| self.reasons[idx])
    }
}

/// Parse a .gitattributes file from the workspace root.
///
/// Recognizes these attributes as collapse triggers:
/// - `-diff` or `diff=false`
/// - `binary`
/// - `linguist-generated` or `linguist-generated=true`
pub fn load_gitattributes(workspace_root: &Path) -> Option<GitAttrMatcher> {
    let path = workspace_root.join(".gitattributes");
    let content = read_to_string(&path).ok()?;

    let mut rules = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Format: <pattern> <attr1> <attr2> ...
        let mut parts = line.split_whitespace();
        let pattern = match parts.next() {
            Some(p) => p,
            None => continue,
        };

        let mut reason: Option<&'static str> = None;
        for attr in parts {
            match attr {
                "-diff" | "diff=false" => {
                    reason = Some("collapsed: gitattributes (-diff)");
                    break;
                }
                "binary" => {
                    reason = Some("collapsed: gitattributes (binary)");
                    break;
                }
                "linguist-generated" | "linguist-generated=true" => {
                    reason = Some("collapsed: gitattributes (linguist-generated)");
                    break;
                }
                _ => {}
            }
        }

        if let Some(reason) = reason {
            match Glob::new(pattern) {
                Ok(glob) => rules.push(GitAttrRule { glob, reason }),
                Err(e) => {
                    warn!(pattern = %pattern, error = %e, "Invalid gitattributes pattern, skipping");
                }
            }
        }
    }

    if rules.is_empty() {
        return None;
    }

    let mut builder = GlobSetBuilder::new();
    let mut reasons = Vec::with_capacity(rules.len());
    for rule in rules {
        builder.add(rule.glob);
        reasons.push(rule.reason);
    }

    match builder.build() {
        Ok(globset) => {
            debug!(count = reasons.len(), "Loaded gitattributes collapse rules");
            Some(GitAttrMatcher { globset, reasons })
        }
        Err(e) => {
            warn!(error = %e, "Failed to build gitattributes matcher");
            None
        }
    }
}

/// Read file content from store
async fn read_file_content(repo: &ReadonlyRepo, path: &RepoPath, id: &FileId) -> Result<Vec<u8>> {
    let mut content = Vec::new();
    repo.store()
        .read_file(path, id)
        .await?
        .read_to_end(&mut content)
        .await?;
    Ok(content)
}

/// Format file diff (added/removed) with line truncation
async fn format_added_removed_diff(
    repo: &ReadonlyRepo,
    path: &RepoPath,
    path_str: &str,
    id: &FileId,
    is_added: bool,
    max_lines: usize,
) -> Result<String> {
    let (status, from, to) = if is_added {
        ("new file", "/dev/null".to_string(), format!("b/{path_str}"))
    } else {
        ("deleted file", format!("a/{path_str}"), "/dev/null".to_string())
    };

    let mut output =
        format!("diff --git a/{path_str} b/{path_str}\n{status}\n--- {from}\n+++ {to}\n");
    let content = read_file_content(repo, path, id).await?;

    match String::from_utf8(content) {
        Ok(text) => {
            let lines: Vec<_> = text.lines().collect();
            let prefix = if is_added { '+' } else { '-' };

            lines.iter().take(max_lines).for_each(|line| {
                let _ = writeln!(output, "{prefix}{line}");
            });

            if lines.len() > max_lines {
                let _ = writeln!(output, "... ({} more lines)", lines.len() - max_lines);
            }
        }
        Err(_) => writeln!(output, "(binary file)")?,
    }

    Ok(output)
}

/// Determine the collapse reason based on limits
fn collapse_reason(
    gitattr_reason: Option<&'static str>,
    pattern_match: bool,
    line_count: usize,
    byte_size: usize,
    max_lines: usize,
    max_bytes: usize,
) -> &'static str {
    if let Some(reason) = gitattr_reason {
        reason
    } else if pattern_match {
        "collapsed: matches pattern"
    } else if line_count > max_lines {
        "collapsed: exceeds line limit"
    } else if byte_size > max_bytes {
        "collapsed: exceeds size limit"
    } else {
        "collapsed"
    }
}

/// Format a collapsed summary for files matching collapse patterns or size limits
fn format_collapsed_summary(
    path_str: &str,
    added: usize,
    removed: usize,
    status: &str,
    reason: &str,
) -> String {
    format!(
        "diff --git a/{path_str} b/{path_str}\n{status} (+{added} -{removed} lines, {reason})\n"
    )
}

/// Get the diff between two trees using jj-lib
pub async fn get_tree_diff(
    repo: &ReadonlyRepo,
    from_tree: &MergedTree,
    to_tree: &MergedTree,
    collapse_matcher: Option<&GlobSet>,
    gitattr_matcher: Option<&GitAttrMatcher>,
    max_diff_lines: usize,
    max_diff_bytes: usize,
) -> Result<String> {
    debug!("Starting tree diff");
    let mut output = String::new();
    let mut stream = from_tree.diff_stream(to_tree, &jj_lib::matchers::EverythingMatcher);
    let mut file_count = 0;
    let mut collapsed_count = 0;

    while let Some(entry) = stream.next().await {
        let path_str = entry.path.as_internal_file_string();
        let values = entry.values?;

        // Check if this file should be collapsed (gitattributes takes precedence)
        let gitattr_reason = gitattr_matcher.and_then(|m| m.collapse_reason(path_str));
        let should_collapse_pattern =
            collapse_matcher.map(|m| m.is_match(path_str)).unwrap_or(false);
        let should_collapse = gitattr_reason.is_some() || should_collapse_pattern;

        let diff_output = match (values.before.as_resolved(), values.after.as_resolved()) {
            (Some(None), Some(Some(TreeValue::File { id, .. }))) => {
                let content = read_file_content(repo, &entry.path, id).await?;
                let byte_size = content.len();
                let line_count = String::from_utf8_lossy(&content).lines().count();
                let should_collapse_size =
                    line_count > max_diff_lines || byte_size > max_diff_bytes;
                trace!(path = %path_str, collapsed = should_collapse, collapsed_size = should_collapse_size, lines = line_count, bytes = byte_size, "Processing added file");
                if should_collapse || should_collapse_size {
                    collapsed_count += 1;
                    let reason = collapse_reason(
                        gitattr_reason,
                        should_collapse_pattern,
                        line_count,
                        byte_size,
                        max_diff_lines,
                        max_diff_bytes,
                    );
                    format_collapsed_summary(path_str, line_count, 0, "new file", reason)
                } else {
                    format_added_removed_diff(repo, &entry.path, path_str, id, true, MAX_LINES)
                        .await?
                }
            }

            (Some(Some(TreeValue::File { id, .. })), Some(None)) => {
                let content = read_file_content(repo, &entry.path, id).await?;
                let byte_size = content.len();
                let line_count = String::from_utf8_lossy(&content).lines().count();
                let should_collapse_size =
                    line_count > max_diff_lines || byte_size > max_diff_bytes;
                trace!(path = %path_str, collapsed = should_collapse, collapsed_size = should_collapse_size, lines = line_count, bytes = byte_size, "Processing deleted file");
                if should_collapse || should_collapse_size {
                    collapsed_count += 1;
                    let reason = collapse_reason(
                        gitattr_reason,
                        should_collapse_pattern,
                        line_count,
                        byte_size,
                        max_diff_lines,
                        max_diff_bytes,
                    );
                    format_collapsed_summary(path_str, 0, line_count, "deleted file", reason)
                } else {
                    format_added_removed_diff(repo, &entry.path, path_str, id, false, MAX_LINES)
                        .await?
                }
            }

            (
                Some(Some(TreeValue::File { id: before_id, .. })),
                Some(Some(TreeValue::File { id: after_id, .. })),
            ) => {
                let (before_content, after_content) = try_join!(
                    read_file_content(repo, &entry.path, before_id),
                    read_file_content(repo, &entry.path, after_id)
                )?;

                // Compute byte_size before consuming the buffers
                let byte_size = before_content.len().max(after_content.len());

                match (String::from_utf8(before_content), String::from_utf8(after_content)) {
                    (Ok(before_text), Ok(after_text)) => {
                        let diff = TextDiff::from_lines(&before_text, &after_text);
                        let added = diff
                            .iter_all_changes()
                            .filter(|c| c.tag() == similar::ChangeTag::Insert)
                            .count();
                        let removed = diff
                            .iter_all_changes()
                            .filter(|c| c.tag() == similar::ChangeTag::Delete)
                            .count();
                        let should_collapse_size =
                            added + removed > max_diff_lines || byte_size > max_diff_bytes;
                        trace!(path = %path_str, collapsed = should_collapse, collapsed_size = should_collapse_size, lines = added + removed, bytes = byte_size, "Processing modified file");
                        if should_collapse || should_collapse_size {
                            collapsed_count += 1;
                            let reason = collapse_reason(
                                gitattr_reason,
                                should_collapse_pattern,
                                added + removed,
                                byte_size,
                                max_diff_lines,
                                max_diff_bytes,
                            );
                            format_collapsed_summary(path_str, added, removed, "modified", reason)
                        } else {
                            format!(
                                "diff --git a/{0} b/{0}\n{1}",
                                path_str,
                                diff.unified_diff()
                                    .context_radius(CONTEXT_LINES)
                                    .header(&format!("a/{path_str}"), &format!("b/{path_str}"))
                            )
                        }
                    }
                    _ => {
                        trace!(path = %path_str, "Binary file modified");
                        format!("diff --git a/{path_str} b/{path_str}\n(binary file modified)\n")
                    }
                }
            }
            _ => String::new(),
        };

        if !diff_output.is_empty() {
            file_count += 1;
            output.push_str(&diff_output);
        }
    }

    debug!(file_count, collapsed_count, output_len = output.len(), "Tree diff complete");
    Ok(output)
}

/// Get summary of file changes between two trees
pub async fn get_file_change_summary(
    from_tree: &MergedTree,
    to_tree: &MergedTree,
) -> FileChangeSummary {
    let mut summary = FileChangeSummary::default();
    let mut stream = from_tree.diff_stream(to_tree, &jj_lib::matchers::EverythingMatcher);

    while let Some(entry) = stream.next().await {
        let path_str = entry.path.as_internal_file_string().to_string();
        let values = match entry.values {
            Ok(v) => v,
            Err(_) => continue,
        };

        match (values.before.as_resolved(), values.after.as_resolved()) {
            // Added: before is None, after is Some
            (Some(None), Some(Some(TreeValue::File { .. }))) => {
                summary.added.push(path_str);
            }
            // Deleted: before is Some, after is None
            (Some(Some(TreeValue::File { .. })), Some(None)) => {
                summary.deleted.push(path_str);
            }
            // Modified: both before and after are Some
            (Some(Some(TreeValue::File { .. })), Some(Some(TreeValue::File { .. }))) => {
                summary.modified.push(path_str);
            }
            _ => {}
        }
    }

    summary
}
