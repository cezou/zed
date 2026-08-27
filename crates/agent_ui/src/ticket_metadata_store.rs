use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use db::{
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use fs::Fs;
use gpui::{AppContext as _, AsyncApp, Entity, Global, Task, WindowHandle};
use remote::RemoteConnectionOptions;
use ui::{App, Context, SharedString};
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenOptions, OpenResult, Workspace};

use crate::terminal_thread_metadata_store::TerminalThreadMetadataStore;
use crate::{AgentPanel, AgentThreadSource, TerminalId};

pub fn init(cx: &mut App) {
    TicketMetadataStore::init_global(cx);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TicketId(pub SharedString);

impl TicketId {
    pub fn new(notion_page_id: impl Into<SharedString>) -> Self {
        Self(notion_page_id.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketLaunchKind {
    Initial,
    Additional,
    /// Started by handing the worktree to `claude --resume` so Claude's own
    /// picker could continue a session Zed never launched. Such a session has
    /// no `cc_session_id`: the id lives only in Claude's history.
    Resumed,
}

impl TicketLaunchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Additional => "additional",
            Self::Resumed => "resumed",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "additional" => Self::Additional,
            "resumed" => Self::Resumed,
            _ => Self::Initial,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketSessionRecord {
    pub terminal_id: TerminalId,
    pub cc_session_id: Option<String>,
    pub launch_kind: TicketLaunchKind,
    pub created_at: DateTime<Utc>,
    pub last_resumed_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

/// The Notion-sourced half of a ticket record, refreshed on every board sync.
///
/// `body_markdown` is deliberately part of this struct but is *not* something
/// a board query can supply — it costs one extra `notion-fetch` per page, so
/// it is filled in lazily and preserved across syncs by [`TicketMetadataStore::upsert_ticket_ref`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TicketDisplayFields {
    pub title: SharedString,
    pub url: SharedString,
    pub status: Option<SharedString>,
    pub ticket_type: Option<SharedString>,
    pub issue_id: Option<SharedString>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketWorktreeRecord {
    pub ticket_id: TicketId,
    pub title: SharedString,
    pub url: SharedString,
    pub status: Option<SharedString>,
    pub ticket_type: Option<SharedString>,
    pub issue_id: Option<SharedString>,
    /// The Notion page body, cleaned to markdown. Fetched on demand rather
    /// than on every sync.
    pub body_markdown: Option<String>,
    pub body_fetched_at: Option<DateTime<Utc>>,
    pub worktree_path: Option<PathBuf>,
    pub branch_name: Option<String>,
    pub base_repo_root: Option<PathBuf>,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub created_at: DateTime<Utc>,
    pub sessions: Vec<TicketSessionRecord>,
}

impl TicketWorktreeRecord {
    /// The most recently active session for this ticket (by creation time),
    /// used to decide what to resume when a ticket with an existing worktree
    /// is reopened.
    pub fn most_recent_session(&self) -> Option<&TicketSessionRecord> {
        self.sessions
            .iter()
            .max_by_key(|session| session.created_at)
    }

    /// Sessions that were never explicitly closed. Note this is *not* a
    /// liveness signal — a crash or a reboot never writes `ended_at`, so a
    /// caller that needs "is it running right now" must cross-check the
    /// agent panel's live terminals.
    pub fn unclosed_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.ended_at.is_none())
            .count()
    }
}

struct GlobalTicketMetadataStore(Entity<TicketMetadataStore>);
impl Global for GlobalTicketMetadataStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTicketMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTicketMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTicketMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TICKET_METADATA_DB_{}", test_name)
            })
    }
}

/// Where the Notion poll currently stands, so a row that only renders tickets
/// can show a sync running (or failing) without depending on the crate that
/// talks to Notion.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum TicketSyncState {
    #[default]
    Idle,
    Syncing,
    Failed(SharedString),
}

pub struct TicketMetadataStore {
    db: TicketMetadataDb,
    tickets: HashMap<TicketId, TicketWorktreeRecord>,
    sync_state: TicketSyncState,
    /// Reverse index so a caller holding only a `TerminalId` (the sidebar
    /// deciding whether a terminal row belongs to a ticket) doesn't have to
    /// scan every ticket's sessions on every rebuild.
    sessions_by_terminal: HashMap<TerminalId, TicketId>,
    /// The board's tracked status options, refreshed from the settings by the
    /// sync service. Not persisted: it is derived configuration, not state.
    status_options: Vec<SharedString>,
    pending_ops_tx: async_channel::Sender<DbOperation>,
    _db_operations_task: Task<()>,
}

#[derive(Debug, Clone)]
enum DbOperation {
    UpsertWorktree(TicketWorktreeRow),
    UpsertSession {
        ticket_id: TicketId,
        session: TicketSessionRecord,
    },
    DeleteWorktree(TicketId),
}

