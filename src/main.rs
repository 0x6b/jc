mod bookmark_generator;
mod commit_message_generator;
mod config;
mod diff;
mod llm_client;
mod prompt_store;
mod text_formatter;

use std::{
    borrow::ToOwned,
    collections::{HashMap, HashSet},
    env::{current_dir, current_exe, var, vars},
    fmt::Write,
    fs::{create_dir_all, read_to_string, write},
    io::{Read, stdin},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bookmark_generator::BookmarkGenerator;
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use colored::Colorize;
use commit_message_generator::CommitMessageGenerator;
use config::{Backend, CONFIG, set_backend};
use console::strip_ansi_codes;
use diff::{
    FileChangeSummary, TreeDiffResult, build_collapse_matcher, get_file_change_summary,
    get_tree_diff, load_gitattributes,
};
use dirs::{config_dir, home_dir};
use futures::TryStreamExt;
use gethostname::gethostname;
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    config::{ConfigLayer, ConfigResolutionContext, ConfigSource, StackedConfig, resolve},
    dsl_util::AliasesMap,
    git::{self, GitImportOptions, export_refs, import_refs},
    gitignore::GitIgnoreFile,
    id_prefix::IdPrefixContext,
    matchers::{EverythingMatcher, NothingMatcher},
    merged_tree::MergedTree,
    object_id::ObjectId,
    op_store::RefTarget,
    ref_name::{RefName, RemoteName},
    repo::{ReadonlyRepo, Repo, StoreFactories},
    repo_path::{RepoPath, RepoPathUiConverter::Fs},
    revset::{
        RevsetAliasesMap, RevsetDiagnostics, RevsetExpression, RevsetExtensions,
        RevsetParseContext, RevsetWorkspaceContext, SymbolResolver, parse,
    },
    settings::UserSettings,
    time_util::DatePatternContext,
    working_copy::SnapshotOptions,
    workspace::{Workspace, default_working_copy_factories},
};
use prompt_store::{PromptStore, select_prompts};
use serde_json::{Value, from_str, json, to_string_pretty};
use tracing::{Level, debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use unicode_width::UnicodeWidthStr;

const JC_LONG_ABOUT: &str = "\
jc generates conventional commit messages and bookmark names for jj workspaces using an LLM backend.

With no subcommand, jc runs `commit`: it snapshots the working copy, compares the working-copy commit against its parent, asks the configured backend for a message, and creates a jj commit.

Use `jc <command> --help` for command-specific inputs, side effects, and examples.";

const JC_AFTER_LONG_HELP: &str = "\
LLM QUICK REFERENCE:
  - Default action: `jc` is equivalent to `jc commit`.
  - Safe preview: add `--dry-run` to `commit`, `describe`, or `bookmark` to print the generated result without mutating the repo.
  - Workspace selection: use `-p <path>` when running from outside the jj workspace.
  - Backend/model: use `--backend codex|claude`, `--model <model>`, or env vars `JC_BACKEND` and `JC_MODEL`.
  - Language: `commit` and `describe` accept `--language <name>` or `JC_LANGUAGE`.
  - Prompt capture: `jc add` records stdin for later commit messages; `jc install` adds the hook that runs it.
  - Prompt inclusion: recorded prompts are appended unless `--no-instructions` is used; `--infer` asks the LLM to keep only relevant prompts.
  - Output: successful mutating commands print a human summary; dry runs print only generated text or a bookmark name.

EXAMPLES:
  jc
  jc commit --dry-run
  jc describe --revision @- --dry-run
  jc bookmark --prefix feature --dry-run
  echo \"Refactor login flow\" | jc add -p /path/to/workspace";

const ADD_LONG_ABOUT: &str = "\
Record a user prompt for the current workspace.

The prompt is read from stdin and stored outside the repository. Later `jc commit` and `jc describe` can append recorded prompts as an \"AI Instructions\" section.";

const ADD_AFTER_LONG_HELP: &str = "\
INPUT:
  Accepts plain text, or a coding-agent hook payload as JSON. For JSON input, jc uses the first string field found among `prompt`, `user_prompt`, and `input`.

EXAMPLES:
  echo \"Add JWT authentication\" | jc add
  jc add -p /path/to/workspace < prompt.txt";

const INSTALL_LONG_ABOUT: &str = "\
Install the UserPromptSubmit hook for this workspace.

jc writes or updates `.claude/settings.local.json` so future prompts are passed to `jc add -p <workspace-root>` automatically. Existing unrelated settings and hooks are preserved.";

const INSTALL_AFTER_LONG_HELP: &str = "\
BEHAVIOR:
  - Creates `.claude/settings.local.json` if needed.
  - Re-running is idempotent.
  - If the workspace path changed, the existing jc hook is updated instead of duplicated.

EXAMPLES:
  jc install
  jc install -p /path/to/workspace";

const BOOKMARK_LONG_ABOUT: &str = "\
Generate or move a jj bookmark for the current branch.

jc compares commit summaries in the `from..to` range. If a local bookmark already exists in that range, jc moves it to the target revision. Otherwise it asks the LLM for a new bookmark name and exports the resulting bookmark to git refs.";

const BOOKMARK_AFTER_LONG_HELP: &str = "\
DEFAULTS:
  - `--from` tries develop, main, master, then trunk, preferring `<name>@origin` over local `<name>`.
  - `--to` defaults to `@`; if `@` is empty, jc uses `@-` as the bookmark target.

EXAMPLES:
  jc bookmark
  jc bookmark --dry-run
  jc bookmark --from main@origin --to @ --prefix feature
  jc b --prefix fix";

const COMMIT_LONG_ABOUT: &str = "\
Generate a commit message and create a jj commit.

jc snapshots the working copy, diffs the working-copy commit against its parent, asks the selected LLM backend for a conventional commit message, optionally appends recorded user prompts, and commits the current tree with that message.";

const COMMIT_AFTER_LONG_HELP: &str = "\
BEHAVIOR:
  - This is the default command: `jc` is the same as `jc commit`.
  - If the working-copy commit already has a description, jc skips it unless `--force` is supplied.
  - `--dry-run` prints the generated commit message and creates no commit.
  - `--no-instructions` omits recorded prompts from the final message.
  - `--infer` passes recorded prompts to the LLM so only relevant prompts are quoted.

EXAMPLES:
  jc
  jc commit --dry-run
  jc commit --language Japanese
  jc c --force --no-instructions";

const DESCRIBE_LONG_ABOUT: &str = "\
Generate and set a jj commit description without creating a new commit.

jc resolves the requested revision, generates a message from its diff against the first parent, and rewrites that commit's description in place.";

const DESCRIBE_AFTER_LONG_HELP: &str = "\
BEHAVIOR:
  - Defaults to `--revision @`.
  - When describing `@`, jc snapshots the working copy first.
  - Refuses to overwrite an existing description unless `--force` is supplied.
  - `--dry-run` prints the generated description and does not rewrite the commit.

EXAMPLES:
  jc describe
  jc describe --revision @- --dry-run
  jc d -r @ --force
  jc d --language Japanese";

#[derive(Parser, Debug)]
#[command(about, version, long_about = JC_LONG_ABOUT, after_long_help = JC_AFTER_LONG_HELP)]
struct Args {
    /// Path to the workspace (defaults to current directory)
    #[arg(short, long, global = true)]
    path: Option<PathBuf>,

    /// LLM backend to use
    #[arg(short, long, default_value = "codex", env = "JC_BACKEND", global = true)]
    backend: Backend,

    /// Model to use for AI generation (defaults to backend's default)
    #[arg(short, long, default_value = "auto", env = "JC_MODEL", global = true)]
    model: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Record a user prompt (read from stdin) to fold into the next commit message
    #[command(visible_alias = "a", long_about = ADD_LONG_ABOUT, after_long_help = ADD_AFTER_LONG_HELP)]
    Add,
    /// Install the `UserPromptSubmit` hook for this workspace into .claude/settings.local.json
    #[command(long_about = INSTALL_LONG_ABOUT, after_long_help = INSTALL_AFTER_LONG_HELP)]
    Install,
    /// Generate a bookmark name for commits between the current revision and a base
    #[command(visible_alias = "b", long_about = BOOKMARK_LONG_ABOUT, after_long_help = BOOKMARK_AFTER_LONG_HELP)]
    Bookmark {
        /// Base revision to compare against (default: develop/main/master/trunk at origin or local)
        #[arg(short, long)]
        from: Option<String>,

        /// Target revision (default: @)
        #[arg(short, long, default_value = "@")]
        to: String,

        /// Prefix for the bookmark name (e.g., "feature" -> "feature/generated-name")
        #[arg(long)]
        prefix: Option<String>,

        /// Only print the generated name, don't create the bookmark
        #[arg(long)]
        dry_run: bool,
    },
    /// Generate a commit message and commit changes (default command)
    #[command(visible_alias = "c", long_about = COMMIT_LONG_ABOUT, after_long_help = COMMIT_AFTER_LONG_HELP)]
    Commit {
        /// Language to use for commit messages
        #[arg(short, long, default_value = "English", env = "JC_LANGUAGE")]
        language: String,

        /// Overwrite existing description
        #[arg(short, long)]
        force: bool,

        /// Only print the generated message, don't commit
        #[arg(long)]
        dry_run: bool,

        /// Do not append recorded user prompts as an "AI Instructions" section
        #[arg(long)]
        no_instructions: bool,

        /// Let the LLM use recorded prompts to explain WHY in the body, and quote only the
        /// prompts it judged relevant (ignored with --no-instructions)
        #[arg(long, env = "JC_INFER_INSTRUCTIONS")]
        infer: bool,
    },
    /// Generate and set a commit description without creating a new commit
    #[command(visible_alias = "d", long_about = DESCRIBE_LONG_ABOUT, after_long_help = DESCRIBE_AFTER_LONG_HELP)]
    Describe {
        /// Revision to describe (default: @)
        #[arg(short, long, default_value = "@")]
        revision: String,

        /// Language to use for commit messages
        #[arg(short, long, default_value = "English", env = "JC_LANGUAGE")]
        language: String,

        /// Overwrite existing description
        #[arg(short, long)]
        force: bool,

        /// Only print the generated message, don't apply it
        #[arg(long)]
        dry_run: bool,

        /// Do not append recorded user prompts as an "AI Instructions" section
        #[arg(long)]
        no_instructions: bool,

        /// Let the LLM use recorded prompts to explain WHY in the body, and quote only the
        /// prompts it judged relevant (ignored with --no-instructions)
        #[arg(long, env = "JC_INFER_INSTRUCTIONS")]
        infer: bool,
    },
}

impl Default for Commands {
    fn default() -> Self {
        Commands::Commit {
            language: "English".to_string(),
            force: false,
            dry_run: false,
            no_instructions: false,
            infer: false,
        }
    }
}

/// Load user configuration from standard jj config locations
fn load_user_config(config: &mut StackedConfig) -> Result<()> {
    let home = home_dir();
    let candidates: Vec<PathBuf> = [
        home.as_ref().map(|h| h.join(".jjconfig.toml")),
        home.as_ref().map(|h| h.join(".config/jj/config.toml")),
        config_dir().map(|c| c.join("jj/config.toml")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for path in candidates {
        if path.exists() {
            let layer = ConfigLayer::load_from_file(ConfigSource::User, path)?;
            config.add_layer(layer);
        }
    }
    Ok(())
}

/// Load gitignore files from global and workspace locations
fn load_base_ignores(workspace_root: &Path) -> Result<Arc<GitIgnoreFile>> {
    let mut git_ignores = GitIgnoreFile::empty();

    // Try to get global excludes file from git config
    let global_excludes = get_global_git_excludes_file();

    if let Some(excludes_path) = global_excludes {
        // Chain the global excludes file (ignore errors if file doesn't exist)
        git_ignores = git_ignores
            .chain_with_file(RepoPath::root(), excludes_path)
            .unwrap_or(git_ignores);
    }

    // Load workspace root .gitignore
    let workspace_gitignore = workspace_root.join(".gitignore");
    git_ignores = git_ignores
        .chain_with_file(RepoPath::root(), workspace_gitignore)
        .unwrap_or(git_ignores);

    Ok(git_ignores)
}

/// Get the global git excludes file path
fn get_global_git_excludes_file() -> Option<PathBuf> {
    // First, try to get from git config
    if let Ok(output) = Command::new("git")
        .args(["config", "--global", "--get", "core.excludesFile"])
        .output()
        && output.status.success()
        && let Ok(path_str) = std::str::from_utf8(&output.stdout)
    {
        let path_str = path_str.trim();
        if !path_str.is_empty() {
            // Expand ~ to home directory if present
            let expanded = if let Some(stripped) = path_str.strip_prefix("~/") {
                if let Some(home) = home_dir() {
                    home.join(stripped)
                } else {
                    PathBuf::from(path_str)
                }
            } else {
                PathBuf::from(path_str)
            };
            return Some(expanded);
        }
    }

    // Fall back to XDG_CONFIG_HOME/git/ignore or ~/.config/git/ignore
    if let Ok(xdg_config) = var("XDG_CONFIG_HOME")
        && !xdg_config.is_empty()
    {
        let path = PathBuf::from(xdg_config).join("git").join("ignore");
        if path.exists() {
            return Some(path);
        }
    }

    // Final fallback: ~/.config/git/ignore
    if let Some(home) = home_dir() {
        let path = home.join(".config").join("git").join("ignore");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Discover the jj workspace starting from the given directory
fn find_workspace(start_dir: &Path) -> Result<Workspace> {
    // First, find the workspace root directory
    let mut current_dir = start_dir;
    let workspace_root = loop {
        if current_dir.join(".jj").exists() {
            break current_dir;
        }

        match current_dir.parent() {
            Some(parent) => current_dir = parent,
            None => bail!(
                "No Jujutsu workspace found in '{}' or any parent directory",
                start_dir.display()
            ),
        }
    };

    // Build config with proper layers (with_defaults includes operation.hostname/username)
    let mut config = StackedConfig::with_defaults();

    // Load user configuration
    load_user_config(&mut config)?;

    // Load repository-specific configuration
    let repo_config_path = workspace_root.join(".jj").join("repo").join("config.toml");
    if repo_config_path.exists() {
        let layer = ConfigLayer::load_from_file(ConfigSource::Repo, repo_config_path)?;
        config.add_layer(layer);
    }

    // Resolve conditional scopes (e.g., --when.repositories)
    let hostname = gethostname().to_str().map(ToOwned::to_owned).unwrap_or_default();
    let home_dir = home_dir();
    let context = ConfigResolutionContext {
        home_dir: home_dir.as_deref(),
        repo_path: Some(workspace_root),
        workspace_path: Some(workspace_root),
        command: None,
        hostname: hostname.as_str(),
        environment: &vars().collect(),
    };
    let resolved_config = resolve(&config, &context)?;

    // Now create settings with resolved config
    let settings = UserSettings::from_config(resolved_config)?;
    let store_factories = StoreFactories::default();
    let working_copy_factories = default_working_copy_factories();

    // Load the workspace with the complete settings
    Workspace::load(&settings, workspace_root, &store_factories, &working_copy_factories)
        .context("Failed to load workspace")
}

/// Create a commit with the generated message
async fn create_commit(
    workspace: &Workspace,
    commit_message: &str,
    tree: MergedTree,
    file_changes: &FileChangeSummary,
) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head().await?;

    // Start transaction
    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();

    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .context("workspace should have a working-copy commit")?;
    let wc_commit = repo.store().get_commit(wc_commit_id)?;

    // Rewrite the working copy commit with the description and snapshotted tree
    let commit_with_description = mut_repo
        .rewrite_commit(&wc_commit)
        .set_tree(tree.clone())
        .set_description(commit_message)
        .write()
        .await?;

    // Rebase descendants (handles the rewrite)
    mut_repo.rebase_descendants().await?;

    // Create a new empty working copy commit on top
    let new_wc_commit = mut_repo
        .new_commit(vec![commit_with_description.id().clone()], tree)
        .write()
        .await?;

    mut_repo.set_wc_commit(workspace.workspace_name().to_owned(), new_wc_commit.id().clone())?;

    // Keep Git HEAD and its index in sync with the new working-copy parent in
    // colocated repositories. `jj` does this when finishing every transaction;
    // without it, `git status` can report the just-committed changes again.
    if is_colocated_git_workspace(workspace, &repo) {
        git::reset_head(mut_repo, &new_wc_commit).await?;
    }

    let new_repo = tx.commit("auto-commit via jc").await?;

    // Finish the working copy with the new state
    let locked_wc = workspace.working_copy().start_mutation().await?;
    locked_wc.finish(new_repo.operation().id().clone()).await?;

    let author = commit_with_description.author();
    let commit_id = commit_with_description.id().hex();
    let short_id = &commit_id[..8.min(commit_id.len())];
    let title = format!(
        "{}{} {} {}",
        "Committed change ".white().dimmed(),
        short_id.blue().dimmed(),
        "by".white().dimmed(),
        format!("{} <{}>", author.name, author.email).white().dimmed()
    );

    // Print the box with title in top border
    print!("{}", format_box_with_title(&title, commit_message, 72));

    // Print file changes below the box (indented to align with box content)
    print_file_changes(file_changes);

    Ok(())
}

fn is_colocated_git_workspace(workspace: &Workspace, repo: &ReadonlyRepo) -> bool {
    let Ok(git_backend) = git::get_git_backend(repo.store()) else {
        return false;
    };
    let Some(git_workdir) = git_backend.git_workdir() else {
        return false;
    };

    if git_workdir == workspace.workspace_root() {
        return true;
    }

    match (git_workdir.canonicalize(), workspace.workspace_root().canonicalize()) {
        (Ok(git_workdir), Ok(workspace_root)) => git_workdir == workspace_root,
        _ => false,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::WARN.into()))
        .init();

    let args = Args::parse();
    set_backend(args.backend);
    debug!(?args, "Parsed arguments");

    // Resolve model: "auto" falls back to the backend's configured default
    let model = if args.model.eq_ignore_ascii_case("auto") {
        CONFIG.generator.default_model.clone()
    } else {
        args.model
    };

    // Determine workspace path
    let workspace_path = match args.path {
        Some(p) => p,
        None => current_dir().context("Failed to get current directory")?,
    };
    info!(?workspace_path, "Starting workspace discovery");

    // Find workspace
    let workspace = find_workspace(&workspace_path)?;
    info!(workspace_root = ?workspace.workspace_root(), "Found workspace");

    match args.command.unwrap_or_default() {
        Commands::Add => run_add(&workspace),
        Commands::Install => run_install(&workspace),
        Commands::Bookmark { from, to, prefix, dry_run } => {
            run_bookmark(&workspace, &model, from, &to, prefix, dry_run).await
        }
        Commands::Commit { language, force, dry_run, no_instructions, infer } => {
            run_commit(&workspace, &language, &model, force, dry_run, no_instructions, infer).await
        }
        Commands::Describe {
            revision,
            language,
            force,
            dry_run,
            no_instructions,
            infer,
        } => {
            run_describe(
                &workspace,
                &language,
                &model,
                &revision,
                force,
                dry_run,
                no_instructions,
                infer,
            )
            .await
        }
    }
}

/// Record a user prompt read from stdin for the current workspace.
fn run_add(workspace: &Workspace) -> Result<()> {
    let mut buf = Vec::new();
    stdin()
        .read_to_end(&mut buf)
        .context("Failed to read prompt from stdin")?;
    PromptStore::new().add(workspace.workspace_root(), &buf)?;
    Ok(())
}

/// Result of merging the hook into an existing settings file.
enum InstallOutcome {
    Installed,
    Updated,
    AlreadyExists,
}

/// Install a `UserPromptSubmit` hook that runs `jc add` for this workspace into
/// `<workspace>/.claude/settings.local.json`, so every prompt is recorded automatically.
fn run_install(workspace: &Workspace) -> Result<()> {
    let root = workspace.workspace_root();
    let binary = current_exe()
        .context("Failed to resolve the jc binary path")?
        .display()
        .to_string();
    let command = format!("{binary} add -p {}", root.display());

    let claude_dir = root.join(".claude");
    create_dir_all(&claude_dir).context("Failed to create .claude directory")?;
    let settings_path = claude_dir.join("settings.local.json");

    let settings = if settings_path.exists() {
        let content = read_to_string(&settings_path)
            .with_context(|| format!("Failed to read {}", settings_path.display()))?;
        from_str::<Value>(&content)
            .with_context(|| format!("{} is not valid JSON", settings_path.display()))?
    } else {
        json!({})
    };

    let (settings, outcome) = upsert_user_prompt_hook(settings, &binary, &command);

    write(&settings_path, to_string_pretty(&settings)?)
        .with_context(|| format!("Failed to write {}", settings_path.display()))?;

    let path = settings_path.display();
    match outcome {
        InstallOutcome::Installed => println!("Hook installed to {path}"),
        InstallOutcome::Updated => println!("Hook updated in {path}"),
        InstallOutcome::AlreadyExists => println!("Hook already present in {path}"),
    }
    Ok(())
}

/// Insert or update the `UserPromptSubmit` hook for `command` in the settings JSON.
///
/// An existing hook whose command starts with `binary_path` is updated in place (so re-running
/// install for a moved workspace refreshes the `-p` path instead of adding a duplicate). Malformed
/// `hooks`/`UserPromptSubmit` values are replaced with the correct shape.
fn upsert_user_prompt_hook(
    mut settings: Value,
    binary_path: &str,
    command: &str,
) -> (Value, InstallOutcome) {
    if !settings.is_object() {
        settings = json!({});
    }
    let root = settings.as_object_mut().unwrap();

    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let events = hooks.as_object_mut().unwrap();

    let ups = events.entry("UserPromptSubmit").or_insert_with(|| json!([]));
    if !ups.is_array() {
        *ups = json!([]);
    }
    let ups = ups.as_array_mut().unwrap();

    let existing = ups.iter().position(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|inner| inner.first())
            .and_then(|h| h.get("command"))
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.starts_with(binary_path))
    });

    let outcome = if let Some(i) = existing {
        let current = ups[i]["hooks"][0]["command"].as_str().unwrap_or_default().to_string();
        if current == command {
            InstallOutcome::AlreadyExists
        } else {
            ups[i]["hooks"][0]["command"] = json!(command);
            InstallOutcome::Updated
        }
    } else {
        ups.push(json!({ "hooks": [ { "type": "command", "command": command } ] }));
        InstallOutcome::Installed
    };

    (settings, outcome)
}

