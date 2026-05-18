# Zed fork — cascade worktrees + Claude Code CLI sidebar

This repository is a fork of `zed-industries/zed` with two work-in-progress features. Each lives on its own branch off `main`, which tracks upstream. Future agent sessions should read this file before doing anything.

## What we're building and why

The user has two UX frustrations with Zed's Agent panel:

1. **Sidebar History groups threads by *project*, not by *worktree*.** Picking a worktree-specific thread needs three clicks (project → `…` → choose worktree). They want each worktree to appear directly as its own row, cascaded under the project name.
2. **Local Claude Code CLI sessions are invisible inside Zed.** Sessions stored in `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` should show up alongside Zed threads (title = last user prompt, timestamp, running/idle status), and clicking one should **spawn a Zed terminal that runs `claude -r <uuid>`** — not implement an ACP bridge. Rationale: native `claude` CLI exposes the full surface (`/mcp`, `/skills`, `/agents`, `/context`, MCP, hooks); ACP only exposes a JSON-RPC subset. The user explicitly said "Claude Code is better than ACP."

The two features are independent. The cascade is designed to be merge-friendly upstream (default off, behind a setting).

## Branches

| Branch | Status | What it does |
| --- | --- | --- |
| `main` | tracks `zed-industries/zed` (upstream) | reference, never patched |
| `feat/sidebar-worktree-cascade` | **needs verification — never successfully compiled** | adds the cascade renderer + `agent.group_by_worktree` setting |
| `feat/claude-code-threads` | **incomplete** — module written but not wired into sidebar | adds `~/.claude/projects/` scanner + `agent.show_claude_code_sessions` + `agent.claude_code_command` settings |

