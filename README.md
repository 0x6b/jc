# jc

A [Jujutsu](https://www.jj-vcs.dev/) (`jj`) CLI tool that uses Claude to generate commit messages and bookmark names.

## Description

`jc` is a standalone command-line tool for Jujutsu workspaces that:

- Automatically generates commit messages from diffs using Claude AI
- Generates and sets commit descriptions on any revision using AI
- Generates meaningful bookmark (branch) names from commit summaries

## Features

- Automatic jj workspace discovery
- Diff extraction using jj-lib (in-process, no shell-out)
- Claude-powered commit message and bookmark name generation
- Conventional commits format
- Smart bookmark handling: reuses existing bookmarks in the branch, syncs to git refs

## Prerequisites

- Rust toolchain (for building)
- [Jujutsu (jj)](https://github.com/martinvonz/jj) - Version control system
- [Claude CLI](https://github.com/anthropics/claude-cli) - For AI generation

## Installation

```console
$ cargo install --git https://github.com/0x6b/ccc-jj
```

## Usage

### Commit (default command)

Generate a commit message and commit changes:

```bash
$ jc
# or explicitly:
$ jc commit
```

Options:

- `-l, --language <LANGUAGE>` - Language for commit messages [default: English]
- `-m, --model <MODEL>` - Claude model to use [default: haiku]
- `-p, --path <PATH>` - Path to workspace [default: current directory]

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
- `-m, --model <MODEL>` - Claude model to use [default: haiku]
- `-p, --path <PATH>` - Path to workspace [default: current directory]

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
- Otherwise, generates a new name from commit summaries using Claude
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
4. Calls Claude CLI to generate conventional commit message
5. Creates commit with generated message

### Describe

1. Resolves target revision (default: `@`)
2. For `@`, snapshots working copy to capture pending file changes
3. Diffs target revision against its parent tree
4. Calls Claude CLI to generate conventional commit message
5. Rewrites the commit description in-place

### Bookmark

1. Resolves target revision (uses `@-` if `@` is empty)
2. Checks for existing bookmark in the branch range (`from..to`)
3. If found, moves existing bookmark to target
4. If not, generates name from commit summaries via Claude
5. Exports bookmark to git refs

## Configuration

### User Configuration

Loads existing jj configuration from:

- `~/.jjconfig.toml`
- `~/.config/jj/config.toml`

### Claude CLI

Uses Claude CLI's existing configuration. Ensure it's properly configured with API credentials.

## License

MIT. See [LICENSE](./LICENSE) for details.
