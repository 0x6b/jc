mod bookmark_generator;
mod commit_message_generator;
mod config;
mod diff;
mod llm_client;
mod text_formatter;

use std::{
    collections::{HashMap, HashSet},
    env::{current_dir, var},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bookmark_generator::BookmarkGenerator;
use chrono::Local;
use clap::{Parser, Subcommand};
use colored::Colorize;
use commit_message_generator::CommitMessageGenerator;
use config::{Backend, CONFIG};
use console::strip_ansi_codes;
use diff::{FileChangeSummary, build_collapse_matcher, get_file_change_summary, get_tree_diff};
use dirs::{config_dir, home_dir};
use gethostname::gethostname;
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    config::{ConfigLayer, ConfigResolutionContext, ConfigSource, StackedConfig, resolve},
    dsl_util::AliasesMap,
    git::{GitImportOptions, export_refs, import_refs},
    gitignore::GitIgnoreFile,
    merged_tree::MergedTree,
    object_id::ObjectId,
    op_store::RefTarget,
    ref_name::{RefName, RemoteName},
    repo::{ReadonlyRepo, Repo, StoreFactories},
    repo_path::RepoPathUiConverter::Fs,
    revset::{
        RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
        RevsetWorkspaceContext, SymbolResolver, parse,
    },
    settings::UserSettings,
    time_util::DatePatternContext,
    working_copy::SnapshotOptions,
    workspace::{Workspace, default_working_copy_factories},
};
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use unicode_width::UnicodeWidthStr;

#[derive(Parser, Debug)]
#[command(about, version)]
struct Args {
    /// Path to the workspace (defaults to current directory)
    #[arg(short, long, global = true)]
    path: Option<PathBuf>,

    /// LLM backend to use
    #[arg(short, long, default_value = "codex", env = "CCC_JJ_BACKEND", global = true)]
    backend: Backend,

    /// Model to use for AI generation (defaults to backend's default)
    #[arg(short, long, default_value = "auto", env = "CCC_JJ_MODEL", global = true)]
    model: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Generate a bookmark name for commits between the current revision and a base
    #[command(alias = "b")]
    Bookmark {
        /// Base revision to compare against (default: main@origin or main)
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
    #[command(alias = "c")]
    Commit {
        /// Language to use for commit messages
        #[arg(short, long, default_value = "English", env = "CCC_JJ_LANGUAGE")]
        language: String,
    },
    /// Generate and set a commit description using AI (without creating a new commit)
    #[command(alias = "d")]
    Describe {
        /// Revision to describe (default: @)
        #[arg(short, long, default_value = "@")]
        revision: String,

        /// Language to use for commit messages
        #[arg(short, long, default_value = "English", env = "CCC_JJ_LANGUAGE")]
        language: String,
    },
}

impl Default for Commands {
    fn default() -> Self {
        Commands::Commit { language: "English".to_string() }
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
        git_ignores = git_ignores.chain_with_file("", excludes_path).unwrap_or(git_ignores);
    }

    // Load workspace root .gitignore
    let workspace_gitignore = workspace_root.join(".gitignore");
    git_ignores = git_ignores
        .chain_with_file("", workspace_gitignore)
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
    let hostname = gethostname().to_str().map(|s| s.to_owned()).unwrap_or_default();
    let home_dir = home_dir();
    let context = ConfigResolutionContext {
        home_dir: home_dir.as_deref(),
        repo_path: Some(workspace_root),
        workspace_path: Some(workspace_root),
        command: None,
        hostname: hostname.as_str(),
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
    let repo = workspace.repo_loader().load_at_head()?;

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
        .write()?;

    // Rebase descendants (handles the rewrite)
    mut_repo.rebase_descendants()?;

    // Create a new empty working copy commit on top
    let new_wc_commit = mut_repo
        .new_commit(vec![commit_with_description.id().clone()], tree)
        .write()?;

    mut_repo.set_wc_commit(workspace.workspace_name().to_owned(), new_wc_commit.id().clone())?;

    let new_repo = tx.commit("auto-commit via jc")?;

    // Finish the working copy with the new state
    let locked_wc = workspace.working_copy().start_mutation()?;
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

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::WARN.into()))
        .init();

    let args = Args::parse();
    config::set_backend(args.backend);
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
        Commands::Bookmark { from, to, prefix, dry_run } => {
            run_bookmark(&workspace, &model, from, &to, prefix, dry_run).await
        }
        Commands::Commit { language } => run_commit(&workspace, &language, &model).await,
        Commands::Describe { revision, language } => {
            run_describe(&workspace, &language, &model, &revision).await
        }
    }
}