impl DbOperation {
    /// Distinguishes operations for dedup purposes: two ops with the same key
    /// are collapsed to just the most recent one when flushing a batch.
    /// `UpsertWorktree` and `DeleteWorktree` share a key (both act on the same
    /// `ticket_worktrees` row) so a delete can never be reordered after a
    /// stale upsert of the same ticket within one flush — HashMap iteration
    /// order for the deduped set is otherwise unspecified.
    fn dedup_key(&self) -> (u8, String) {
        match self {
            DbOperation::UpsertWorktree(row) => (0, row.ticket_id.0.to_string()),
            DbOperation::UpsertSession { session, .. } => (1, session.terminal_id.to_key_string()),
            DbOperation::DeleteWorktree(ticket_id) => (0, ticket_id.0.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct TicketWorktreeRow {
    ticket_id: TicketId,
    title: SharedString,
    url: SharedString,
    worktree_path: Option<PathBuf>,
    branch_name: Option<String>,
    base_repo_root: Option<PathBuf>,
    remote_connection: Option<RemoteConnectionOptions>,
    created_at: DateTime<Utc>,
    status: Option<SharedString>,
    ticket_type: Option<SharedString>,
    issue_id: Option<SharedString>,
    body_markdown: Option<String>,
    body_fetched_at: Option<DateTime<Utc>>,
}

/// Orders tickets by the leading integer of their raw Notion status string
/// (`"3 - In progress"` → `3`). Statuses without one sort last so a board that
/// stops numbering its options degrades to title order rather than to an
/// arbitrary one.
fn status_rank(status: Option<&SharedString>) -> u32 {
    status
        .and_then(|status| {
            let digits: String = status
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .unwrap_or(u32::MAX)
}

impl TicketMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTicketMetadataStore>() {
            return;
        }

        let db = TicketMetadataDb::global(cx);
        let store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTicketMetadataStore(store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTicketMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TicketMetadataDb>(&db_name));
        let store = cx.new(|cx| Self::new(TicketMetadataDb(db), cx));
        cx.set_global(GlobalTicketMetadataStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTicketMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTicketMetadataStore>().0.clone()
    }

    pub fn entry(&self, ticket_id: &TicketId) -> Option<&TicketWorktreeRecord> {
        self.tickets.get(ticket_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TicketWorktreeRecord> + '_ {
        self.tickets.values()
    }

    /// Tickets in a deterministic order: by status, then title, then id.
    /// `entries` iterates a `HashMap`, so consecutive calls can yield
    /// different orders; UIs that memoize per-row heights across rebuilds
    /// would reshuffle their rows on every rebuild if fed that.
    pub fn entries_sorted(&self) -> Vec<&TicketWorktreeRecord> {
        let mut entries: Vec<&TicketWorktreeRecord> = self.tickets.values().collect();
        entries.sort_by(|left, right| {
            status_rank(left.status.as_ref())
                .cmp(&status_rank(right.status.as_ref()))
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.ticket_id.0.cmp(&right.ticket_id.0))
        });
        entries
    }

    pub fn ticket_id_for_terminal(&self, terminal_id: TerminalId) -> Option<&TicketId> {
        self.sessions_by_terminal.get(&terminal_id)
    }

    pub fn session_for_terminal(&self, terminal_id: TerminalId) -> Option<&TicketSessionRecord> {
        let ticket_id = self.sessions_by_terminal.get(&terminal_id)?;
        self.tickets
            .get(ticket_id)?
            .sessions
            .iter()
            .find(|session| session.terminal_id == terminal_id)
    }

    /// Records or refreshes a ticket's display metadata as synced from
    /// Notion. Never touches worktree/session state — safe to call on every
    /// refresh regardless of whether a worktree exists yet.
    ///
    /// A board sync cannot supply the page body, so `body_markdown` and
    /// `body_fetched_at` are carried over from the existing record instead of
    /// being cleared on every sync.
    pub fn sync_state(&self) -> &TicketSyncState {
        &self.sync_state
    }

    pub fn set_sync_state(&mut self, state: TicketSyncState, cx: &mut Context<Self>) {
        if self.sync_state == state {
            return;
        }
        self.sync_state = state;
        cx.notify();
    }

    /// Drops a ticket the Notion board no longer returns.
    ///
    /// Only ever called for a ticket with nothing of its own on disk — no
    /// worktree, no sessions — because the board stops returning a ticket for
    /// reasons that have nothing to do with the work: its status moved out of
    /// the configured filter, or it was reassigned. A ticket someone is still
    /// working in stays, with its status refreshed instead.
    pub fn forget_ticket(&mut self, ticket_id: &TicketId, cx: &mut Context<Self>) {
        if self.tickets.remove(ticket_id).is_some() {
            cx.notify();
        }
    }

    /// The tickets the board no longer returns, split by whether they can be
    /// dropped outright or still hold work and must have their real status
    /// fetched instead.
    pub fn tickets_missing_from(&self, returned: &HashSet<TicketId>) -> MissingTickets {
        let mut missing = MissingTickets::default();
        for (ticket_id, record) in &self.tickets {
            if returned.contains(ticket_id) {
                continue;
            }
            if record.worktree_path.is_none() && record.sessions.is_empty() {
                missing.droppable.push(ticket_id.clone());
            } else {
                missing
                    .still_working
                    .push((ticket_id.clone(), record.url.to_string()));
            }
        }
        missing
    }

    pub fn upsert_ticket_ref(
        &mut self,
        ticket_id: TicketId,
        fields: TicketDisplayFields,
        cx: &mut Context<Self>,
    ) {
        let record = if let Some(existing) = self.tickets.get(&ticket_id) {
            TicketWorktreeRecord {
                title: fields.title,
                url: fields.url,
                status: fields.status,
                ticket_type: fields.ticket_type,
                issue_id: fields.issue_id,
                ..existing.clone()
            }
        } else {
            TicketWorktreeRecord {
                ticket_id,
                title: fields.title,
                url: fields.url,
                status: fields.status,
                ticket_type: fields.ticket_type,
                issue_id: fields.issue_id,
                body_markdown: None,
                body_fetched_at: None,
                worktree_path: None,
                branch_name: None,
                base_repo_root: None,
                remote_connection: None,
                created_at: Utc::now(),
                sessions: Vec::new(),
            }
        };
        self.save_worktree_record(record, cx);
    }

    /// Overwrites a ticket's status locally, so the sidebar reflects a status
    /// change immediately instead of waiting for the next board poll. The
    /// caller is responsible for writing the same value to Notion, and for
    /// calling this again with the previous value if that write fails.
    pub fn set_status(
        &mut self,
        ticket_id: &TicketId,
        status: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(existing) = self.tickets.get(ticket_id) else {
            anyhow::bail!("cannot set the status of an unknown ticket {ticket_id:?}");
        };
        if existing.status == status {
            return Ok(());
        }
        let record = TicketWorktreeRecord {
            status,
            ..existing.clone()
        };
        self.save_worktree_record(record, cx);
        Ok(())
    }

    /// The status options a ticket can be moved to, in board order.
    ///
    /// Lives here rather than in the settings so the sidebar can render a
    /// status picker without depending on the Notion crates — the same reason
    /// it dispatches [`crate::SetTicketStatus`] instead of writing to Notion
    /// itself.
    pub fn status_options(&self) -> &[SharedString] {
        &self.status_options
    }

    pub fn set_status_options(&mut self, options: Vec<SharedString>, cx: &mut Context<Self>) {
        if self.status_options == options {
            return;
        }
        self.status_options = options;
        cx.notify();
    }

    /// Caches the ticket's Notion page body, which is fetched lazily rather
    /// than during a board sync.
    pub fn save_body(
        &mut self,
        ticket_id: &TicketId,
        markdown: String,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(existing) = self.tickets.get(ticket_id) else {
            anyhow::bail!("cannot save a body for an unknown ticket {ticket_id:?}");
        };
        let record = TicketWorktreeRecord {
            body_markdown: Some(markdown),
            body_fetched_at: Some(Utc::now()),
            ..existing.clone()
        };
        self.save_worktree_record(record, cx);
        Ok(())
    }

    /// Persists a newly created worktree's location for a ticket. The ticket
    /// must already have an entry (from `upsert_ticket_ref`).
    pub fn save_worktree(
        &mut self,
        ticket_id: &TicketId,
        worktree_path: PathBuf,
        branch_name: String,
        base_repo_root: PathBuf,
        remote_connection: Option<RemoteConnectionOptions>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(existing) = self.tickets.get(ticket_id) else {
            anyhow::bail!("cannot save a worktree for an unknown ticket {ticket_id:?}");
        };
        let record = TicketWorktreeRecord {
            worktree_path: Some(worktree_path),
            branch_name: Some(branch_name),
            base_repo_root: Some(base_repo_root),
            remote_connection,
            ..existing.clone()
        };
        self.save_worktree_record(record, cx);
        Ok(())
    }

    fn save_worktree_record(&mut self, record: TicketWorktreeRecord, cx: &mut Context<Self>) {
        let row = TicketWorktreeRow {
            ticket_id: record.ticket_id.clone(),
            title: record.title.clone(),
            url: record.url.clone(),
            worktree_path: record.worktree_path.clone(),
            branch_name: record.branch_name.clone(),
            base_repo_root: record.base_repo_root.clone(),
            remote_connection: record.remote_connection.clone(),
            created_at: record.created_at,
            status: record.status.clone(),
            ticket_type: record.ticket_type.clone(),
            issue_id: record.issue_id.clone(),
            body_markdown: record.body_markdown.clone(),
            body_fetched_at: record.body_fetched_at,
        };
        self.tickets.insert(record.ticket_id.clone(), record);
        self.pending_ops_tx
            .try_send(DbOperation::UpsertWorktree(row))
            .log_err();
        cx.notify();
    }

    /// Records a newly launched or resumed Claude Code session for a ticket.
    /// The caller (`AgentPanel::spawn_ticket_terminal`) is expected to call
    /// this *before* actually spawning the terminal, so a crash mid-launch
    /// still leaves a resumable record.
    pub fn add_session(
        &mut self,
        ticket_id: &TicketId,
        session: TicketSessionRecord,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(existing) = self.tickets.get_mut(ticket_id) else {
            anyhow::bail!("cannot add a session for an unknown ticket {ticket_id:?}");
        };
        existing
            .sessions
            .retain(|s| s.terminal_id != session.terminal_id);
        existing.sessions.push(session.clone());
        self.sessions_by_terminal
            .insert(session.terminal_id, ticket_id.clone());
        self.pending_ops_tx
            .try_send(DbOperation::UpsertSession {
                ticket_id: ticket_id.clone(),
                session,
            })
            .log_err();
        cx.notify();
        Ok(())
    }

    /// Records that a session's terminal was resumed. Terminals that don't
    /// belong to a ticket (plain agent-panel terminals share these call sites)
    /// are ignored rather than treated as an error.
    pub fn mark_session_resumed(
        &mut self,
        terminal_id: TerminalId,
        at: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) {
        self.update_session(terminal_id, cx, |session| {
            session.last_resumed_at = Some(at);
            session.ended_at = None;
        });
    }

    /// Records that a session's terminal was closed. As with
    /// [`Self::mark_session_resumed`], an unknown terminal is a no-op.
    pub fn mark_session_ended(
        &mut self,
        terminal_id: TerminalId,
        at: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) {
        self.update_session(terminal_id, cx, |session| {
            session.ended_at = Some(at);
        });
    }

    fn update_session(
        &mut self,
        terminal_id: TerminalId,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut TicketSessionRecord),
    ) {
        let Some(ticket_id) = self.sessions_by_terminal.get(&terminal_id).cloned() else {
            return;
        };
        let Some(record) = self.tickets.get_mut(&ticket_id) else {
            return;
        };
        let Some(session) = record
            .sessions
            .iter_mut()
            .find(|session| session.terminal_id == terminal_id)
        else {
            return;
        };
        update(session);
        let session = session.clone();
        self.pending_ops_tx
            .try_send(DbOperation::UpsertSession { ticket_id, session })
            .log_err();
        cx.notify();
    }

    pub fn delete_worktree(&mut self, ticket_id: &TicketId, cx: &mut Context<Self>) {
        if let Some(record) = self.tickets.remove(ticket_id) {
            for session in &record.sessions {
                self.sessions_by_terminal.remove(&session.terminal_id);
            }
        }
        self.pending_ops_tx
            .try_send(DbOperation::DeleteWorktree(ticket_id.clone()))
            .log_err();
        cx.notify();
    }

    fn new(db: TicketMetadataDb, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let _db_operations_task = cx.background_spawn({
            let db = db.clone();
            async move {
                while let Ok(first_update) = rx.recv().await {
                    let mut updates = vec![first_update];
                    while let Ok(update) = rx.try_recv() {
                        updates.push(update);
                    }
                    let updates = Self::dedup_db_operations(updates);
                    for operation in updates {
                        match operation {
                            DbOperation::UpsertWorktree(row) => {
                                db.save_worktree(row).await.log_err();
                            }
                            DbOperation::UpsertSession { ticket_id, session } => {
                                db.save_session(ticket_id, session).await.log_err();
                            }
                            DbOperation::DeleteWorktree(ticket_id) => {
                                db.delete_worktree(ticket_id).await.log_err();
                            }
                        }
                    }
                }
            }
        });

        let mut this = Self {
            db,
            tickets: HashMap::default(),
            sync_state: TicketSyncState::default(),
            sessions_by_terminal: HashMap::default(),
            status_options: Vec::new(),
            pending_ops_tx: tx,
            _db_operations_task,
        };
        this.reload(cx);
        this
    }

    fn dedup_db_operations(operations: Vec<DbOperation>) -> Vec<DbOperation> {
        let mut ops = HashMap::default();
        for operation in operations.into_iter().rev() {
            let key = operation.dedup_key();
            if ops.contains_key(&key) {
                continue;
            }
            ops.insert(key, operation);
        }
        ops.into_values().collect()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let (worktrees, sessions) = cx
                .background_spawn(async move {
                    let worktrees = db
                        .list_worktrees()
                        .context("Failed to fetch ticket worktrees")
                        .log_err()
                        .unwrap_or_default();
                    let sessions = db
                        .list_sessions()
                        .context("Failed to fetch ticket sessions")
                        .log_err()
                        .unwrap_or_default();
                    (worktrees, sessions)
                })
                .await;

            this.update(cx, |this, cx| {
                this.tickets.clear();
                this.sessions_by_terminal.clear();
                for row in worktrees {
                    this.tickets.insert(
                        row.ticket_id.clone(),
                        TicketWorktreeRecord {
                            ticket_id: row.ticket_id,
                            title: row.title,
                            url: row.url,
                            status: row.status,
                            ticket_type: row.ticket_type,
                            issue_id: row.issue_id,
                            body_markdown: row.body_markdown,
                            body_fetched_at: row.body_fetched_at,
                            worktree_path: row.worktree_path,
                            branch_name: row.branch_name,
                            base_repo_root: row.base_repo_root,
                            remote_connection: row.remote_connection,
                            created_at: row.created_at,
                            sessions: Vec::new(),
                        },
                    );
                }
                for (ticket_id, session) in sessions {
                    if let Some(record) = this.tickets.get_mut(&ticket_id) {
                        this.sessions_by_terminal
                            .insert(session.terminal_id, ticket_id.clone());
                        record.sessions.push(session);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

struct TicketMetadataDb(ThreadSafeConnection);

impl Domain for TicketMetadataDb {
    const NAME: &str = stringify!(TicketMetadataDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS ticket_worktrees(
                ticket_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                url TEXT NOT NULL,
                worktree_path TEXT,
                branch_name TEXT,
                base_repo_root TEXT,
                remote_connection TEXT,
                created_at TEXT NOT NULL
            ) STRICT;

            CREATE TABLE IF NOT EXISTS ticket_sessions(
                terminal_id TEXT PRIMARY KEY,
                ticket_id TEXT NOT NULL,
                cc_session_id TEXT,
                launch_kind TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_resumed_at TEXT,
                ended_at TEXT
            ) STRICT;
        ),
        sql!(ALTER TABLE ticket_worktrees ADD COLUMN status TEXT),
        sql!(ALTER TABLE ticket_worktrees ADD COLUMN ticket_type TEXT),
        sql!(ALTER TABLE ticket_worktrees ADD COLUMN issue_id TEXT),
        sql!(ALTER TABLE ticket_worktrees ADD COLUMN body_markdown TEXT),
        sql!(ALTER TABLE ticket_worktrees ADD COLUMN body_fetched_at TEXT),
    ];
}

db::static_connection!(TicketMetadataDb, []);

impl TicketMetadataDb {
    fn list_worktrees(&self) -> anyhow::Result<Vec<TicketWorktreeRow>> {
        self.select::<TicketWorktreeRow>(
            "SELECT ticket_id, title, url, worktree_path, branch_name, base_repo_root, \
            remote_connection, created_at, status, ticket_type, issue_id, body_markdown, \
            body_fetched_at \
            FROM ticket_worktrees \
            ORDER BY created_at DESC",
        )?()
    }

    fn list_sessions(&self) -> anyhow::Result<Vec<(TicketId, TicketSessionRecord)>> {
        self.select::<(TicketId, TicketSessionRecord)>(
            "SELECT ticket_id, terminal_id, cc_session_id, launch_kind, created_at, \
            last_resumed_at, ended_at \
            FROM ticket_sessions \
            ORDER BY created_at DESC",
        )?()
    }

    async fn save_worktree(&self, row: TicketWorktreeRow) -> anyhow::Result<()> {
        let ticket_id = row.ticket_id.0.to_string();
        let title = row.title.to_string();
        let url = row.url.to_string();
        let worktree_path = row
            .worktree_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let branch_name = row.branch_name.clone();
        let base_repo_root = row
            .base_repo_root
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let remote_connection = row
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize ticket remote connection")?;
        let created_at = row.created_at.to_rfc3339();
        let status = row.status.as_ref().map(ToString::to_string);
        let ticket_type = row.ticket_type.as_ref().map(ToString::to_string);
        let issue_id = row.issue_id.as_ref().map(ToString::to_string);
        let body_markdown = row.body_markdown.clone();
        let body_fetched_at = row.body_fetched_at.map(|time| time.to_rfc3339());

        self.write(move |conn| {
            let sql = "INSERT INTO ticket_worktrees(ticket_id, title, url, worktree_path, branch_name, base_repo_root, remote_connection, created_at, status, ticket_type, issue_id, body_markdown, body_fetched_at) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                       ON CONFLICT(ticket_id) DO UPDATE SET \
                           title = excluded.title, \
                           url = excluded.url, \
                           worktree_path = excluded.worktree_path, \
                           branch_name = excluded.branch_name, \
                           base_repo_root = excluded.base_repo_root, \
                           remote_connection = excluded.remote_connection, \
                           created_at = excluded.created_at, \
                           status = excluded.status, \
                           ticket_type = excluded.ticket_type, \
                           issue_id = excluded.issue_id, \
                           body_markdown = excluded.body_markdown, \
                           body_fetched_at = excluded.body_fetched_at";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&ticket_id, 1)?;
            i = stmt.bind(&title, i)?;
            i = stmt.bind(&url, i)?;
            i = stmt.bind(&worktree_path, i)?;
            i = stmt.bind(&branch_name, i)?;
            i = stmt.bind(&base_repo_root, i)?;
            i = stmt.bind(&remote_connection, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&status, i)?;
            i = stmt.bind(&ticket_type, i)?;
            i = stmt.bind(&issue_id, i)?;
            i = stmt.bind(&body_markdown, i)?;
            stmt.bind(&body_fetched_at, i)?;
            stmt.exec()
        })
        .await
    }

    async fn save_session(
        &self,
        ticket_id: TicketId,
        session: TicketSessionRecord,
    ) -> anyhow::Result<()> {
        let terminal_id = session.terminal_id.to_key_string();
        let ticket_id = ticket_id.0.to_string();
        let cc_session_id = session.cc_session_id.clone();
        let launch_kind = session.launch_kind.as_str().to_string();
        let created_at = session.created_at.to_rfc3339();
        let last_resumed_at = session.last_resumed_at.map(|time| time.to_rfc3339());
        let ended_at = session.ended_at.map(|time| time.to_rfc3339());

        self.write(move |conn| {
            let sql = "INSERT INTO ticket_sessions(terminal_id, ticket_id, cc_session_id, launch_kind, created_at, last_resumed_at, ended_at) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                       ON CONFLICT(terminal_id) DO UPDATE SET \
                           ticket_id = excluded.ticket_id, \
                           cc_session_id = excluded.cc_session_id, \
                           launch_kind = excluded.launch_kind, \
                           created_at = excluded.created_at, \
                           last_resumed_at = excluded.last_resumed_at, \
                           ended_at = excluded.ended_at";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&terminal_id, 1)?;
            i = stmt.bind(&ticket_id, i)?;
            i = stmt.bind(&cc_session_id, i)?;
            i = stmt.bind(&launch_kind, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&last_resumed_at, i)?;
            stmt.bind(&ended_at, i)?;
            stmt.exec()
        })
        .await
    }

    async fn delete_worktree(&self, ticket_id: TicketId) -> anyhow::Result<()> {
        let ticket_id = ticket_id.0.to_string();
        self.write(move |conn| {
            let mut stmt =
                Statement::prepare(conn, "DELETE FROM ticket_worktrees WHERE ticket_id = ?")?;
            stmt.bind(&ticket_id, 1)?;
            stmt.exec()?;
            let mut stmt =
                Statement::prepare(conn, "DELETE FROM ticket_sessions WHERE ticket_id = ?")?;
            stmt.bind(&ticket_id, 1)?;
            stmt.exec()
        })
        .await
    }
}

impl Column for TicketWorktreeRow {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (ticket_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (url, next): (String, i32) = Column::column(statement, next)?;
        let (worktree_path, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (branch_name, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (base_repo_root, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (status, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (ticket_type, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (issue_id, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (body_markdown, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (body_fetched_at, next): (Option<String>, i32) = Column::column(statement, next)?;

        let remote_connection = remote_connection_json
            .as_deref()
            .map(serde_json::from_str::<RemoteConnectionOptions>)
            .transpose()
            .context("deserialize ticket remote connection")?;

        Ok((
            TicketWorktreeRow {
                ticket_id: TicketId(SharedString::from(ticket_id)),
                title: SharedString::from(title),
                url: SharedString::from(url),
                worktree_path: worktree_path.map(PathBuf::from),
                branch_name,
                base_repo_root: base_repo_root.map(PathBuf::from),
                remote_connection,
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                status: status.map(SharedString::from),
                ticket_type: ticket_type.map(SharedString::from),
                issue_id: issue_id.map(SharedString::from),
                body_markdown,
                body_fetched_at: body_fetched_at
                    .map(|time| DateTime::parse_from_rfc3339(&time))
                    .transpose()?
                    .map(|time| time.with_timezone(&Utc)),
            },
            next,
        ))
    }
}

impl Column for TicketId {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (ticket_id, next): (String, i32) = Column::column(statement, start_index)?;
        Ok((TicketId(SharedString::from(ticket_id)), next))
    }
}

impl Column for TicketSessionRecord {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (cc_session_id, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (launch_kind, next): (String, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (last_resumed_at, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (ended_at, next): (Option<String>, i32) = Column::column(statement, next)?;

        Ok((
            TicketSessionRecord {
                terminal_id: TerminalId::from_key_string(&terminal_id)?,
                cc_session_id,
                launch_kind: TicketLaunchKind::from_str(&launch_kind),
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                last_resumed_at: last_resumed_at
                    .map(|time| DateTime::parse_from_rfc3339(&time))
                    .transpose()?
                    .map(|time| time.with_timezone(&Utc)),
                ended_at: ended_at
                    .map(|time| DateTime::parse_from_rfc3339(&time))
                    .transpose()?
                    .map(|time| time.with_timezone(&Utc)),
            },
            next,
        ))
    }
}

/// Opens (or focuses, if already open in some window) a workspace at `path`
/// and waits for its project's initial scan to finish, mirroring the await
/// sequence `git_ui::worktree_service::open_worktree_workspace` uses before
/// touching panels. `workspace::open_paths` already implements reuse-across-
/// all-windows-else-open-new, so callers never need to check "is it open"
/// themselves.
async fn open_ticket_workspace(
    path: PathBuf,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    let OpenResult {
        window, workspace, ..
    } = cx
        .update(|cx| workspace::open_paths(&[path], app_state, OpenOptions::default(), cx))
        .await?;

    workspace
        .update(cx, |workspace, cx| {
            workspace.project().read(cx).wait_for_initial_scan(cx)
        })
        .await;

    Ok((window, workspace))
}

/// Runs `git gtr new <branch> --yes` in `repo_path`, which creates the
/// worktree and runs any `.gtrconfig` post-create hooks (secrets fetch,
/// dependency install, editor/AI config sync, etc.) before returning —
/// https://github.com/coderabbitai/git-worktree-runner. A failed hook exits
/// non-zero, so `gtr` itself is the single source of truth for whether the
/// worktree is usable — no separate rollback step is needed here.
///
/// `--yes` is `new`'s non-interactive mode: spawned from the UI there is no
/// terminal to answer a prompt on, so gtr must fail rather than block.
///
/// The resulting path is then looked up through `git gtr list` rather than read
/// out of `new`'s own `path` record: `new` derives that record from the shell's
/// working directory, which under Git for Windows is an MSYS path
/// (`/c/Users/…`) that no Windows API accepts, whereas `list` reports the paths
/// `git worktree list` does.
async fn run_gtr_new(repo_path: &Path, branch_name: &str) -> anyhow::Result<PathBuf> {
    let output = smol::process::Command::new("git")
        .args(["gtr", "new", branch_name, "--yes"])
        .current_dir(repo_path)
        .output()
        .await
        .context("failed to run `git gtr` — is git-worktree-runner installed and on PATH?")?;

    if !output.status.success() {
        anyhow::bail!(
            "git gtr new failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let list = gtr_list(repo_path).await?;
    worktree_path_for_branch(&list, branch_name)
        .with_context(|| format!("git gtr list did not report a worktree for branch {branch_name}"))
}

/// What a ticket's worktree still holds that removing it would destroy, so the
/// confirmation can say so before anything is deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeWorkStatus {
    pub dirty_files: usize,
    pub unpushed_commits: usize,
}

impl WorktreeWorkStatus {
    pub fn has_unsaved_work(&self) -> bool {
        self.dirty_files > 0 || self.unpushed_commits > 0
    }
}

/// Counts the uncommitted files and unpushed commits in `worktree_path`.
///
/// Every failure answers "nothing" rather than propagating: this only feeds a
/// confirmation prompt, and a worktree whose directory is already gone, or
/// whose branch has no upstream, must not block closing the ticket.
pub async fn worktree_work_status(worktree_path: &Path) -> WorktreeWorkStatus {
    let git = |args: &'static [&'static str]| async move {
        let output = smol::process::Command::new("git")
            .args(args)
            .current_dir(worktree_path)
            .output()
            .await
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };

    let dirty_files = git(&["status", "--porcelain"])
        .await
        .map(|output| output.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or_default();
    let unpushed_commits = git(&["rev-list", "--count", "@{upstream}..HEAD"])
        .await
        .and_then(|output| output.trim().parse().ok())
        .unwrap_or_default();

    WorktreeWorkStatus {
        dirty_files,
        unpushed_commits,
    }
}

/// Runs `git gtr rm <branch> --yes` in `repo_path`, the counterpart of the
/// `git gtr new` that cut the worktree — so gtr's own pre/post-remove hooks run
/// and its bookkeeping stays consistent.
///
/// `force` is what gtr needs to remove a worktree with uncommitted changes (and
/// to override a failing hook), so it is only passed once the user has
/// confirmed that second, louder prompt.
pub async fn run_gtr_rm(
    repo_path: &Path,
    branch_name: &str,
    delete_branch: bool,
    force: bool,
) -> anyhow::Result<()> {
    let mut args = vec!["gtr", "rm", branch_name, "--yes"];
    if delete_branch {
        args.push("--delete-branch");
    }
    if force {
        args.push("--force");
    }

    let output = smol::process::Command::new("git")
        .args(&args)
        .current_dir(repo_path)
        .output()
        .await
        .context("failed to run `git gtr` — is git-worktree-runner installed and on PATH?")?;

    if !output.status.success() {
        anyhow::bail!(
            "git gtr rm failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// The known tickets a board query did not return, split by what closing the
/// gap costs: dropping a row, or one page fetch to learn its real status.
#[derive(Debug, Default)]
pub struct MissingTickets {
    /// Tickets with no worktree and no sessions — nothing on disk depends on
    /// them, so they simply leave the list.
    pub droppable: Vec<TicketId>,
    /// Tickets that still have a worktree or sessions, paired with their Notion
    /// page url so their real status can be fetched.
    pub still_working: Vec<(TicketId, String)>,
}

/// A worktree as `git gtr list --porcelain` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct GtrWorktree {
    pub path: PathBuf,
    pub branch: String,
}

/// Every worktree `gtr` reports for `repo_path`, so a ticket can be attached
/// to one that already exists instead of cutting a new one. The repository's
/// main worktree is in there too: working straight in the checkout the
/// repository was registered from is a legitimate choice.
pub async fn existing_worktrees(repo_path: &Path) -> anyhow::Result<Vec<GtrWorktree>> {
    Ok(parse_gtr_worktrees(&gtr_list(repo_path).await?))
}

async fn gtr_list(repo_path: &Path) -> anyhow::Result<String> {
    let list = smol::process::Command::new("git")
        .args(["gtr", "list", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .await
        .context("failed to run `git gtr list` — is git-worktree-runner installed and on PATH?")?;

    if !list.status.success() {
        anyhow::bail!(
            "git gtr list failed: {}",
            String::from_utf8_lossy(&list.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&list.stdout).into_owned())
}

/// Parses `git gtr list --porcelain` output, whose records are
/// `<path>\t<branch>\t<hook_status>`.
fn parse_gtr_worktrees(list_output: &str) -> Vec<GtrWorktree> {
    list_output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let path = fields.next()?;
            let branch = fields.next()?;
            (!path.is_empty() && !branch.is_empty()).then(|| GtrWorktree {
                path: PathBuf::from(path),
                branch: branch.to_string(),
            })
        })
        .collect()
}

/// Picks a branch's worktree path out of `git gtr list --porcelain` output.
fn worktree_path_for_branch(list_output: &str, branch_name: &str) -> Option<PathBuf> {
    parse_gtr_worktrees(list_output)
        .into_iter()
        .find(|worktree| worktree.branch == branch_name)
        .map(|worktree| worktree.path)
}

/// The per-worktree mirror directory for a ticket's brief and attachments.
///
/// `@` mentions are whitespace-delimited **by Claude itself**, so no amount of
/// shell quoting rescues a path containing a space — and the canonical copy
/// lives under `paths::data_dir()`, whose Windows account-name component very
/// often has one. Mirroring into the worktree lets the launch command point at
/// the files with short, relative, space-free paths, which works because the
/// ticket terminal's working directory already *is* the worktree.
const WORKTREE_MIRROR_DIR: &str = ".zed-ticket";
const BRIEF_FILE_NAME: &str = "brief.md";

/// Ticket ids are Notion URL path segments, so they can carry separators and
/// other characters a file name cannot; used raw, one would escape the
/// tickets directory.
pub fn sanitize_ticket_id(ticket_id: &str) -> String {
    let sanitized: String = ticket_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() {
        "ticket".to_string()
    } else {
        sanitized
    }
}

/// The canonical, worktree-independent home for a ticket's brief and its
/// attachments. It deliberately outlives the worktree: the mirror is rebuilt
/// from here on every resume, so a `git clean` or a recut worktree never
/// strands the transcript's references.
pub fn ticket_data_dir(ticket_id: &TicketId) -> PathBuf {
    paths::data_dir()
        .join("tickets")
        .join(sanitize_ticket_id(&ticket_id.0))
}

/// Where pasted screenshots for a ticket are kept.
pub fn ticket_images_dir(ticket_id: &TicketId) -> PathBuf {
    ticket_data_dir(ticket_id).join("images")
}

/// What the launch modal produces: the reviewed brief plus the absolute paths
/// of the attachments it saved under [`ticket_images_dir`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TicketLaunchSpec {
    pub brief_markdown: String,
    pub attachments: Vec<PathBuf>,
}

/// Which kind of `claude` session the launch modal asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum TicketSessionStart {
    /// Mirror the brief into the worktree and start a fresh session pointed at
    /// it — what launching a ticket has always meant.
    Brief(TicketLaunchSpec),
    /// Run bare `claude --resume` in the worktree and let Claude's own picker
    /// pick the session to continue. Used to adopt work started outside Zed,
    /// so there is no brief and no prompt to send.
    ResumePicker,
}

/// What the launch command references: worktree-relative paths into
/// [`WORKTREE_MIRROR_DIR`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TicketLaunchFiles {
    pub brief_relative_path: String,
    pub attachment_relative_paths: Vec<String>,
}

/// What the agent panel spawns a ticket terminal with, once the brief (if any)
/// has been written out.
#[derive(Debug, Clone, PartialEq)]
pub enum TicketTerminalLaunch {
    Brief {
        launch_files: TicketLaunchFiles,
        launch_kind: TicketLaunchKind,
    },
    ResumePicker,
}

/// Writes the brief and attachments to their canonical location and mirrors
/// them into the worktree, returning the relative paths the launch command
/// should mention.
///
/// Idempotent, so it doubles as the self-heal step on resume.
///
/// The mention paths this returns are space-free *by construction*, which is
/// the whole point of the mirror: `@` mentions are split on whitespace by
/// Claude itself, and a mention whose path contains a space loads nothing at
/// all — with no error and no warning (verified against the real CLI with
/// `--disallowed-tools Read Glob Grep Bash`, which is required to observe it:
/// with tools enabled Claude just reads the file and the breakage is masked).
/// Every component of the returned paths is chosen here — the literal
/// `.zed-ticket`, the literal `brief.md`, and file names run through
/// [`mentionable_file_name`] — so the only place a space *could* come from is
/// the worktree path, and the worktree path never appears in a mention.
async fn materialize_brief(
    fs: &Arc<dyn Fs>,
    ticket_id: &TicketId,
    worktree_path: &Path,
    spec: &TicketLaunchSpec,
) -> anyhow::Result<TicketLaunchFiles> {
    let mirror_dir = worktree_path.join(WORKTREE_MIRROR_DIR);
    fs.create_dir(&mirror_dir)
        .await
        .with_context(|| format!("failed to create {}", mirror_dir.display()))?;

    let mut attachment_relative_paths = Vec::with_capacity(spec.attachments.len());
    let mut read_with_tool_paths = Vec::new();
    for attachment in &spec.attachments {
        let file_name = attachment
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("attachment {} has no file name", attachment.display()))?;
        let file_name = mentionable_file_name(file_name);
        fs.copy_file(
            attachment,
            &mirror_dir.join(&file_name),
            fs::CopyOptions {
                overwrite: true,
                ignore_if_exists: false,
            },
        )
        .await
        .with_context(|| format!("failed to mirror attachment {}", attachment.display()))?;

        let relative_path = format!("{WORKTREE_MIRROR_DIR}/{file_name}");
        // Belt and braces: `mentionable_file_name` cannot leave whitespace in,
        // but losing a pasted screenshot silently is the worst outcome here, so
        // anything unmentionable is handed to Claude as a Read instruction
        // rather than dropped.
        if relative_path.contains(char::is_whitespace) {
            read_with_tool_paths.push(relative_path);
        } else {
            attachment_relative_paths.push(relative_path);
        }
    }

    let brief_markdown = brief_with_read_fallback(&spec.brief_markdown, &read_with_tool_paths);

    let canonical_dir = ticket_data_dir(ticket_id);
    fs.create_dir(&canonical_dir)
        .await
        .with_context(|| format!("failed to create {}", canonical_dir.display()))?;
    fs.atomic_write(canonical_dir.join(BRIEF_FILE_NAME), brief_markdown.clone())
        .await
        .context("failed to write the ticket brief")?;
    fs.atomic_write(mirror_dir.join(BRIEF_FILE_NAME), brief_markdown)
        .await
        .context("failed to mirror the ticket brief into the worktree")?;

    Ok(TicketLaunchFiles {
        brief_relative_path: format!("{WORKTREE_MIRROR_DIR}/{BRIEF_FILE_NAME}"),
        attachment_relative_paths,
    })
}

/// Makes a file name safe to appear inside an `@` mention: whitespace ends the
/// mention, so it is folded away rather than escaped (no escaping mechanism
/// exists on Claude's side).
fn mentionable_file_name(file_name: &str) -> String {
    let collapsed = file_name
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                '-'
            } else {
                character
            }
        })
        .collect::<String>();
    if collapsed.is_empty() {
        "attachment".to_string()
    } else {
        collapsed
    }
}

/// Appends the paths that could not be `@`-mentioned to the brief, so Claude
/// opens them with its Read tool instead of never seeing them.
fn brief_with_read_fallback(brief_markdown: &str, read_with_tool_paths: &[String]) -> String {
    if read_with_tool_paths.is_empty() {
        return brief_markdown.to_string();
    }
    let mut brief = brief_markdown.to_string();
    if !brief.ends_with('\n') {
        brief.push('\n');
    }
    brief.push_str("\n## Attachments to open with the Read tool\n");
    for path in read_with_tool_paths {
        brief.push_str(&format!("- {path}\n"));
    }
    brief
}

/// Hides the mirror from git via `info/exclude` in the *common* git dir,
/// which every linked worktree of the repository shares — so one write covers
/// them all — rather than the tracked `.gitignore`, which belongs to the user.
async fn exclude_mirror_from_git(fs: &Arc<dyn Fs>, worktree_path: &Path) -> anyhow::Result<()> {
    let output = smol::process::Command::new("git")
        .args(["-C"])
        .arg(worktree_path)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .await
        .context("failed to locate the repository's common git directory")?;
    anyhow::ensure!(
        output.status.success(),
        "git rev-parse --git-common-dir failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let common_dir = PathBuf::from(
        String::from_utf8(output.stdout)
            .context("git rev-parse returned non-UTF-8 output")?
            .trim(),
    );
    let info_dir = common_dir.join("info");
    fs.create_dir(&info_dir)
        .await
        .with_context(|| format!("failed to create {}", info_dir.display()))?;

    let exclude_path = info_dir.join("exclude");
    let existing = fs.load(&exclude_path).await.unwrap_or_default();
    let entry = format!("{WORKTREE_MIRROR_DIR}/");
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&entry);
    updated.push('\n');
    fs.atomic_write(exclude_path.clone(), updated)
        .await
        .with_context(|| format!("failed to update {}", exclude_path.display()))
}

/// Rebuilds a [`TicketLaunchSpec`] from the canonical copy on disk, for
/// resuming a ticket whose brief was written by an earlier launch.
async fn launch_spec_from_canonical(
    fs: &Arc<dyn Fs>,
    ticket_id: &TicketId,
) -> anyhow::Result<TicketLaunchSpec> {
    let brief_markdown = fs
        .load(&ticket_data_dir(ticket_id).join(BRIEF_FILE_NAME))
        .await
        .context("this ticket has no stored brief to restore")?;

    let images_dir = ticket_images_dir(ticket_id);
    let mut attachments = Vec::new();
    if let Ok(mut entries) = fs.read_dir(&images_dir).await {
        while let Some(entry) = futures::StreamExt::next(&mut entries).await {
            attachments.push(entry?);
        }
    }
    attachments.sort();

    Ok(TicketLaunchSpec {
        brief_markdown,
        attachments,
    })
}

/// Spawns a `claude` CLI session for a ticket in the given worktree, via the
/// workspace's `AgentPanel`.
async fn launch_ticket_session(
    ticket_id: TicketId,
    worktree_path: PathBuf,
    launch: TicketTerminalLaunch,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let (window, workspace) = open_ticket_workspace(worktree_path, app_state, cx).await?;
    let agent_panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
        .context("agent panel not available in this workspace")?;

    window.update(cx, |_multi_workspace, window, cx| {
        agent_panel.update(cx, |panel, cx| {
            panel.spawn_ticket_terminal(ticket_id, launch, window, cx)
        })
    })?
}

/// Turns the modal's choice into what the agent panel needs: a brief-driven
/// start has to be materialized into the worktree first, a resume has nothing
/// to write. `launch_kind` only describes the brief case; a resume is always
/// recorded as [`TicketLaunchKind::Resumed`].
async fn terminal_launch_for(
    start: TicketSessionStart,
    ticket_id: &TicketId,
    worktree_path: &Path,
    launch_kind: TicketLaunchKind,
    fs: &Arc<dyn Fs>,
) -> anyhow::Result<TicketTerminalLaunch> {
    Ok(match start {
        TicketSessionStart::Brief(spec) => TicketTerminalLaunch::Brief {
            launch_files: materialize_brief(fs, ticket_id, worktree_path, &spec).await?,
            launch_kind,
        },
        TicketSessionStart::ResumePicker => TicketTerminalLaunch::ResumePicker,
    })
}

/// Creates a git worktree for a ticket via `git gtr new` (running any
/// `.gtrconfig` post-create hooks) in `repo_path`, persists the result into
/// `TicketMetadataStore`, writes out the ticket's brief, then launches the
/// ticket's initial Claude Code session in the worktree. `ticket_id` must
/// already have an entry in `TicketMetadataStore` (from a prior
/// `upsert_ticket_ref` sync).
pub async fn create_worktree_and_launch(
    ticket_id: TicketId,
    repo_path: PathBuf,
    branch_name: String,
    spec: TicketLaunchSpec,
    fs: Arc<dyn Fs>,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let worktree_path = run_gtr_new(&repo_path, &branch_name).await?;
    // Hiding the mirror from git is a courtesy, not a precondition: a
    // repository layout that defeats it must not stop the session launching.
    exclude_mirror_from_git(&fs, &worktree_path).await.log_err();

    let ticket_store = cx.update(|cx| TicketMetadataStore::global(cx));
    ticket_store.update(cx, |store, cx| {
        store.save_worktree(
            &ticket_id,
            worktree_path.clone(),
            branch_name,
            repo_path,
            None,
            cx,
        )
    })?;

    let launch = TicketTerminalLaunch::Brief {
        launch_files: materialize_brief(&fs, &ticket_id, &worktree_path, &spec).await?,
        launch_kind: TicketLaunchKind::Initial,
    };

    launch_ticket_session(ticket_id, worktree_path, launch, app_state, cx).await
}

/// Attaches a ticket to a worktree that already exists — no `git gtr new`, no
/// new branch — and starts a session in it: either a fresh brief-driven one or
/// a `claude --resume` handing the choice of session to Claude's own picker.
///
/// The worktree is recorded exactly as a freshly cut one is, so the ticket
/// becomes resumable through [`open_ticket`] and its sessions are counted in
/// the sidebar.
pub async fn attach_worktree_and_launch(
    ticket_id: TicketId,
    repo_path: PathBuf,
    worktree_path: PathBuf,
    branch_name: String,
    start: TicketSessionStart,
    fs: Arc<dyn Fs>,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    // Same courtesy as on a fresh worktree, and idempotent: a repository
    // layout that defeats it must not stop the session launching.
    exclude_mirror_from_git(&fs, &worktree_path).await.log_err();

    let ticket_store = cx.update(|cx| TicketMetadataStore::global(cx));
    let launch_kind = ticket_store.read_with(cx, |store, _cx| match store.entry(&ticket_id) {
        Some(entry) if !entry.sessions.is_empty() => TicketLaunchKind::Additional,
        _ => TicketLaunchKind::Initial,
    });
    ticket_store.update(cx, |store, cx| {
        store.save_worktree(
            &ticket_id,
            worktree_path.clone(),
            branch_name,
            repo_path,
            None,
            cx,
        )
    })?;

    let launch = terminal_launch_for(start, &ticket_id, &worktree_path, launch_kind, &fs).await?;

    launch_ticket_session(ticket_id, worktree_path, launch, app_state, cx).await
}

/// Opens a ticket whose worktree already exists: focuses/opens the
/// worktree's workspace and resumes its most recently active session via
/// `claude --resume <id>` (built by `AgentPanel::restore_terminal`'s
/// resume-command logic — this call site does not construct the command
/// itself).
///
/// The resumed transcript still refers to `.zed-ticket/brief.md`, so the
/// mirror is rebuilt from the canonical copy first; a `git clean` between
/// sessions would otherwise leave those references dangling.
pub async fn open_ticket(
    ticket_id: TicketId,
    fs: Arc<dyn Fs>,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let store = cx.update(|cx| TicketMetadataStore::global(cx));
    let (worktree_path, most_recent_terminal_id) = store.read_with(cx, |store, _cx| {
        let entry = store
            .entry(&ticket_id)
            .context("ticket is not tracked by TicketMetadataStore")?;
        let worktree_path = entry
            .worktree_path
            .clone()
            .context("ticket has no worktree yet")?;
        let terminal_id = entry
            .most_recent_session()
            .context("ticket has a worktree but no recorded session")?
            .terminal_id;
        anyhow::Ok((worktree_path, terminal_id))
    })?;

    if let Ok(spec) = launch_spec_from_canonical(&fs, &ticket_id).await {
        materialize_brief(&fs, &ticket_id, &worktree_path, &spec).await?;
    }

    let (window, workspace) = open_ticket_workspace(worktree_path, app_state, cx).await?;
    let agent_panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
        .context("agent panel not available in this workspace")?;

    let terminal_metadata = cx
        .update(|cx| TerminalThreadMetadataStore::global(cx))
        .read_with(cx, |store, _cx| {
            store.entry(most_recent_terminal_id).cloned()
        })
        .context("no persisted metadata for the ticket's most recent session")?;

    window.update(cx, |_multi_workspace, window, cx| {
        agent_panel.update(cx, |panel, cx| {
            panel.restore_terminal(
                terminal_metadata,
                true,
                AgentThreadSource::TicketPanel,
                None,
                window,
                cx,
            )
        })
    })?;
    Ok(())
}

/// Launches one more independent Claude Code session for a ticket that already
/// has a worktree (and possibly other sessions running) — brief-driven, or a
/// `claude --resume` adopting a session started outside Zed.
pub async fn launch_additional_session(
    ticket_id: TicketId,
    start: TicketSessionStart,
    fs: Arc<dyn Fs>,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let worktree_path = cx
        .update(|cx| TicketMetadataStore::global(cx))
        .read_with(cx, |store, _cx| {
            store
                .entry(&ticket_id)
                .and_then(|entry| entry.worktree_path.clone())
        })
        .context("ticket has no worktree yet")?;

    let launch = terminal_launch_for(
        start,
        &ticket_id,
        &worktree_path,
        TicketLaunchKind::Additional,
        &fs,
    )
    .await?;

    launch_ticket_session(ticket_id, worktree_path, launch, app_state, cx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            TicketMetadataStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    #[test]
    fn test_sanitize_ticket_id_cannot_escape_the_tickets_directory() {
        assert_eq!(
            sanitize_ticket_id("Fix-chart-0123abcd"),
            "Fix-chart-0123abcd"
        );
        assert_eq!(sanitize_ticket_id("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_ticket_id("///"), "ticket");
    }

    #[test]
    fn test_mentionable_file_name_folds_whitespace_away() {
        // `@` mentions are whitespace-delimited with no escaping, so a name
        // like this must not reach a mention intact.
        assert_eq!(
            mentionable_file_name("Screenshot 2026-08-21 101400.png"),
            "Screenshot-2026-08-21-101400.png"
        );
        assert_eq!(mentionable_file_name("img-1.png"), "img-1.png");
        assert_eq!(mentionable_file_name(""), "attachment");
    }

    #[test]
    fn test_brief_gains_a_read_fallback_for_unmentionable_paths() {
        assert_eq!(brief_with_read_fallback("# Ticket\n", &[]), "# Ticket\n");
        assert_eq!(
            brief_with_read_fallback("# Ticket", &[".zed-ticket/a b.png".to_string()]),
            "# Ticket\n\n## Attachments to open with the Read tool\n- .zed-ticket/a b.png\n"
        );
    }

    #[gpui::test]
    async fn test_materialize_brief_produces_space_free_mentions(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let ticket_id = TicketId::new("CT-1487");
        let worktree_path = PathBuf::from(util::path!("/repos/my repo-worktrees/fix"));
        let source_dir = PathBuf::from(util::path!("/downloads"));
        fs.create_dir(&source_dir).await.unwrap();
        fs.write(&source_dir.join("a shot.png"), b"pixels")
            .await
            .unwrap();

        let spec = TicketLaunchSpec {
            brief_markdown: "# Ticket: spaces everywhere\n".to_string(),
            attachments: vec![source_dir.join("a shot.png")],
        };
        let fs: Arc<dyn Fs> = fs;
        let files = materialize_brief(&fs, &ticket_id, &worktree_path, &spec)
            .await
            .expect("materializing should succeed");

        // The worktree path has a space in it; the mention must not.
        assert_eq!(
            files.attachment_relative_paths,
            vec![".zed-ticket/a-shot.png".to_string()]
        );
        assert!(!files.brief_relative_path.contains(char::is_whitespace));
        assert_eq!(
            fs.load_bytes(&worktree_path.join(".zed-ticket").join("a-shot.png"))
                .await
                .unwrap(),
            b"pixels"
        );
    }

    #[gpui::test]
    async fn test_materialize_brief_mirrors_into_the_worktree(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let ticket_id = TicketId::new("Fix-the-chart-0123");
        let worktree_path = PathBuf::from(util::path!("/repos/inox-worktrees/fix-the-chart"));
        let images_dir = ticket_images_dir(&ticket_id);
        fs.create_dir(&images_dir).await.unwrap();
        fs.write(&images_dir.join("img-1.png"), b"first")
            .await
            .unwrap();
        fs.write(&images_dir.join("img-2.png"), b"second")
            .await
            .unwrap();

        let spec = TicketLaunchSpec {
            brief_markdown: "# Ticket: Fix the chart\n".to_string(),
            attachments: vec![images_dir.join("img-1.png"), images_dir.join("img-2.png")],
        };

        let fs: Arc<dyn Fs> = fs;
        let files = materialize_brief(&fs, &ticket_id, &worktree_path, &spec)
            .await
            .expect("materializing the brief should succeed");

        assert_eq!(files.brief_relative_path, ".zed-ticket/brief.md");
        assert_eq!(
            files.attachment_relative_paths,
            vec![
                ".zed-ticket/img-1.png".to_string(),
                ".zed-ticket/img-2.png".to_string()
            ]
        );

        let mirror = worktree_path.join(".zed-ticket");
        assert_eq!(
            fs.load(&mirror.join("brief.md")).await.unwrap(),
            spec.brief_markdown
        );
        assert_eq!(
            fs.load_bytes(&mirror.join("img-1.png")).await.unwrap(),
            b"first"
        );
        assert_eq!(
            fs.load_bytes(&mirror.join("img-2.png")).await.unwrap(),
            b"second"
        );

        // The canonical copy survives the worktree, so a resume can rebuild
        // the mirror from it.
        let canonical = ticket_data_dir(&ticket_id).join("brief.md");
        assert_eq!(fs.load(&canonical).await.unwrap(), spec.brief_markdown);

        let restored = launch_spec_from_canonical(&fs, &ticket_id)
            .await
            .expect("the canonical copy should be readable back");
        assert_eq!(restored.brief_markdown, spec.brief_markdown);
        assert_eq!(restored.attachments, spec.attachments);
    }

    #[test]
    fn test_worktree_path_for_branch() {
        let list_output = concat!(
            "C:/Users/dev/repo\tmaster\tok\n",
            "C:/Users/dev/repo-worktrees/spider-fix\tspider-fix\tok\n",
        );

        assert_eq!(
            worktree_path_for_branch(list_output, "spider-fix"),
            Some(PathBuf::from("C:/Users/dev/repo-worktrees/spider-fix"))
        );
        assert_eq!(
            worktree_path_for_branch(list_output, "master"),
            Some(PathBuf::from("C:/Users/dev/repo"))
        );
        assert_eq!(worktree_path_for_branch(list_output, "spider"), None);
        assert_eq!(worktree_path_for_branch("", "spider-fix"), None);
    }

    #[test]
    fn test_parse_gtr_worktrees() {
        let list_output = concat!(
            "C:/Users/dev/repo\tmain\tok\n",
            "C:/Users/dev/repo-worktrees/feature-user-auth\tfeature/user-auth\thooks-failed\n",
            "\n",
        );

        assert_eq!(
            parse_gtr_worktrees(list_output),
            vec![
                GtrWorktree {
                    path: PathBuf::from("C:/Users/dev/repo"),
                    branch: "main".to_string(),
                },
                GtrWorktree {
                    path: PathBuf::from("C:/Users/dev/repo-worktrees/feature-user-auth"),
                    branch: "feature/user-auth".to_string(),
                },
            ]
        );
        assert_eq!(parse_gtr_worktrees(""), Vec::new());
        // A record `gtr` truncated to a bare path has no branch to attach to.
        assert_eq!(parse_gtr_worktrees("C:/Users/dev/repo\n"), Vec::new());
    }

    fn display_fields(title: &str, url: &str, status: Option<&str>) -> TicketDisplayFields {
        TicketDisplayFields {
            title: title.to_string().into(),
            url: url.to_string().into(),
            status: status.map(|status| status.to_string().into()),
            ticket_type: None,
            issue_id: None,
        }
    }

    #[test]
    fn test_status_rank() {
        assert_eq!(status_rank(Some(&"3 - In progress".into())), 3);
        assert_eq!(status_rank(Some(&"10 - Done".into())), 10);
        assert_eq!(status_rank(Some(&"Backlog".into())), u32::MAX);
        assert_eq!(status_rank(None), u32::MAX);
    }

    #[gpui::test]
    async fn test_upsert_and_save_worktree_round_trip(cx: &mut TestAppContext) {
        init_test(cx);

        let ticket_id = TicketId::new("notion-page-1");
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    ticket_id.clone(),
                    TicketDisplayFields {
                        title: "Fix invoice export".into(),
                        url: "https://notion.so/ticket-1".into(),
                        status: Some("3 - In progress".into()),
                        ticket_type: Some("Bug".into()),
                        issue_id: Some("CT-1487".into()),
                    },
                    cx,
                );
            });
        });

        cx.update(|cx| {
            TicketMetadataStore::global(cx)
                .update(cx, |store, cx| {
                    store.save_body(&ticket_id, "# Repro steps".to_string(), cx)
                })
                .expect("ticket should be present");
        });

        cx.update(|cx| {
            TicketMetadataStore::global(cx)
                .update(cx, |store, cx| {
                    store.save_worktree(
                        &ticket_id,
                        PathBuf::from("/worktrees/fix-invoice-export"),
                        "ticket/fix-invoice-export".to_string(),
                        PathBuf::from("/repo"),
                        None,
                        cx,
                    )
                })
                .unwrap();
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let entry = store.entry(&ticket_id).expect("ticket should be present");
            assert_eq!(entry.title.as_ref(), "Fix invoice export");
            assert_eq!(
                entry.worktree_path,
                Some(PathBuf::from("/worktrees/fix-invoice-export"))
            );
            assert_eq!(
                entry.branch_name.as_deref(),
                Some("ticket/fix-invoice-export")
            );
            assert_eq!(entry.status.as_deref(), Some("3 - In progress"));
            assert_eq!(entry.ticket_type.as_deref(), Some("Bug"));
            assert_eq!(entry.issue_id.as_deref(), Some("CT-1487"));
            assert_eq!(entry.body_markdown.as_deref(), Some("# Repro steps"));
            assert!(entry.body_fetched_at.is_some());
        });

        // A later board sync refreshes the display fields but must not drop
        // the lazily fetched page body.
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    ticket_id.clone(),
                    display_fields(
                        "Fix invoice export v2",
                        "https://notion.so/ticket-1",
                        Some("4 - Review"),
                    ),
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let entry = store.entry(&ticket_id).expect("ticket should be present");
            assert_eq!(entry.title.as_ref(), "Fix invoice export v2");
            assert_eq!(entry.status.as_deref(), Some("4 - Review"));
            assert_eq!(entry.ticket_type, None);
            assert_eq!(entry.body_markdown.as_deref(), Some("# Repro steps"));
            assert_eq!(
                entry.worktree_path,
                Some(PathBuf::from("/worktrees/fix-invoice-export"))
            );
        });

        // Reload from the database to confirm persistence, not just the
        // in-memory cache.
        let db = cx.update(|cx| TicketMetadataStore::global(cx).read(cx).db.clone());
        cx.run_until_parked();
        let rows = db.list_worktrees().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ticket_id, ticket_id);
        assert_eq!(rows[0].status.as_deref(), Some("4 - Review"));
        assert_eq!(rows[0].ticket_type, None);
        assert_eq!(rows[0].issue_id, None);
        assert_eq!(rows[0].body_markdown.as_deref(), Some("# Repro steps"));
        assert!(rows[0].body_fetched_at.is_some());
    }

    #[gpui::test]
    async fn test_entries_sorted(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    TicketId::new("page-c"),
                    display_fields("Zebra", "https://notion.so/c", Some("3 - In progress")),
                    cx,
                );
                store.upsert_ticket_ref(
                    TicketId::new("page-a"),
                    display_fields("Apple", "https://notion.so/a", Some("3 - In progress")),
                    cx,
                );
                store.upsert_ticket_ref(
                    TicketId::new("page-b"),
                    display_fields("Mango", "https://notion.so/b", Some("1 - Backlog")),
                    cx,
                );
                store.upsert_ticket_ref(
                    TicketId::new("page-d"),
                    display_fields("Anteater", "https://notion.so/d", Some("Icebox")),
                    cx,
                );
                store.upsert_ticket_ref(
                    TicketId::new("page-e"),
                    display_fields("Anteater", "https://notion.so/e", None),
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let titles: Vec<_> = store
                .entries_sorted()
                .iter()
                .map(|entry| entry.title.to_string())
                .collect();
            assert_eq!(
                titles,
                vec!["Mango", "Apple", "Zebra", "Anteater", "Anteater"]
            );

            let unranked: Vec<_> = store
                .entries_sorted()
                .iter()
                .skip(3)
                .map(|entry| entry.ticket_id.0.to_string())
                .collect();
            assert_eq!(unranked, vec!["page-d", "page-e"]);
        });
    }

    #[gpui::test]
    async fn test_worktree_work_status_answers_nothing_for_a_missing_worktree(
        cx: &mut TestAppContext,
    ) {
        let status = cx
            .background_executor
            .spawn(async {
                worktree_work_status(Path::new("/definitely-not-a-worktree-here")).await
            })
            .await;

        assert_eq!(status, WorktreeWorkStatus::default());
        assert!(
            !status.has_unsaved_work(),
            "a worktree that is already gone must not block closing its ticket"
        );
    }

    #[gpui::test]
    async fn test_tickets_missing_from_a_board_query_split_by_work_on_disk(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let untouched = TicketId::new("notion-page-untouched");
        let with_worktree = TicketId::new("notion-page-with-worktree");
        let still_returned = TicketId::new("notion-page-returned");
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                for (ticket_id, url) in [
                    (&untouched, "https://notion.so/untouched"),
                    (&with_worktree, "https://notion.so/with-worktree"),
                    (&still_returned, "https://notion.so/returned"),
                ] {
                    store.upsert_ticket_ref(
                        ticket_id.clone(),
                        display_fields("Ticket", url, Some("Waiting for customer")),
                        cx,
                    );
                }
                store
                    .save_worktree(
                        &with_worktree,
                        PathBuf::from("/repo-feature"),
                        "feature".to_string(),
                        PathBuf::from("/repo"),
                        None,
                        cx,
                    )
                    .unwrap();
            });
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let returned = HashSet::from_iter([still_returned.clone()]);
            let missing = store.read(cx).tickets_missing_from(&returned);

            assert_eq!(missing.droppable, vec![untouched.clone()]);
            assert_eq!(
                missing.still_working,
                vec![(
                    with_worktree.clone(),
                    "https://notion.so/with-worktree".to_string()
                )],
                "a ticket with a worktree must be kept and re-queried, not dropped"
            );

            store.update(cx, |store, cx| {
                for ticket_id in &missing.droppable {
                    store.forget_ticket(ticket_id, cx);
                }
            });
            let store = store.read(cx);
            assert!(store.entry(&untouched).is_none());
            assert!(store.entry(&with_worktree).is_some());
            assert!(store.entry(&still_returned).is_some());
        });
    }

