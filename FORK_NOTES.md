# Zed fork — Notion tickets in the sidebar

A fork of `zed-industries/zed` that turns the agent sidebar into a work queue:
your assigned Notion tickets, the git worktree each one lives in, and the
Claude Code CLI sessions running against that worktree.

Everything below is implemented and lives on `main`.

## What it does

### Tickets in the left sidebar

`crates/sidebar/src/sidebar.rs` renders tickets grouped under the repository
their worktree was cut from (with a separate section for repositories not open
in this window). Tickets with no worktree yet are collected in a "Ready for
Dev" section. Each ticket row expands to the Claude Code sessions launched for
it.

The ticket rows are fed by `TicketMetadataStore`
(`crates/agent_ui/src/ticket_metadata_store.rs`), a SQLite-backed global that
survives restarts. It is the only ticket source the UI reads.

### Notion sync

`crates/ticket_sync` polls the configured Notion board and upserts the results
into that store. Two credential paths: a Personal Access Token over the REST
API, or OAuth against Notion's public MCP server (`notion: Connect to Notion`)
for workspaces where admin policy blocks token creation. OAuth wins when both
are configured.

The poll is a process-wide global (`TicketSyncService`), registered from
`ticket_sync::init`, so a second window does not start a second poll.

Settings live under the `tickets_panel` key — the crate was renamed but the key
was not, so existing `settings.json` files keep working.

### Launch modal

`crates/ticket_sync/src/ticket_launch_modal.rs`, opened from a ticket row via
the `agent_ui::StartTicketWork` action. It collects:

- the repository to cut the worktree from (a registry in settings, plus a
  folder picker that appends to it — `repository_registry.rs`),
- the worktree name,
- an editable brief, seeded from the Notion page body fetched over MCP
  `notion-fetch` (`crates/notion_client/src/page_body.rs`),
- images pasted from the clipboard (`clipboard_images.rs`), written next to the
  brief and referenced with `@`.

### Worktrees and sessions

Worktree creation shells out to `git gtr new <branch> --porcelain`. The brief
and its images are written into `.zed-ticket/` inside the new worktree.

A session is started as:

```
claude --session-id <uuid> --permission-mode plan "Read .zed-ticket/brief.md and start working on it."
```

Because the session id is chosen by Zed rather than by Claude, a recorded
session can always be resumed with `claude --resume <id>` — which is why
sessions survive a reboot. Session records live in `TicketMetadataStore`
alongside the ticket.

### Sessions live in the workspace center

A session's terminal is an item of the workspace's center pane group, not of the
agent panel's dock, and launching one closes that dock. The center is the flex
between the docks, so the session fills the frame beside the git panel with no
empty gap in between — and, being in a real `Pane`, it gets Zed's splitting,
tab bar, search bar and pane navigation for free.

The agent panel stays the registry: it owns `AgentTerminal` (Claude liveness,
bell, titles, `cc_session_id`) and still respawns sessions with
`claude --resume`. Only the display surface moved. `AgentTerminal::host` records
which of the two a terminal uses.

An additional session splits the ticket's live session instead of replacing it,
in the direction given by `agent.session_split_direction` (default `right`).
Shortcuts, bound to `Terminal && TicketSession`:

| Shortcut | Effect |
| --- | --- |
| `ctrl-alt-<arrow>` | one more Claude Code session for the same ticket |
| `ctrl-alt-shift-<arrow>` | a bare shell in the same worktree (`clone_on_split`) |

`ctrl-k <arrow>` is unbound there on purpose: `Pane` binds it as a multi-key
prefix, which would swallow readline's kill-line inside Claude's own prompt.

Closing a session's tab detaches it — the `TicketSessionRecord` survives, so it
stays in the sidebar and `claude --resume` can pick it up. `TerminalView` opts
out of the workspace's own item serialization
(`SerializableItem::included_in_workspace_serialization`) when it is a session:
otherwise a restart would rebuild it as a bare shell next to the real session
the agent panel restores. Sessions other than the last active one are restored
lazily, when their sidebar row is clicked.

### `agent.show_zed_agent_threads`

Defaults to `false` and hides Zed's own agent threads from the sidebar, leaving
only tickets and Claude Code sessions.

## Development

Build and run: see `CLAUDE.md`.

UI changes are verified from **native Windows PowerShell**, not WSL — Zed's
GPU-composited window cannot be screenshotted through WSL's X11. The
`.claude/skills/run-zed/` skill drives the app through UI Automation (backed by
GPUI's AccessKit tree) rather than pixel coordinates: it can launch, click named
elements, type, and screenshot.

Cutting a release: `.claude/skills/release/`.

## Remotes

- `origin` → `https://github.com/cezou/zed.git`
- `upstream` → `https://github.com/zed-industries/zed.git`