async fn run_bookmark(
    workspace: &Workspace,
    model: &str,
    from: Option<String>,
    to: &str,
    prefix: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head().await?;
    debug!("Loaded repository at head");

    let from_rev = match from {
        Some(rev) => rev,
        None => find_default_base(&repo)?,
    };

    // Resolve target revision, skipping empty @ if needed
    let effective_to = resolve_bookmark_target(&repo, workspace, to).await?;
    let target_commit = resolve_single_commit(&repo, workspace, &effective_to).await?;

    // Check if any commit in the range already has a bookmark - if so, move it
    if let Some(existing_name) =
        find_existing_bookmark_in_range(&repo, workspace, &from_rev, &effective_to).await?
    {
        let final_name = match &prefix {
            Some(p) if !existing_name.starts_with(&format!("{p}/")) => {
                format!("{p}/{existing_name}")
            }
            _ => existing_name.clone(),
        };

        if dry_run {
            println!("{final_name}");
            return Ok(());
        }

        let was_moved = set_bookmark(&repo, &final_name, &target_commit).await?;
        let action = if was_moved { "Moved bookmark" } else { "Created bookmark" };
        println!(
            "{} {} {} {}",
            action.green(),
            final_name.blue().bold(),
            "at".white().dimmed(),
            target_commit.id().hex()[..8].to_string().yellow()
        );
        return Ok(());
    }

    // No existing bookmark - generate a new name
    info!(from = %from_rev, to = %effective_to, "Resolving revset range");

    let commit_summaries = get_commit_summaries(&repo, workspace, &from_rev, &effective_to).await?;
    if commit_summaries.is_empty() {
        bail!("No commits found between {from_rev} and {effective_to}");
    }
    debug!(commit_count = commit_summaries.lines().count(), "Found commits");

    info!(model = %model, backend = %config::backend(), "Generating bookmark name");
    let generator = BookmarkGenerator::new(model);
    let Some(bookmark_name) = generator.generate(&commit_summaries).await else {
        bail!("Failed to generate bookmark name")
    };

    let final_name = match &prefix {
        Some(p) => format!("{p}/{bookmark_name}"),
        None => bookmark_name,
    };

    if dry_run {
        println!("{final_name}");
        return Ok(());
    }

    set_bookmark(&repo, &final_name, &target_commit).await?;
    println!(
        "{} {} {} {}",
        "Created bookmark".green(),
        final_name.blue().bold(),
        "at".white().dimmed(),
        target_commit.id().hex()[..8].to_string().yellow()
    );

    Ok(())
}

