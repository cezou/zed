# Notion ticket status from Zed — handoff notes

**Delete this file when this branch is merged into `main`.** It exists only to hand the work over
between machines; it is not documentation the fork should keep.

Branch: `worktree-notion-ticket-status`, rebased on `origin/main` (`8239d30556`).

## What was asked

1. Be able to **change a ticket's Notion status from Zed**: clicking the status chip unrolls the
   Notion status list, and picking one writes it back to Notion.
2. When a **new session** is started for a ticket (a fresh worktree *or* an existing one — the modal
   can attach to either), move the ticket to *In progress*. The requested default was
   `2 - 🏁 Ready for dev` → `3 - ⏳ In progress`; the user then asked for it to apply from **any**
   starting status, shown as `before → after` in the modal, with the *after* overridable.
3. Rename the sidebar section listing tickets with no session yet from **"Ready for Dev"** to
   **"No sessions"**.

Decisions taken with the user:

- The picker lists **only the tracked statuses** (`tickets_panel.notion_status_filter`, already in
  settings) — no extra Notion call, and no new settings key.
- The launch modal's *after* defaults to the tracked option whose name contains "progress", and can
  be set to any other tracked option or to "Keep as is" (writes nothing).

## Board facts (verified against the live workspace)

- Data source: `collection://4a087919-bf99-4ceb-bf97-de0e2b2808f1` ("Tracking Issues").
- `Status` is a real Notion **`status`** property (not a `select`), so it has groups.
- Exact option strings: `0 - 🗣️ Discussion`, `🚫 Blocked`, `Waiting for customer`, `1 - 🩺 Triage`,
  `1 - 🖋️ Draft`, `2 - 🏁 Ready for dev`, `3 - ⏳ In progress`, `4 - 🔍 In review`, `5 - 🛫 Staging`,
  `6 - ✅ Validated for Prod`, `7 - 📈 Prod monitoring`, `0 - Abandoned`, `8 - 🛬 Prod`.

## What was implemented

### Notion writes (new — the fork was read-only before this)

- `crates/notion_client/src/mcp_board.rs` — `set_page_status()`, through the already-generic
  `McpClient::call_tool`: `notion-update-page` with `command: "update_properties"` and
  `properties: { <status property>: <option string> }`. This is the path the user's workspace
  actually uses (OAuth/MCP, since Personal Access Tokens are blocked by admin policy).
- `crates/notion_client/src/notion_client.rs` — `NotionClient::set_ticket_status()`, the PAT
  equivalent: `PATCH /pages/{uuid}` with `{"properties": {<prop>: {"status": {"name": …}}}}`.
  Reuses the existing `request` helper, which was already generic over the method.

Trap worth remembering: `TicketRef::page_id` is **not** a UUID on the MCP path (it is the URL's
slug-plus-id segment, kept that way because it is the ticket store's primary key). Both write paths
need `TicketRef::notion_page_uuid()` / `notion_client::extract_page_id(&record.url)`.

### Plumbing

- `agent_ui::SetTicketStatus { ticket_id, status }` action, next to `StartTicketWork` — same reason:
  the sidebar renders the picker but must not depend on the Notion crates.
- `ticket_sync::set_ticket_status()` handles it: writes the store **optimistically** (so the chip
  changes at once instead of waiting up to `refresh_interval_secs` for the next poll), then writes
  to Notion, and on failure rolls the store back and shows a workspace error notification.
- `TicketMetadataStore` gained `set_status()` and a non-persisted `status_options` list, published
  by `ticket_sync` from the settings. That list is how the sidebar builds its menu without a
  dependency on `notion_client`/`ticket_sync`.

### UI

- `crates/ui/src/components/ai/ticket_item.rs` — `TicketItem::status_menu(id, builder)`. When set,
  the status chip is wrapped in a `PopoverMenu` (menu built lazily — one `ContextMenu` entity per
  visible row per frame would be wasted work). The wrapper carries the same
  `on_mouse_down(Left, stop_propagation)` guard the action slot uses, otherwise clicking the chip
  would also toggle the row open.
- `crates/sidebar/src/sidebar.rs` — `render_ticket` passes that menu: one toggleable entry per
  tracked status, the current one checked, each dispatching `SetTicketStatus`.
- `crates/ticket_sync/src/ticket_launch_modal.rs` — the header's single status chip became
  `Chip(current) → DropdownMenu(target)`, backed by a new `target_status: Option<SharedString>`
  field (`default_target_status()` picks the "progress" option). The write happens in
  `apply_target_status()`, called **only after the launch task succeeds** — a ticket whose worktree
  failed to be cut has not been started — through a `WeakEntity<Workspace>` now held by the modal,
  so a Notion failure surfaces as a notification rather than in a footer that is already gone.
- Section label: `NO_SESSIONS_SECTION_LABEL = "No sessions"` (const, enum variant
  `TicketSectionKey::NoSessions`, and the `sidebar_tests.rs` expectation renamed with it).

## Also on this branch: `script/winrun` and the `remote-build` skill are deleted

At the user's request. Nothing on the Linux laptop is to drive builds on the desktop any more; work
moves between machines through git only.

**Follow-up needed when merging:** `CLAUDE.md` and `.rules` (both line ~140, "Running Zed in dev
mode") still tell agents to "build it with the `remote-build` skill". That skill no longer exists, so
the sentence needs rewriting to say the build happens on the desktop directly. Left untouched here on
purpose — this fork's own rules hygiene section says not to edit `.rules` inline during feature work.

## Verification status

The code was checked earlier in the session, but **on the pre-rebase base**, and the equivalent runs
were not repeated after rebasing onto `8239d30556`. Treat the results below as indicative, and
re-run them on the desktop before trusting them:

- `cargo check -p ticket_sync -p sidebar -p notion_client -p agent_ui -p ui` — clean.
- `cargo clippy` on those five crates, `--all-targets` — no new warnings (the two remaining
  `notion_client` ones predate this branch: a `while_let_loop` in `extract_tagged_blocks` and a
  `useless_format` in a test).
- `cargo test -p sidebar` — 150 passed. `cargo test -p notion_client -p agent_ui` — 438 + 33 passed.

Note: whole-workspace `script/clippy` failed for an unrelated reason — `wasmtime-c-api-impl`'s build
script could not find `cmake`.

**Not verified at all: the UI itself.** Nothing here has been clicked. What to check from the Windows
desktop with the `run-zed` skill:

1. Click a ticket's status chip in the sidebar → the menu lists the 8 tracked statuses with the
   current one checked; the row must **not** expand/collapse from that click.
2. Pick another status → the chip updates immediately, and the Notion page really changes.
3. Open the launch modal on a `2 - 🏁 Ready for dev` ticket → header reads
   `2 - 🏁 Ready for dev → 3 - ⏳ In progress`; launch → session starts **and** Notion moves.
4. Launch with "Keep as is" → no status write.
5. Break the OAuth token and repeat (2): expect an error notification and the chip reverting to its
   old value.
