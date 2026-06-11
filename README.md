# jc

A [Jujutsu](https://www.jj-vcs.dev/) (`jj`) CLI tool that uses Claude or Codex to generate commit messages and bookmark names.

## Description

`jc` is a standalone command-line tool for Jujutsu workspaces that:

- Automatically generates commit messages from diffs using Codex
- Generates and sets commit descriptions on any revision using AI
- Generates meaningful bookmark (branch) names from commit summaries

## Features

- Automatic jj workspace discovery
- Diff extraction using jj-lib (in-process, no shell-out)
- Codex-powered commit message and bookmark name generation
- Conventional commits format
- Smart bookmark handling: reuses existing bookmarks in the branch, syncs to git refs
- Records the user prompts you sent to coding agents and folds them into commit messages (inspired by [ayumi](https://github.com/stefafafan/ayumi))

## Prerequisites

- Rust toolchain for building
- [Jujutsu (jj)](https://github.com/martinvonz/jj)
- [Codex CLI](https://github.com/openai/codex) or [Claude Code](https://claude.com/product/claude-code) for message generation

## Installation

```console
$ cargo install --git https://github.com/0x6b/jc
```

## Usage

### Add (record agent prompts)

Record a user prompt so it can be folded into the next commit message. The prompt is read from
standard input — either a coding-agent hook payload (JSON with a `prompt`, `user_prompt`, or
`input` field) or plain text:

```bash
$ echo "Add JWT authentication" | jc add
# or use the alias:
$ echo "..." | jc a
```

Wire it into your agent's `UserPromptSubmit` hook so every instruction is captured automatically.
For example, with Claude Code or Codex:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "jc add -p /absolute/path/to/workspace" }
        ]
      }
    ]
  }
}
```

Recorded prompts are stored per-workspace **outside** the repository (default:
`<platform data dir>/jc`, e.g. `~/.local/share/jc` on Linux, `~/Library/Application Support/jc` on
macOS). Override with `JC_PROMPT_STORAGE_DIR`. Only your instructions are stored — never AI
responses, transcripts, reasoning, or tool output.

> [!WARNING]
> `jc add` copies raw prompts into your commit messages. Do not include secrets, credentials, or
> anything else you do not want recorded.

When you later run `jc` or `jc describe`, the prompts recorded since the parent commit are appended
to the generated message as a quoted section:

```text
feat: add JWT middleware

AI Instructions:
> Add JWT authentication

> Move it into middleware
```

Customize the heading with `JC_PROMPT_HEADING` (default: `AI Instructions`), or skip the section for
a single run with `--no-instructions`.

### Commit (default command)

Generate a commit message and commit changes:

```bash
$ jc
# or explicitly:
$ jc commit
```

Options:

- `-l, --language <LANGUAGE>` - Language for commit messages [default: English]
- `-m, --model <MODEL>` - Codex model to use [default: auto]
- `-p, --path <PATH>` - Path to workspace [default: current directory]
- `--no-instructions` - Don't append recorded user prompts as an "AI Instructions" section

### Describe

Generate and set a commit description on any revision (without creating a new commit):

```bash
$ jc describe
# or use the alias:
$ jc d
# describe a specific revision:
$ jc d -r @-
```

Options:

- `-r, --revision <REV>` - Revision to describe [default: @]
- `-l, --language <LANGUAGE>` - Language for commit messages [default: English]
- `-m, --model <MODEL>` - Codex model to use [default: auto]
- `-p, --path <PATH>` - Path to workspace [default: current directory]
- `--no-instructions` - Don't append recorded user prompts as an "AI Instructions" section

Behavior:

- Diffs the target revision against its parent to generate a description
- When targeting `@`, snapshots the working copy first so the tree is up-to-date
- Rewrites the commit description in-place (no new commit is created)

### Bookmark

Generate and set a bookmark name for the current branch:

```bash
$ jc bookmark
# or use the alias:
$ jc b
```

Options:

- `-f, --from <REV>` - Base revision [default: main@origin or main]
- `-t, --to <REV>` - Target revision [default: @, or @- if @ is empty]
- `--prefix <PREFIX>` - Add prefix (e.g., `feature` → `feature/generated-name`)
- `--dry-run` - Print generated name without creating bookmark

Behavior:

- If a bookmark already exists in the branch range, it moves that bookmark to the target
- Otherwise, generates a new name from commit summaries using Codex
- Automatically exports to git refs (no `@git` drift)

Example workflow:

```bash
# Make changes and commit
$ jc

# Create/update bookmark for the branch
$ jc b

# Push to remote
$ jj git push
```

## How It Works

### Commit

1. Discovers jj workspace from current directory
2. Snapshots working copy and compares with parent tree
3. Generates diff using jj-lib
4. Calls Codex CLI to generate conventional commit message
5. Appends user prompts recorded since the parent commit as an "AI Instructions" section
6. Creates commit with generated message

### Describe

1. Resolves target revision (default: `@`)
2. For `@`, snapshots working copy to capture pending file changes
3. Diffs target revision against its parent tree
4. Calls Codex CLI to generate conventional commit message
5. Appends user prompts recorded since the parent commit as an "AI Instructions" section
6. Rewrites the commit description in-place

### Bookmark

1. Resolves target revision (uses `@-` if `@` is empty)
2. Checks for existing bookmark in the branch range (`from..to`)
3. If found, moves existing bookmark to target
4. If not, generates name from commit summaries via Codex
5. Exports bookmark to git refs

## Configuration

### User Configuration

Loads existing jj configuration from:

- `~/.jjconfig.toml`
- `~/.config/jj/config.toml`

### Prompt Recording

Controlled with environment variables:

- `JC_PROMPT_STORAGE_DIR` - Directory for recorded prompts (default: `<platform data dir>/jc`). Must be outside the workspace.
- `JC_PROMPT_HEADING` - Heading for the appended section (default: `AI Instructions`).

Prompts are stored as JSON Lines, one file per workspace, keyed by a hash of the workspace path. At
commit/describe time, prompts recorded after the parent commit's timestamp are included (so each
prompt is folded into exactly one commit).

### Diff Collapsing

Large or noisy diffs are automatically collapsed to summary lines to keep LLM prompts focused. Collapsing is triggered by:

- **Built-in patterns** — lock files (`*.lock`, `package-lock.json`, etc.), minified files (`*.min.js`, `*.min.css`), generated files (`*.generated.*`, `*.pb.go`, `*.pb.rs`), and vendored directories (`vendor/**`, `node_modules/**`, `third_party/**`)
- **`.gitattributes`** — files marked with `-diff`, `binary`, or `linguist-generated` in the workspace root `.gitattributes` are collapsed with a distinct reason (e.g., `collapsed: gitattributes (-diff)`)
- **Size limits** — per-file limits (2048 lines / 64 KB) and total diff limits (8192 lines / 256 KB) truncate remaining large diffs

Collapsed files still appear in the diff output as a one-line summary so the LLM knows the file changed.

## License

MIT. See [LICENSE](./LICENSE) for details.