/// Find an existing local bookmark anywhere in the given revset range
async fn find_existing_bookmark_in_range(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    from: &str,
    to: &str,
) -> Result<Option<String>> {
    let revset_str = format!("{from}..{to}");
    let commit_ids: HashSet<_> = evaluate_revset(repo, workspace, &revset_str)
        .await?
        .into_iter()
        .collect();

    for (name, target) in repo.view().local_bookmarks() {
        if target.added_ids().any(|id| commit_ids.contains(id)) {
            return Ok(Some(name.as_str().to_string()));
        }
    }
    Ok(None)
}

/// Resolve bookmark target, using @- if @ is empty (idiomatic jj behavior)
async fn resolve_bookmark_target(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    to: &str,
) -> Result<String> {
    if to != "@" {
        return Ok(to.to_string());
    }

    let commit = resolve_single_commit(repo, workspace, "@").await?;

    // Check if @ is empty (no description and tree matches parent)
    let is_empty = commit.description().is_empty() && {
        if let Some(parent_id) = commit.parent_ids().first() {
            let parent = repo.store().get_commit(parent_id)?;
            commit.tree_ids() == parent.tree_ids()
        } else {
            false
        }
    };

    if is_empty {
        debug!("@ is empty, using @- as bookmark target");
        Ok("@-".to_string())
    } else {
        Ok("@".to_string())
    }
}

