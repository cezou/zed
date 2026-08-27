# Notion ticket status from Zed — handoff notes

**Delete this file when this branch is merged into `main`.** It exists only to hand the work over
between machines; it is not documentation the fork should keep.

Branch: `notion-ticket-status`, on `origin/main` (`1a0fcdd45e`). It merges the two earlier
branches: everything on `worktree-fix-notion-ticket-url` was already in `main` except its tip
(`Repair Notion ticket links that arrive as a bare page id`), which is cherry-picked here alongside
the two `worktree-notion-ticket-status` feature commits and the `winrun` removal.

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

**Follow-up done:** the "Running Zed in dev mode" rule pointed at the now-deleted `remote-build`
skill; it is rewritten in its own commit to describe a local build. (`CLAUDE.md` is a symlink to
`.rules`, so there is only one file to change, not two.)

## Also on this branch: the ticket url fix

Notion's query engine hands rows back as `https://app.notion.com/<32hex>` on this workspace, which
**404s** — only `https://app.notion.com/p/<32hex>` resolves. Both were checked against the live
workspace with `curl -I`. `normalize_page_url()` in `notion_client` rewrites the bare-id shape and
leaves every other shape alone; it is applied at three points: `parse_row` (so new syncs store the
openable form), the sidebar's link (for rows synced before the fix), and `render_brief` (the
`brief.md` a session is handed — the one already on disk carries the 404 form).

## Verification status

Re-run on the Windows desktop on this exact tree (2026-08-27), `dev` profile, 16 threads:

- `cargo build -p zed` — clean in 3m03s from the shared `target/`. The only warning is a
  pre-existing `LNK4217` between `wasmtime_c_api` and `tree_sitter`.
- `cargo clippy -p ticket_sync -p sidebar -p notion_client -p agent_ui -p ui --all-targets` —
  exit 0, no new warnings. The two `notion_client` ones predate this work: a `while_let_loop` in
  `extract_tagged_blocks` and a `useless_format` in a test.
- `cargo test -p notion_client -p sidebar -p ticket_sync` — 39 + 150 + 11 passed, 0 failed.

Note: whole-workspace `script/clippy` needs `cmake`, which ships with Visual Studio but is not on
`PATH`; prepend
`C:\Program Files\Microsoft Visual Studio\18\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`.

### UI, verified by driving the app

1. **Status picker** — confirmed. Clicking a ticket's status chip opens a menu listing exactly the
   8 statuses from `notion_status_filter`, with the ticket's current one (`3 - ⏳ In progress`)
   checked, and the row does **not** expand from that click.
2. **Section label** — confirmed, the section reads "No sessions".
3. **Launch moves the ticket in Notion** — confirmed end to end, though by accident: a stray click
   launched a real session on "Spider tap_portugal non fonctionnelle en canary", and the Notion page
   moved `2 - 🏁 Ready for dev` → `3 - ⏳ In progress` (verified by querying the board afterwards).
   The generated `.zed-ticket/brief.md` also carried the repaired `/p/` url.

Still unverified, and needing a deliberate run rather than a stray click:

- The launch modal's `before → after` header and overriding the *after*, including "Keep as is"
  (writes nothing).
- The failure path: with a broken OAuth token, picking a status should show an error notification
  and revert the chip.

### The `run-zed` skill's UI Automation path is currently broken

`list-elements` returns an empty tree on this build: the window's UIA root (`Zed::Window`) is found,
but `FindAll(Descendants)` reports 0 nodes, and stays at 0 after foreground activation and several
frames. The `gpui_windows` AccessKit bridge and the `WM_GETOBJECT` dispatch are both present in the
source, so the adapter is simply never activating. This is independent of this branch — it needs its
own investigation. `screenshot` still works, so the verification above was done with geometric
clicks derived from the screenshots, which is fragile: the ticket list reorders under you on each
Notion poll, and that is exactly how the stray launch happened.