    #[gpui::test]
    async fn test_add_session_and_delete_worktree_round_trip(cx: &mut TestAppContext) {
        init_test(cx);

        let ticket_id = TicketId::new("notion-page-2");
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    ticket_id.clone(),
                    display_fields("Add dark mode", "https://notion.so/ticket-2", None),
                    cx,
                );
            });
        });

        let terminal_id = TerminalId::new();
        cx.update(|cx| {
            TicketMetadataStore::global(cx)
                .update(cx, |store, cx| {
                    store.add_session(
                        &ticket_id,
                        TicketSessionRecord {
                            terminal_id,
                            cc_session_id: Some("session-uuid".to_string()),
                            launch_kind: TicketLaunchKind::Initial,
                            created_at: Utc::now(),
                            last_resumed_at: None,
                            ended_at: None,
                        },
                        cx,
                    )
                })
                .unwrap();
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let entry = store.entry(&ticket_id).expect("ticket should be present");
            assert_eq!(entry.sessions.len(), 1);
            assert_eq!(
                entry
                    .most_recent_session()
                    .unwrap()
                    .cc_session_id
                    .as_deref(),
                Some("session-uuid")
            );
            assert_eq!(entry.unclosed_session_count(), 1);
            assert_eq!(store.ticket_id_for_terminal(terminal_id), Some(&ticket_id));
            assert_eq!(
                store
                    .session_for_terminal(terminal_id)
                    .and_then(|session| session.cc_session_id.as_deref()),
                Some("session-uuid")
            );
            assert!(store.ticket_id_for_terminal(TerminalId::new()).is_none());
        });

        let resumed_at = Utc::now();
        let ended_at = resumed_at + chrono::Duration::seconds(30);
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.mark_session_resumed(terminal_id, resumed_at, cx);
                // Terminals that never belonged to a ticket must be ignored,
                // not reported as an error.
                store.mark_session_ended(TerminalId::new(), ended_at, cx);
            });
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let session = store
                .session_for_terminal(terminal_id)
                .expect("session should be indexed");
            assert_eq!(session.last_resumed_at, Some(resumed_at));
            assert_eq!(session.ended_at, None);
        });

        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.mark_session_ended(terminal_id, ended_at, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let session = store
                .session_for_terminal(terminal_id)
                .expect("session should be indexed");
            assert_eq!(session.ended_at, Some(ended_at));
            let entry = store.entry(&ticket_id).expect("ticket should be present");
            assert_eq!(entry.unclosed_session_count(), 0);
        });

        let db = cx.update(|cx| TicketMetadataStore::global(cx).read(cx).db.clone());
        let persisted = db.list_sessions().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].1.ended_at, Some(ended_at));
        assert_eq!(persisted[0].1.last_resumed_at, Some(resumed_at));

        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.delete_worktree(&ticket_id, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(store.entry(&ticket_id).is_none());
            assert!(store.ticket_id_for_terminal(terminal_id).is_none());
            assert!(store.session_for_terminal(terminal_id).is_none());
        });

        cx.run_until_parked();
        assert!(db.list_worktrees().unwrap().is_empty());
        assert!(db.list_sessions().unwrap().is_empty());
    }

    /// A session adopted through Claude's own picker has no session id to
    /// record, which the schema has to survive: `cc_session_id` is the column
    /// every other code path fills in.
    #[gpui::test]
    async fn test_resumed_session_round_trips_without_a_session_id(cx: &mut TestAppContext) {
        init_test(cx);

        let ticket_id = TicketId::new("notion-page-3");
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    ticket_id.clone(),
                    display_fields("Adopt a session", "https://notion.so/ticket-3", None),
                    cx,
                );
            });
        });

        let terminal_id = TerminalId::new();
        cx.update(|cx| {
            TicketMetadataStore::global(cx)
                .update(cx, |store, cx| {
                    store.add_session(
                        &ticket_id,
                        TicketSessionRecord {
                            terminal_id,
                            cc_session_id: None,
                            launch_kind: TicketLaunchKind::Resumed,
                            created_at: Utc::now(),
                            last_resumed_at: None,
                            ended_at: None,
                        },
                        cx,
                    )
                })
                .unwrap();
        });
        cx.run_until_parked();

        let db = cx.update(|cx| TicketMetadataStore::global(cx).read(cx).db.clone());
        let persisted = db.list_sessions().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].0, ticket_id);
        assert_eq!(persisted[0].1.cc_session_id, None);
        assert_eq!(persisted[0].1.launch_kind, TicketLaunchKind::Resumed);
    }
}