const DEFAULT_BASE_BRANCHES: &[&str] = &["develop", "main", "master", "trunk"];

fn find_default_base(repo: &Arc<ReadonlyRepo>) -> Result<String> {
    let view = repo.view();
    let remote_name = RemoteName::new("origin");

    for &name in DEFAULT_BASE_BRANCHES {
        let ref_name = RefName::new(name);

        let remote_symbol = ref_name.to_remote_symbol(remote_name);
        let remote_ref = view.get_remote_bookmark(remote_symbol);
        if remote_ref.target.is_present() {
            debug!("Using {name}@origin as base");
            return Ok(format!("{name}@origin"));
        }

        let local_ref = view.get_local_bookmark(ref_name);
        if local_ref.is_present() {
            debug!("Using {name} as base");
            return Ok(name.to_string());
        }
    }

    bail!(
        "Could not find a default base branch ({}). Please specify --from explicitly.",
        DEFAULT_BASE_BRANCHES.join(", ")
    )
}

/// Evaluate a revset expression and return the matching commit IDs.
async fn evaluate_revset(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    revset_str: &str,
) -> Result<Vec<CommitId>> {
    let settings = repo.settings();
    let extensions = Arc::new(RevsetExtensions::new());
    let aliases_map: RevsetAliasesMap = AliasesMap::new();
    let path_converter = Fs {
        cwd: workspace.workspace_root().to_path_buf(),
        base: workspace.workspace_root().to_path_buf(),
    };
    let workspace_ctx = RevsetWorkspaceContext {
        path_converter: &path_converter,
        workspace_name: workspace.workspace_name(),
    };
    let fileset_aliases_map = AliasesMap::new();
    let context = RevsetParseContext {
        aliases_map: &aliases_map,
        local_variables: HashMap::new(),
        user_email: settings.user_email(),
        date_pattern_context: DatePatternContext::Local(Local::now()),
        default_ignored_remote: None,
        fileset_aliases_map: &fileset_aliases_map,
        extensions: &extensions,
        workspace: Some(workspace_ctx),
    };

    let mut diagnostics = RevsetDiagnostics::new();
    let expression = parse(&mut diagnostics, revset_str, &context)?;
    let id_prefix_context =
        IdPrefixContext::new(extensions.clone()).disambiguate_within(RevsetExpression::all());
    let symbol_resolver = SymbolResolver::new(repo.as_ref(), extensions.symbol_resolvers())
        .with_id_prefix_context(&id_prefix_context);
    let resolved = expression.resolve_user_expression(repo.as_ref(), &symbol_resolver)?;
    let revset = resolved.evaluate(repo.as_ref())?;
    revset.stream().try_collect::<Vec<_>>().await.map_err(Into::into)
}