Remotes:
- `origin` → `https://github.com/cezou/zed.git` (the user's GitHub fork; isFork=true, parent=zed-industries/zed)
- `upstream` → `https://github.com/zed-industries/zed.git`

## Files touched per branch

### `feat/sidebar-worktree-cascade`

| File | Change |
| --- | --- |
| `crates/settings_content/src/agent.rs` | new field `group_by_worktree: Option<bool>` on `AgentSettingsContent` |
| `crates/agent_settings/src/agent_settings.rs` | new field `group_by_worktree: bool` on `AgentSettings`; `from_settings` reads it with `unwrap_or(false)` |
| `crates/agent/src/tool_permissions.rs` | test/default `AgentSettings` initializer extended with `group_by_worktree: false` |
| `assets/settings/default.json` | documented new key `"group_by_worktree": false` |
| `crates/sidebar/src/sidebar.rs` | added `worktree_sub_label: Option<SharedString>` to `ListEntry::ProjectHeader`; render path swaps label/highlights when set; `push_entries_by_display_time` accepts `Option<ProjectGroupKey>` — when `Some`, buckets time-sorted rows by `worktrees.first().full_path` and emits a sub-header per bucket. The two `entries.push(ListEntry::ProjectHeader { … })` sites pass `group_by_worktree.then_some(group_key.clone())` |

### `feat/claude-code-threads`

| File | Change |
| --- | --- |
| `crates/settings_content/src/agent.rs` | new fields `show_claude_code_sessions: Option<bool>`, `claude_code_command: Option<String>` |
| `crates/agent_settings/src/agent_settings.rs` | new fields `show_claude_code_sessions: bool`, `claude_code_command: Arc<str>`; `from_settings` defaults to `false` / `"claude"` |
| `crates/agent/src/tool_permissions.rs` | test default initializer extended |
| `assets/settings/default.json` | documented new keys |
| `crates/agent_ui/src/agent_ui.rs` | `pub mod claude_code_sessions;` |
| `crates/agent_ui/src/claude_code_sessions.rs` | **new file** (~344 lines): scans `~/.claude/projects/`, parses head + bounded tail of each JSONL, exposes `ClaudeCodeSession { session_id, cwd, title, last_user_prompt, last_activity, status, git_branch }`. Status is `Running` / `Idle` / `Closed`, derived from last non-attachment message type + mtime recency. Has unit tests for the path-decoding helpers |

**Not yet done on this branch:**
- Sidebar UI section to render the sessions
- Click handler to spawn `<claude_code_command> -r <session-id>` in a Zed terminal at the recorded `cwd`
- File watcher for live status updates (currently scan-on-demand)
- `/proc` scan for attached PIDs (currently mtime is the only liveness signal)

## Build environment

The user's machine is Ubuntu 24.04, x86_64. `rustup` is installed (`~/.cargo/bin/cargo`); system deps from `script/linux` were installed on 2026-05-18. Existing Zed lives at `~/.local/zed.app/` with the symlink `~/.local/bin/zed`.

**Crash warning:** the first `script/install-linux` on `feat/sidebar-worktree-cascade` crashed the machine during the release build (Zed has ~1500 crates and `cargo build` defaults to all-cores parallelism, which spikes RAM). Cap parallelism on the next attempt:

```bash
source "$HOME/.cargo/env"
git checkout feat/sidebar-worktree-cascade
CARGO_BUILD_JOBS=4 cargo build --release -p zed   # compile only, no install yet
```

The `target/` directory from the crashed build is preserved (~7+ GB). Subsequent invocations resume incrementally.

Once `cargo build` succeeds:

```bash
CARGO_BUILD_JOBS=4 script/install-linux
```

This replaces `~/.local/zed.app/libexec/zed-editor` with the new binary and refreshes the desktop file.

## How to enable each feature

Edit `~/.config/zed/settings.json`:

```jsonc
{
  "agent": {
    // feat/sidebar-worktree-cascade
    "group_by_worktree": true,

    // feat/claude-code-threads (settings are read but UI isn't wired yet)
    "show_claude_code_sessions": true,
    "claude_code_command": "claude"
  }
}
```

## Verification plan — step by step

### 1. Compile `feat/sidebar-worktree-cascade`

```bash
cd ~/Documents/Projets/ZedWorktreeClaudeCodePannel
git checkout feat/sidebar-worktree-cascade
source "$HOME/.cargo/env"
CARGO_BUILD_JOBS=4 cargo build --release -p zed
```

Most likely failure sites if it doesn't compile:
- `crates/sidebar/src/sidebar.rs::push_entries_by_display_time`: type mismatch on `HashMap<Option<SharedString>, Vec<ListEntry>>` or borrow conflict around `buckets.remove(&bucket)` inside the bucket loop
- `crates/sidebar/src/sidebar.rs` render destructure (around the `match entry` for `ListEntry::ProjectHeader { … }`): `display_highlights: &[usize]` coercion from `&Vec<usize>` should be implicit; if not, use `highlight_positions.as_slice()`
- A forgotten match arm — every `match entry { ListEntry::ProjectHeader { … } => … }` that fully destructures must bind `worktree_sub_label` or use `..`. Grep `ListEntry::ProjectHeader` in `crates/sidebar/src/sidebar.rs` to audit
- `AgentSettings::get_global(cx).group_by_worktree` access requires `use settings::Settings as _;` in `sidebar.rs` — already present in the file

### 2. Install and test the cascade

```bash
CARGO_BUILD_JOBS=4 script/install-linux
# Back up the user's settings first, then add the toggle:
cp ~/.config/zed/settings.json ~/.config/zed/settings.json.bak
# Add "agent": { "group_by_worktree": true } to settings.json
zed
```

Test cases:
1. Open a project with multiple linked worktrees (the user has `~/cayzn-tracking-zen` plus `~/zen/feat-*` worktrees — see `~/.claude/projects/` directory names for the full list)
2. Open the Agent panel sidebar
3. Verify each project header expands into one row per worktree (label = `<worktree_name> (<branch>)`), threads nested below
4. Toggle the setting to `false`, reload Zed — should fall back to upstream layout
5. Edge case: a thread without any `worktrees` info should appear under the project header *before* the first worktree sub-header

### 3. Compile + unit-test `feat/claude-code-threads`

```bash
git checkout feat/claude-code-threads
CARGO_BUILD_JOBS=4 cargo build --release -p zed
cargo test -p agent_ui --lib claude_code_sessions
```

The unit tests cover `decode_project_dir_name` and `truncate_for_title`. They don't read the user's real `~/.claude/`.

### 4. Wire `claude_code_sessions` into the sidebar (not started)

Remaining work for `feat/claude-code-threads`:

1. In `crates/sidebar/src/sidebar.rs::rebuild_contents`, when `AgentSettings::get_global(cx).show_claude_code_sessions` is true, call `claude_code_sessions::scan_all(&default_projects_root()?)` (likely behind an `Entity` that owns a cache) and group by cwd. Inject entries under the matching project/worktree header
2. Add a new `ListEntry::ClaudeCodeSession(ClaudeCodeSessionEntry)` variant (or reuse `ListEntry::Thread` with a flag — a new variant is cleaner since rendering and click handlers differ from native threads)
3. Render with status icon (`●` Running, `○` Idle, `✕` Closed), title, timestamp, `last_user_prompt` as secondary line
4. Click handler: read `AgentSettings::get_global(cx).claude_code_command`, then open a Zed terminal at `session.cwd` running `[cmd, "-r", session_id]`. Look at `crates/terminal_view` for the existing API (`open_terminal` or similar)
5. Add a file watcher: spawn an `Entity` that holds a `notify::RecommendedWatcher` over `~/.claude/projects/`; on each event, rescan and `cx.notify()` so the sidebar refreshes
6. Optional: parse `/proc/*/cmdline` periodically to detect attached `claude --session-id <uuid>` processes for a more reliable Running indicator (mtime alone misses crashed-mid-turn sessions and false-positives quiet streaming)

### 5. Optional — PR `feat/sidebar-worktree-cascade` upstream

The cascade is designed to be merge-friendly (default `false`, no behavior change for existing users). Once verified locally:
- Open a PR from `cezou/zed:feat/sidebar-worktree-cascade` to `zed-industries/zed:main`
- Title: `agent_panel: cascade git worktrees in threads sidebar`
- Reference discussion #54865 (Better Agents Sidebar organization)
- Include before/after screenshots

## Pointers

- Original design plan: `/home/cviegas/.claude/plans/jaimerai-pouvoir-importer-mes-encapsulated-umbrella.md`
- Auto-memory the user keeps:
  - `~/.claude/projects/-home-cviegas-Documents-Projets-ZedWorktreeClaudeCodePannel/memory/zed_fork_layout.md`
  - `~/.claude/projects/-home-cviegas-Documents-Projets-ZedWorktreeClaudeCodePannel/memory/feedback_zed_vs_acp.md`
- Zed's own contributor guide for build prerequisites: `docs/src/development/linux.md`