async fn run_bookmark(
    workspace: &Workspace,
    model: &str,
    from: Option<String>,
    to: &str,
    prefix: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head()?;
    debug!("Loaded repository at head");

    let from_rev = match from {
        Some(rev) => rev,
        None => find_default_base(&repo)?,
    };

    // Resolve target revision, skipping empty @ if needed
    let effective_to = resolve_bookmark_target(&repo, workspace, to)?;
    let target_commit = resolve_single_commit(&repo, workspace, &effective_to)?;

    // Check if any commit in the range already has a bookmark - if so, move it
    if let Some(existing_name) =
        find_existing_bookmark_in_range(&repo, workspace, &from_rev, &effective_to)?
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

        let was_moved = set_bookmark(&repo, &final_name, &target_commit)?;
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

    let commit_summaries = get_commit_summaries(&repo, workspace, &from_rev, &effective_to)?;
    if commit_summaries.is_empty() {
        bail!("No commits found between {from_rev} and {effective_to}");
    }
    debug!(commit_count = commit_summaries.lines().count(), "Found commits");

    info!(model = %model, backend = %config::backend(), "Generating bookmark name");
    let generator = BookmarkGenerator::new(model);
    let bookmark_name = match generator.generate(&commit_summaries).await {
        Some(name) => name,
        None => bail!("Failed to generate bookmark name"),
    };

    let final_name = match &prefix {
        Some(p) => format!("{p}/{bookmark_name}"),
        None => bookmark_name,
    };

    if dry_run {
        println!("{final_name}");
        return Ok(());
    }

    set_bookmark(&repo, &final_name, &target_commit)?;
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
fn find_existing_bookmark_in_range(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    from: &str,
    to: &str,
) -> Result<Option<String>> {
    let revset_str = format!("{from}..{to}");
    let commit_ids: HashSet<_> =
        evaluate_revset(repo, workspace, &revset_str)?.into_iter().collect();

    for (name, target) in repo.view().local_bookmarks() {
        if target.added_ids().any(|id| commit_ids.contains(id)) {
            return Ok(Some(name.as_str().to_string()));
        }
    }
    Ok(None)
}

/// Resolve bookmark target, using @- if @ is empty (idiomatic jj behavior)
fn resolve_bookmark_target(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    to: &str,
) -> Result<String> {
    if to != "@" {
        return Ok(to.to_string());
    }

    let commit = resolve_single_commit(repo, workspace, "@")?;

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

fn find_default_base(repo: &Arc<ReadonlyRepo>) -> Result<String> {
    let view = repo.view();
    let remote_name = RemoteName::new("origin");
    let main_ref = RefName::new("main");

    let remote_symbol = main_ref.to_remote_symbol(remote_name);
    let remote_ref = view.get_remote_bookmark(remote_symbol);
    if remote_ref.target.is_present() {
        debug!("Using main@origin as base");
        return Ok("main@origin".to_string());
    }

    let local_ref = view.get_local_bookmark(main_ref);
    if local_ref.is_present() {
        debug!("Using main as base");
        return Ok("main".to_string());
    }

    bail!("Could not find main@origin or main bookmark. Please specify --from explicitly.")
}

/// Evaluate a revset expression and return the matching commit IDs.
fn evaluate_revset(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    revset_str: &str,
) -> Result<Vec<CommitId>> {
    let settings = repo.settings();
    let extensions = RevsetExtensions::new();
    let aliases_map: RevsetAliasesMap = AliasesMap::new();
    let path_converter = Fs {
        cwd: workspace.workspace_root().to_path_buf(),
        base: workspace.workspace_root().to_path_buf(),
    };
    let workspace_ctx = RevsetWorkspaceContext {
        path_converter: &path_converter,
        workspace_name: workspace.workspace_name(),
    };
    let context = RevsetParseContext {
        aliases_map: &aliases_map,
        local_variables: HashMap::new(),
        user_email: settings.user_email(),
        date_pattern_context: DatePatternContext::Local(Local::now()),
        default_ignored_remote: None,
        use_glob_by_default: false,
        extensions: &extensions,
        workspace: Some(workspace_ctx),
    };

    let mut diagnostics = RevsetDiagnostics::new();
    let expression = parse(&mut diagnostics, revset_str, &context)?;
    let symbol_resolver = SymbolResolver::new(repo.as_ref(), extensions.symbol_resolvers());
    let resolved = expression.resolve_user_expression(repo.as_ref(), &symbol_resolver)?;
    let revset = resolved.evaluate(repo.as_ref())?;
    revset.iter().collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn get_commit_summaries(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    from: &str,
    to: &str,
) -> Result<String> {
    let revset_str = format!("{from}..{to}");
    let commit_ids = evaluate_revset(repo, workspace, &revset_str)?;

    let mut summaries = Vec::new();
    for commit_id in commit_ids {
        let commit = repo.store().get_commit(&commit_id)?;
        let desc = commit.description().trim();
        if !desc.is_empty() {
            summaries.push(format!("- {}", desc.lines().next().unwrap_or("")));
        }
    }

    Ok(summaries.join("\n"))
}

fn resolve_single_commit(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    rev: &str,
) -> Result<Commit> {
    match evaluate_revset(repo, workspace, rev)?.as_slice() {
        [id] => repo.store().get_commit(id).map_err(Into::into),
        [] => bail!("Revset resolved to no commits"),
        _ => bail!("Revset '{rev}' resolved to multiple commits, expected single commit"),
    }
}

/// Set bookmark to point to commit. Returns true if bookmark already existed (moved), false if
/// created. Also exports the bookmark to git refs.
fn set_bookmark(repo: &Arc<ReadonlyRepo>, name: &str, commit: &Commit) -> Result<bool> {
    let ref_name = RefName::new(name);
    let existed = repo.view().get_local_bookmark(ref_name).is_present();

    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();

    // Import git refs first to sync state (prevents compare-and-swap failures)
    let import_options = GitImportOptions {
        auto_local_bookmark: false,
        abandon_unreachable_commits: true,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    if let Err(e) = import_refs(mut_repo, &import_options) {
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
    tx.commit(format!("{action} bookmark '{name}' via jc"))?;
    Ok(existed)
}

/// Snapshot the working copy and reload the repo to reflect the latest state.
async fn snapshot_working_copy(
    workspace: &Workspace,
    repo: &Arc<ReadonlyRepo>,
) -> Result<Arc<ReadonlyRepo>> {
    debug!("Starting working copy mutation");
    let mut locked_wc = workspace.working_copy().start_mutation()?;
    let base_ignores = load_base_ignores(workspace.workspace_root())?;
    let snapshot_options = SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: &jj_lib::matchers::EverythingMatcher,
        force_tracking_matcher: &jj_lib::matchers::NothingMatcher,
        max_new_file_size: 1024 * 1024 * 100,
    };
    let (_tree, _stats) = locked_wc.snapshot(&snapshot_options).await?;
    debug!("Snapshot complete");
    locked_wc.finish(repo.operation().id().clone()).await?;
    workspace.repo_loader().load_at_head().map_err(Into::into)
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

/// Generate a diff between a commit and its parent, with size validation.
async fn generate_diff(repo: &ReadonlyRepo, commit: &Commit) -> Result<Option<String>> {
    let current_tree = commit.tree();
    let parent_tree = get_parent_tree(repo, commit);

    if current_tree.tree_ids() == parent_tree.tree_ids() {
        return Ok(None);
    }

    let collapse_matcher = build_collapse_matcher(&CONFIG.diff.collapse_patterns);
    let diff = get_tree_diff(
        repo,
        &parent_tree,
        &current_tree,
        collapse_matcher.as_ref(),
        CONFIG.diff.max_diff_lines,
        CONFIG.diff.max_diff_bytes,
    )
    .await?;

    if diff.trim().is_empty() {
        return Ok(None);
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

    Ok(Some(diff))
}

/// Generate a commit message from a diff using the configured LLM backend.
async fn generate_message(diff: &str, language: &str, model: &str) -> Result<String> {
    info!(language = %language, model = %model, backend = %config::backend(), "Generating commit message");
    let generator = CommitMessageGenerator::new(language, model);
    match generator.generate(diff).await {
        Some(msg) => Ok(msg),
        None => bail!("Failed to generate commit message"),
    }
}

async fn run_commit(workspace: &Workspace, language: &str, model: &str) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head()?;
    debug!("Loaded repository at head");

    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .context("workspace should have a working-copy commit")?;
    let wc_commit = repo.store().get_commit(wc_commit_id)?;
    debug!(wc_commit_id = %wc_commit_id.hex(), "Working copy commit");

    if !wc_commit.description().is_empty() {
        warn!(description = %wc_commit.description(), "Working copy already has description, skipping");
        return Ok(());
    }

    let repo = snapshot_working_copy(workspace, &repo).await?;
    let wc_commit = repo.store().get_commit(wc_commit_id)?;

    let diff = match generate_diff(&repo, &wc_commit).await? {
        Some(diff) => diff,
        None => {
            println!("No changes detected, nothing to commit");
            return Ok(());
        }
    };

    let commit_message = generate_message(&diff, language, model).await?;
    debug!(commit_message = %commit_message, "Generated commit message");

    let current_tree = wc_commit.tree();
    let file_changes =
        get_file_change_summary(&get_parent_tree(&repo, &wc_commit), &current_tree).await;

    info!("Creating commit");
    create_commit(workspace, &commit_message, current_tree, &file_changes).await?;
    info!("Commit created successfully");

    Ok(())
}

async fn run_describe(
    workspace: &Workspace,
    language: &str,
    model: &str,
    revision: &str,
) -> Result<()> {
    let repo = workspace.repo_loader().load_at_head()?;
    debug!("Loaded repository at head");

    let repo = if revision == "@" { snapshot_working_copy(workspace, &repo).await? } else { repo };

    let commit = resolve_single_commit(&repo, workspace, revision)?;
    let commit_id = commit.id().hex();
    let short_id = &commit_id[..8.min(commit_id.len())];
    debug!(revision = %revision, commit_id = %short_id, "Resolved target revision");

    let diff = match generate_diff(&repo, &commit).await? {
        Some(diff) => diff,
        None => {
            println!("No changes in revision {short_id}, nothing to describe");
            return Ok(());
        }
    };

    let description = generate_message(&diff, language, model).await?;
    debug!(description = %description, "Generated description");

    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();
    mut_repo
        .rewrite_commit(&commit)
        .set_description(&description)
        .write()?;
    mut_repo.rebase_descendants()?;
    tx.commit(format!("describe revision {short_id} via jc"))?;

    let file_changes =
        get_file_change_summary(&get_parent_tree(&repo, &commit), &commit.tree()).await;

    let title = format!(
        "{}{} {}",
        "Described change ".white().dimmed(),
        short_id.blue().dimmed(),
        if revision == "@" {
            "".to_string()
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
    let remaining = width + 2 - title_width - 1; // -1 for the leading ─
    let border = "─".repeat(remaining.max(0));
    result.push_str(&format!(
        "{}{title}{}{}\n",
        "╭─".white().dimmed(),
        border.white().dimmed(),
        "╮".white().dimmed()
    ));

    for line in &lines {
        let line_width = line.width();
        if line_width <= width {
            let padding = width - line_width;
            result.push_str(&format!(
                "{} {line}{} {}\n",
                "│".white().dimmed(),
                " ".repeat(padding),
                "│".white().dimmed()
            ));
        } else {
            result.push_str(&format!("{} {line} {}\n", "│".white().dimmed(), "│".white().dimmed()));
        }
    }
    result.push_str(&format!(
        "{}{}{}\n",
        "╰".white().dimmed(),
        "─".repeat(width + 2).white().dimmed(),
        "╯".white().dimmed()
    ));
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

    #[test]
    fn test_format_box_with_title_ascii() {
        let result = format_box_with_title("Title", "Hello", 72);
        assert!(result.contains("╭─Title"));
        assert!(result.contains("│ Hello"));
        // All lines should have same width (72 + 4 for borders and spaces)
        let line_widths: Vec<usize> = result.lines().map(|l| l.width()).collect();
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_japanese() {
        let result = format_box_with_title("コミット", "こんにちは", 72);
        let line_widths: Vec<usize> = result.lines().map(|l| l.width()).collect();
        // All lines should have same display width
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_mixed() {
        let result = format_box_with_title("Commit by 太郎", "Hello こんにちは World", 72);
        let line_widths: Vec<usize> = result.lines().map(|l| l.width()).collect();
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_multiline() {
        let content = "タイトル\n\nこれは日本語のテストです";
        let result = format_box_with_title("Committed change a05fdfa2", content, 72);
        let line_widths: Vec<usize> = result.lines().map(|l| l.width()).collect();
        // All lines should have same display width
        assert!(line_widths.iter().all(|&w| w == 76));
    }

    #[test]
    fn test_format_box_with_title_fixed_width() {
        let result = format_box_with_title("Title", "Short", 72);
        let first_line = result.lines().next().unwrap();
        // width=72, plus 4 for borders and spaces = 76
        assert_eq!(first_line.width(), 76);
    }
}