async fn get_commit_summaries(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    from: &str,
    to: &str,
) -> Result<String> {
    let revset_str = format!("{from}..{to}");
    let commit_ids = evaluate_revset(repo, workspace, &revset_str).await?;

    let mut summaries = Vec::new();
    for commit_id in commit_ids {
        let commit = repo.store().get_commit(&commit_id)?;
        let desc = commit.description().trim();
        if !desc.is_empty() {
            let title = desc.lines().next().unwrap_or("");
            let title = if title.chars().count() > 120 {
                format!(
                    "{}...",
                    &title[..title.char_indices().nth(120).map_or(title.len(), |(i, _)| i)]
                )
            } else {
                title.to_string()
            };
            summaries.push(format!("- {title}"));
        }
    }

    Ok(summaries.join("\n"))
}

async fn resolve_single_commit(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    rev: &str,
) -> Result<Commit> {
    match evaluate_revset(repo, workspace, rev).await?.as_slice() {
        [id] => repo.store().get_commit(id).map_err(Into::into),
        [] => bail!("Revset resolved to no commits"),
        _ => bail!("Revset '{rev}' resolved to multiple commits, expected single commit"),
    }
}

/// Set bookmark to point to commit. Returns true if bookmark already existed (moved), false if
/// created. Also exports the bookmark to git refs.
async fn set_bookmark(repo: &Arc<ReadonlyRepo>, name: &str, commit: &Commit) -> Result<bool> {
    let ref_name = RefName::new(name);
    let existed = repo.view().get_local_bookmark(ref_name).is_present();

    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();

    // Import git refs first to sync state (prevents compare-and-swap failures)
    let import_options = GitImportOptions {
        abandon_unreachable_commits: true,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    if let Err(e) = import_refs(mut_repo, &import_options).await {
        warn!(error = %e, "Failed to import git refs");
    }

    let target = RefTarget::normal(commit.id().clone());
    mut_repo.set_local_bookmark_target(ref_name, target);

    // Export to git refs - now should succeed since we imported first
    match export_refs(mut_repo) {
        Ok(stats) => {
            if !stats.failed_bookmarks.is_empty() {
                for (ref_name, reason) in &stats.failed_bookmarks {
                    warn!(bookmark = %ref_name, reason = ?reason, "Failed to export bookmark");
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to export refs to git");
        }
    }

    let action = if existed { "move" } else { "create" };
    tx.commit(format!("{action} bookmark '{name}' via jc")).await?;
    Ok(existed)
}

/// Snapshot the working copy and reload the repo to reflect the latest state.
async fn snapshot_working_copy(
    workspace: &Workspace,
    repo: &Arc<ReadonlyRepo>,
) -> Result<Arc<ReadonlyRepo>> {
    debug!("Starting working copy mutation");
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .context("workspace should have a working-copy commit")?;
    let wc_commit = repo.store().get_commit(wc_commit_id)?;

    let mut locked_wc = workspace.working_copy().start_mutation().await?;
    let base_ignores = load_base_ignores(workspace.workspace_root())?;
    let snapshot_options = SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: &EverythingMatcher,
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size: 1024 * 1024 * 100,
    };
    let (new_tree, _stats) = locked_wc.snapshot(&snapshot_options).await?;
    debug!("Snapshot complete");

    let final_repo = if new_tree.tree_ids_and_labels() != wc_commit.tree().tree_ids_and_labels() {
        debug!("Working copy tree changed; rewriting working-copy commit");
        let mut tx = repo.start_transaction();
        tx.set_is_snapshot(true);
        let mut_repo = tx.repo_mut();
        let new_commit = mut_repo.rewrite_commit(&wc_commit).set_tree(new_tree).write().await?;
        mut_repo.set_wc_commit(workspace.workspace_name().to_owned(), new_commit.id().clone())?;
        mut_repo.rebase_descendants().await?;
        tx.commit("snapshot working copy").await?
    } else {
        debug!("Working copy tree unchanged");
        repo.clone()
    };

    locked_wc.finish(final_repo.op_id().clone()).await?;
    Ok(final_repo)
}

/// Committer timestamp of a commit's first parent, used as the cutoff for which recorded prompts
/// belong to this change. Returns `None` for root commits (no parent), meaning "include all".
fn parent_commit_time(repo: &ReadonlyRepo, commit: &Commit) -> Option<DateTime<Utc>> {
    let parent_id = commit.parent_ids().first()?;
    let parent = repo.store().get_commit(parent_id).ok()?;
    DateTime::from_timestamp_millis(parent.committer().timestamp.timestamp.0)
}

/// Load recorded user prompts that belong to this change, or an empty list when disabled or
/// unavailable.
fn load_prompts(
    workspace: &Workspace,
    repo: &ReadonlyRepo,
    commit: &Commit,
    no_instructions: bool,
) -> Vec<String> {
    if no_instructions {
        return Vec::new();
    }
    let cutoff = parent_commit_time(repo, commit);
    match PromptStore::new().prompts_since(workspace.workspace_root(), cutoff) {
        Ok(prompts) => prompts,
        Err(e) => {
            warn!(error = %e, "Failed to read recorded prompts; skipping AI Instructions");
            Vec::new()
        }
    }
}

/// Get parent tree for a commit.
fn get_parent_tree(repo: &ReadonlyRepo, commit: &Commit) -> MergedTree {
    if let Some(parent_id) = commit.parent_ids().first()
        && let Ok(parent_commit) = repo.store().get_commit(parent_id)
    {
        return parent_commit.tree();
    }
    MergedTree::resolved(repo.store().clone(), repo.store().empty_tree_id().clone())
}

/// Print a warning listing the files that were collapsed out of the LLM diff.
fn report_collapsed(collapsed_paths: &[String]) {
    if collapsed_paths.is_empty() {
        return;
    }
    let mark = "!".yellow().dimmed();
    eprintln!("  {mark} {} files collapsed from LLM diff:", collapsed_paths.len());
    for path in collapsed_paths {
        eprintln!("  {mark}   {path}");
    }
}

/// Generate a diff between a commit and its parent, with size validation.
/// Returns `Some((diff, collapsed_paths))` or `None` if unchanged.
async fn generate_diff(
    repo: &ReadonlyRepo,
    commit: &Commit,
    workspace_root: &Path,
) -> Result<Option<(String, Vec<String>)>> {
    let current_tree = commit.tree();
    let parent_tree = get_parent_tree(repo, commit);

    if current_tree.tree_ids() == parent_tree.tree_ids() {
        return Ok(None);
    }

    let collapse_matcher = build_collapse_matcher(&CONFIG.diff.collapse_patterns);
    let gitattr_matcher = load_gitattributes(workspace_root);
    let TreeDiffResult { mut diff, collapsed_paths } = get_tree_diff(
        repo,
        &parent_tree,
        &current_tree,
        collapse_matcher.as_ref(),
        gitattr_matcher.as_ref(),
        CONFIG.diff.max_diff_lines,
        CONFIG.diff.max_diff_bytes,
    )
    .await?;

    if diff.trim().is_empty() {
        return Ok(None);
    }

    // Prepend merge notice for merge commits
    let parent_count = commit.parent_ids().len();
    if parent_count > 1 {
        diff =
            format!("Merge commit ({parent_count} parents) - diff against first parent:\n\n{diff}");
    }

    let diff_lines = diff.lines().count();
    let diff_bytes = diff.len();
    let max_lines = CONFIG.diff.max_total_diff_lines;
    let max_bytes = CONFIG.diff.max_total_diff_bytes;

    if diff_lines > max_lines || diff_bytes > max_bytes {
        bail!(
            "Diff too large: {diff_lines} lines / {diff_bytes} bytes (limits: {max_lines} lines / {max_bytes} bytes). \
            Consider splitting changes or using `jj describe` to set the message manually."
        );
    }

    Ok(Some((diff, collapsed_paths)))
}

/// Generate a commit message from a diff using the configured LLM backend.
async fn generate_message(
    diff: &str,
    language: &str,
    model: &str,
    instruction_prompts: &[String],
) -> Result<(String, Option<Vec<usize>>)> {
    info!(language = %language, model = %model, backend = %config::backend(), "Generating commit message");
    let generator = CommitMessageGenerator::new(language, model);
    match generator.generate(diff, instruction_prompts).await {
        Some(result) => Ok(result),
        None => bail!("Failed to generate commit message"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_commit(
    workspace: &Workspace,
    language: &str,
    model: &str,
    force: bool,
    dry_run: bool,
    no_instructions: bool,
    infer: bool,
) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head().await?;
    debug!("Loaded repository at head");

    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .context("workspace should have a working-copy commit")?;
    let wc_commit = repo.store().get_commit(wc_commit_id)?;
    debug!(wc_commit_id = %wc_commit_id.hex(), "Working copy commit");

    if !wc_commit.description().is_empty() && !force {
        warn!(description = %wc_commit.description(), "Working copy already has description, skipping (use --force to overwrite)");
        return Ok(());
    }

    let repo = snapshot_working_copy(workspace, &repo).await?;
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .context("workspace should have a working-copy commit")?;
    let wc_commit = repo.store().get_commit(wc_commit_id)?;

    let Some((diff, collapsed_paths)) =
        generate_diff(&repo, &wc_commit, workspace.workspace_root()).await?
    else {
        println!("No changes detected, nothing to commit");
        return Ok(());
    };

    report_collapsed(&collapsed_paths);

    let prompts = load_prompts(workspace, &repo, &wc_commit, no_instructions);
    let instruction_prompts: &[String] = if infer { &prompts } else { &[] };
    let (commit_message, selection) =
        generate_message(&diff, language, model, instruction_prompts).await?;
    let quoted = select_prompts(prompts, selection);
    let commit_message = PromptStore::new().append_instructions(&commit_message, &quoted);
    debug!(commit_message = %commit_message, "Generated commit message");

    if dry_run {
        println!("{commit_message}");
        return Ok(());
    }

    let current_tree = wc_commit.tree();
    let file_changes =
        get_file_change_summary(&get_parent_tree(&repo, &wc_commit), &current_tree).await;

    info!("Creating commit");
    create_commit(workspace, &commit_message, current_tree, &file_changes).await?;
    info!("Commit created successfully");

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_describe(
    workspace: &Workspace,
    language: &str,
    model: &str,
    revision: &str,
    force: bool,
    dry_run: bool,
    no_instructions: bool,
    infer: bool,
) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head().await?;
    debug!("Loaded repository at head");

    let repo = if revision == "@" { snapshot_working_copy(workspace, &repo).await? } else { repo };

    let commit = resolve_single_commit(&repo, workspace, revision).await?;
    let commit_id = commit.id().hex();
    let short_id = &commit_id[..8.min(commit_id.len())];
    debug!(revision = %revision, commit_id = %short_id, "Resolved target revision");

    if !commit.description().trim().is_empty() && !force {
        bail!(
            "Revision {short_id} already has a description; refusing to overwrite. \
             Use --force to overwrite, or `jj describe` to edit it manually."
        );
    }

    let Some((diff, collapsed_paths)) =
        generate_diff(&repo, &commit, workspace.workspace_root()).await?
    else {
        println!("No changes in revision {short_id}, nothing to describe");
        return Ok(());
    };

    report_collapsed(&collapsed_paths);

    let prompts = load_prompts(workspace, &repo, &commit, no_instructions);
    let instruction_prompts: &[String] = if infer { &prompts } else { &[] };
    let (description, selection) =
        generate_message(&diff, language, model, instruction_prompts).await?;
    let quoted = select_prompts(prompts, selection);
    let description = PromptStore::new().append_instructions(&description, &quoted);
    debug!(description = %description, "Generated description");

    if dry_run {
        println!("{description}");
        return Ok(());
    }

    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();
    mut_repo
        .rewrite_commit(&commit)
        .set_description(&description)
        .write()
        .await?;
    mut_repo.rebase_descendants().await?;
    tx.commit(format!("describe revision {short_id} via jc")).await?;

    let file_changes =
        get_file_change_summary(&get_parent_tree(&repo, &commit), &commit.tree()).await;

    let title = format!(
        "{}{} {}",
        "Described change ".white().dimmed(),
        short_id.blue().dimmed(),
        if revision == "@" {
            String::new()
        } else {
            format!("({revision})").white().dimmed().to_string()
        },
    );

    print!("{}", format_box_with_title(&title, &description, 72));
    print_file_changes(&file_changes);

    Ok(())
}

/// Formats text content inside a box with a title in the top border (with colors).
fn format_box_with_title(title: &str, content: &str, width: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let title_width = strip_ansi_codes(title).width();

    let mut result = String::new();

    // Top border with title: ╭─Title───...───╮
    let remaining = (width + 2).saturating_sub(title_width).saturating_sub(1); // -1 for the leading ─
    let border = "─".repeat(remaining);
    let _ = writeln!(
        result,
        "{}{title}{}{}",
        "╭─".white().dimmed(),
        border.white().dimmed(),
        "╮".white().dimmed()
    );

    for line in &lines {
        let line_width = line.width();
        if line_width <= width {
            let padding = width - line_width;
            let _ = writeln!(
                result,
                "{} {line}{} {}",
                "│".white().dimmed(),
                " ".repeat(padding),
                "│".white().dimmed()
            );
        } else {
            let _ = writeln!(result, "{} {line} {}", "│".white().dimmed(), "│".white().dimmed());
        }
    }
    let _ = writeln!(
        result,
        "{}{}{}",
        "╰".white().dimmed(),
        "─".repeat(width + 2).white().dimmed(),
        "╯".white().dimmed()
    );
    result
}

/// Prints file changes with colored status indicators.
fn print_file_changes(changes: &FileChangeSummary) {
    for file in &changes.added {
        println!("  {} {}", "A".green().dimmed(), file.dimmed());
    }
    for file in &changes.deleted {
        println!("  {} {}", "D".red().dimmed(), file.dimmed());
    }
    for file in &changes.modified {
        println!("  {} {}", "M".yellow().dimmed(), file.dimmed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn stripped_width(line: &str) -> usize {
        strip_ansi_codes(line).width()
    }

    fn long_help(mut command: clap::Command) -> String {
        let mut buffer = Vec::new();
        command.write_long_help(&mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    fn subcommand_long_help(name: &str) -> String {
        let mut command = Args::command();
        let subcommand = command.find_subcommand_mut(name).unwrap();
        let mut buffer = Vec::new();
        subcommand.write_long_help(&mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    fn normalize_whitespace(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn top_level_help_has_llm_quick_reference() {
        let help = long_help(Args::command());
        let normalized = normalize_whitespace(&help);

        assert!(help.contains("LLM QUICK REFERENCE:"));
        assert!(normalized.contains("Default action: `jc` is equivalent to `jc commit`."));
        assert!(normalized.contains("Prompt capture: `jc add` records stdin"));
        assert!(help.contains("jc bookmark --prefix feature --dry-run"));
    }

    #[test]
    fn commit_help_documents_default_command_and_dry_run() {
        let help = subcommand_long_help("commit");
        let normalized = normalize_whitespace(&help);

        assert!(
            normalized.contains("This is the default command: `jc` is the same as `jc commit`.")
        );
        assert!(normalized.contains("`--dry-run` prints the generated commit message"));
        assert!(help.contains("jc c --force --no-instructions"));
    }

    #[test]
    fn bookmark_help_matches_default_base_resolution() {
        let help = subcommand_long_help("bookmark");
        let normalized = normalize_whitespace(&help);

        assert!(normalized.contains("tries develop, main, master, then trunk"));
        assert!(normalized.contains("preferring `<name>@origin` over local `<name>`"));
        assert!(normalized.contains("if `@` is empty, jc uses `@-`"));
    }

    #[test]
    fn test_format_box_with_title_ascii() {
        let result = format_box_with_title("Title", "Hello", 72);
        let plain = strip_ansi_codes(&result);
        assert!(plain.contains("╭─Title"));
        assert!(plain.contains("│ Hello"));
        // All lines should have same width (72 + 4 for borders and spaces)
        let line_widths: Vec<usize> = result.lines().map(stripped_width).collect();
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_japanese() {
        let result = format_box_with_title("コミット", "こんにちは", 72);
        let line_widths: Vec<usize> = result.lines().map(stripped_width).collect();
        // All lines should have same display width
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_mixed() {
        let result = format_box_with_title("Commit by 太郎", "Hello こんにちは World", 72);
        let line_widths: Vec<usize> = result.lines().map(stripped_width).collect();
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_multiline() {
        let content = "タイトル\n\nこれは日本語のテストです";
        let result = format_box_with_title("Committed change a05fdfa2", content, 72);
        let line_widths: Vec<usize> = result.lines().map(stripped_width).collect();
        // All lines should have same display width
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_fixed_width() {
        let result = format_box_with_title("Title", "Short", 72);
        let first_line = result.lines().next().unwrap_or("");
        // width=72, plus 4 for borders and spaces = 76
        assert_eq!(stripped_width(first_line), 76);
    }

    #[test]
    fn test_format_box_with_title_empty_content() {
        let result = format_box_with_title("Title", "", 72);
        let first_line = result.lines().next().unwrap_or("");
        assert!(strip_ansi_codes(first_line).contains("╭─Title"));
        assert!(strip_ansi_codes(&result).contains("╰"));
    }

    #[test]
    fn test_install_into_empty_settings() {
        let (settings, outcome) =
            upsert_user_prompt_hook(json!({}), "/bin/jc", "/bin/jc add -p /repo");
        assert!(matches!(outcome, InstallOutcome::Installed));
        assert_eq!(
            settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            json!("/bin/jc add -p /repo")
        );
        assert_eq!(settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["type"], json!("command"));
    }

    #[test]
    fn test_install_is_idempotent() {
        let (settings, _) = upsert_user_prompt_hook(json!({}), "/bin/jc", "/bin/jc add -p /repo");
        let (settings, outcome) =
            upsert_user_prompt_hook(settings, "/bin/jc", "/bin/jc add -p /repo");
        assert!(matches!(outcome, InstallOutcome::AlreadyExists));
        assert_eq!(settings["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_install_updates_changed_path() {
        let (settings, _) = upsert_user_prompt_hook(json!({}), "/bin/jc", "/bin/jc add -p /old");
        let (settings, outcome) =
            upsert_user_prompt_hook(settings, "/bin/jc", "/bin/jc add -p /new");
        assert!(matches!(outcome, InstallOutcome::Updated));
        let ups = settings["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["hooks"][0]["command"], json!("/bin/jc add -p /new"));
    }

    #[test]
    fn test_install_preserves_other_hooks() {
        let existing = json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [ { "type": "command", "command": "/other/tool" } ] }
                ]
            },
            "model": "opus"
        });
        let (settings, outcome) =
            upsert_user_prompt_hook(existing, "/bin/jc", "/bin/jc add -p /repo");
        assert!(matches!(outcome, InstallOutcome::Installed));
        let ups = settings["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 2);
        assert_eq!(settings["model"], json!("opus"));
    }

    #[test]
    fn test_format_box_with_title_long_title() {
        let result = format_box_with_title(
            "This is an extremely long title that exceeds the box width",
            "Short",
            20,
        );
        // Should not panic; may have no border padding but still produce output
        let plain = strip_ansi_codes(&result);
        assert!(plain.contains("╭─"));
        assert!(plain.contains("╮"));
    }
}
